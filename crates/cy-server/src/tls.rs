//! 控制通道的 TLS：自签证书的生成、持久化与指纹。
//!
//! 控制端口通常直接暴露在公网，但它不是给浏览器用的——只有我们自己的客户端会连。
//! 所以这里不走 CA 那一套，而是服务端首次启动自签一张证书、打印指纹，客户端按指纹
//! 校验（pin）。省掉「为了内网工具再申请一张证书」的麻烦，安全性等价于私有 CA。
//!
//! 指纹用的是**整张证书的 SHA-256**。理论上 SPKI 指纹更好（换证书不换密钥时 pin 不变），
//! 但我们的证书是长期自签、只生成一次，换证书必然连密钥一起换，两者没有区别——
//! 那就选不需要解析 X.509 的那个。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("读写证书文件失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("生成自签证书失败: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("TLS 配置有误: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("证书文件 {0} 解析失败或为空")]
    BadCert(PathBuf),
}

/// 服务端 TLS 身份：证书链、私钥，以及给客户端核对的指纹。
pub struct Identity {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    /// 整张证书的 SHA-256，十六进制小写。
    pub fingerprint: String,
}

impl Identity {
    /// 从数据目录加载自签证书；不存在就生成一张并存下来。
    ///
    /// 存下来很重要：每次重启都换证书的话，客户端 pin 的值就失效了。
    pub fn load_or_create(data_dir: &Path) -> Result<Self, TlsError> {
        let cert_path = cert_path(data_dir);
        let key_path = key_path(data_dir);

        if cert_path.exists() && key_path.exists() {
            return Self::from_pem_files(&cert_path, &key_path);
        }

        std::fs::create_dir_all(data_dir)?;
        let generated = generate_self_signed()?;
        std::fs::write(&cert_path, &generated.cert_pem)?;
        write_private(&key_path, generated.key_pem.as_bytes())?;

        Self::from_pem_files(&cert_path, &key_path)
    }

    /// 直接从 PEM 文件加载（直连模式下的正式证书也走这里）。
    pub fn from_pem_files(cert_path: &Path, key_path: &Path) -> Result<Self, TlsError> {
        use rustls::pki_types::pem::PemObject;

        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
            .map_err(|_| TlsError::BadCert(cert_path.to_path_buf()))?
            .collect::<Result<_, _>>()
            .map_err(|_| TlsError::BadCert(cert_path.to_path_buf()))?;
        if certs.is_empty() {
            return Err(TlsError::BadCert(cert_path.to_path_buf()));
        }

        let key = PrivateKeyDer::from_pem_file(key_path)
            .map_err(|_| TlsError::BadCert(key_path.to_path_buf()))?;

        let fingerprint = fingerprint_of(&certs[0]);
        Ok(Self {
            certs,
            key,
            fingerprint,
        })
    }

    /// 组装 rustls 服务端配置。
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, TlsError> {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(self.certs.clone(), self.key.clone_key())?;
        Ok(Arc::new(config))
    }
}

/// 自签证书与私钥在数据目录里的位置。
///
/// 单独暴露出来，是为了让管理命令能「只查不建」——`load_or_create` 在缺失时会
/// 生成一张，而管理命令常以 root 运行，生成出来的文件属主是 root，服务反而起不来。
pub fn cert_path(data_dir: &Path) -> PathBuf {
    data_dir.join("control-cert.pem")
}

pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("control-key.pem")
}

/// 证书指纹：SHA-256，十六进制小写。
pub fn fingerprint_of(cert: &CertificateDer<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert.as_ref());
    hex::encode(hasher.finalize())
}

/// 统一用 ring 作为加密后端。
///
/// rustls 0.23 默认是 aws-lc-rs，它的构建链在 musl 交叉编译时很麻烦，
/// 而服务端要出静态二进制。显式指定 provider 也顺带避开「进程级默认值
/// 装没装、谁先装」这类隐性顺序问题。
pub fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

struct Generated {
    cert_pem: String,
    key_pem: String,
}

fn generate_self_signed() -> Result<Generated, rcgen::Error> {
    // 证书里的名字对我们没有意义（客户端按指纹校验，不看名字），
    // 但总得填一个，填个能说明来历的。
    let mut params = rcgen::CertificateParams::new(vec!["chuanyun-control".to_string()])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "chuanyun control channel");
    // 有效期给得长：这张证书只在首次启动生成一次，中途过期会让所有客户端连不上。
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2100, 1, 1);

    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(Generated {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// 写私钥文件，权限收到仅属主可读。
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.fingerprint.len(), 64, "SHA-256 十六进制是 64 个字符");

        // 再次加载必须拿到同一张证书——否则客户端 pin 的值每次重启都失效
        let second = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn different_data_dirs_get_different_identities() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ia = Identity::load_or_create(a.path()).unwrap();
        let ib = Identity::load_or_create(b.path()).unwrap();
        assert_ne!(ia.fingerprint, ib.fingerprint);
    }

    #[test]
    fn builds_a_usable_server_config() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        id.server_config().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("control-key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "私钥不该让同机其他用户读到，实际权限 {mode:o}"
        );
    }
}
