//! 服务端证书校验（**安全敏感**）。
//!
//! 服务端用的是自签证书，走不了 CA 那一套，所以这里按指纹校验（pin）：
//! 只认指纹对得上的那一张，别的一律拒绝。
//!
//! # 改这个文件之前请读完
//!
//! 自定义证书校验最经典的翻车方式，是把 `verify_tls12_signature` /
//! `verify_tls13_signature` 写成"直接返回通过"。那两个方法验的是**对端是否真的
//! 持有证书里的私钥**——放行它们，等于任何人拿着一张公开的证书副本就能冒充服务端，
//! 指纹校验也就白做了。所以这里只自定义"认哪张证书"，验签一律交还给 rustls。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

/// 算一张证书的 SHA-256 指纹（十六进制小写）。
///
/// 和服务端 `tls::fingerprint_of` 必须算的是同一个东西，否则永远对不上。
pub fn fingerprint_of(cert: &CertificateDer<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert.as_ref());
    hex::encode(hasher.finalize())
}

/// 把用户可能粘贴进来的各种写法归一：去掉冒号、空格，转小写。
pub fn normalize_fingerprint(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 只认指定指纹的校验器。
#[derive(Debug)]
pub struct PinnedCertVerifier {
    expected: String,
    provider: Arc<CryptoProvider>,
}

impl PinnedCertVerifier {
    pub fn new(expected_fingerprint: &str, provider: Arc<CryptoProvider>) -> Self {
        Self {
            expected: normalize_fingerprint(expected_fingerprint),
            provider,
        }
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = fingerprint_of(end_entity);

        // 常量时间比较：指纹本身不是秘密，但按内容长短提前返回是个坏习惯，
        // 别让它成为以后复制粘贴的模板。
        if constant_time_eq(actual.as_bytes(), self.expected.as_bytes()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(format!(
                "服务端证书指纹不符：期望 {}，实际 {}。\
                 如果服务端刚重装过，请向管理员确认新指纹后更新设置。",
                short(&self.expected),
                short(&actual)
            )))
        }
    }

    // 下面两个方法验的是对端是否真的持有私钥，必须交给 rustls 实打实地验。
    // 详见文件顶部的说明——这里返回 `Ok` 就等于整套校验作废。
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 首次连接时把指纹交给用户确认（TOFU），确认后由调用方存下来。
///
/// 只用在开源发行版：公司内部版的指纹已经编译进安装包，用户不会看到这一步。
#[derive(Debug)]
pub struct TofuCertVerifier {
    seen: std::sync::Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl TofuCertVerifier {
    pub fn new(provider: Arc<CryptoProvider>) -> Self {
        Self {
            seen: std::sync::Mutex::new(None),
            provider,
        }
    }

    /// 握手中看到的证书指纹。拿它去问用户"要信任这台服务器吗"。
    pub fn observed(&self) -> Option<String> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl ServerCertVerifier for TofuCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint_of(end_entity));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn short(fingerprint: &str) -> String {
    fingerprint.chars().take(16).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_pasted_fingerprints() {
        // 用户可能从日志、邮件、聊天窗口里粘各种写法过来
        let canonical = "abcdef0123456789";
        assert_eq!(normalize_fingerprint("AB:CD:EF:01:23:45:67:89"), canonical);
        assert_eq!(normalize_fingerprint("ab cd ef 01 23 45 67 89"), canonical);
        assert_eq!(normalize_fingerprint("ABCDEF0123456789"), canonical);
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    /// 拿一张真实的自签证书来验：指纹对得上就放行，差一位就拒绝。
    #[test]
    fn accepts_only_the_pinned_certificate() {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["test".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        let der = CertificateDer::from(cert.der().to_vec());

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let expected = fingerprint_of(&der);

        let good = PinnedCertVerifier::new(&expected, provider.clone());
        assert!(verify(&good, &der).is_ok());

        // 换一个指纹（末位改掉）应当被拒
        let mut wrong = expected.clone();
        let last = wrong.pop().unwrap();
        wrong.push(if last == 'a' { 'b' } else { 'a' });
        let bad = PinnedCertVerifier::new(&wrong, provider.clone());
        assert!(verify(&bad, &der).is_err());

        // 另一张证书（不同密钥）也应当被拒
        let other_key = rcgen::KeyPair::generate().unwrap();
        let other = rcgen::CertificateParams::new(vec!["test".to_string()])
            .unwrap()
            .self_signed(&other_key)
            .unwrap();
        let other_der = CertificateDer::from(other.der().to_vec());
        assert!(verify(&good, &other_der).is_err());
    }

    #[test]
    fn pin_accepts_colon_separated_form() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["test".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let der = CertificateDer::from(cert.der().to_vec());

        // 管理员从别处拷来一个带冒号的指纹，也该能用
        let pretty = fingerprint_of(&der)
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_uppercase())
            .collect::<Vec<_>>()
            .join(":");

        let v =
            PinnedCertVerifier::new(&pretty, Arc::new(rustls::crypto::ring::default_provider()));
        assert!(verify(&v, &der).is_ok());
    }

    #[test]
    fn tofu_records_what_it_saw() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["test".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let der = CertificateDer::from(cert.der().to_vec());

        let v = TofuCertVerifier::new(Arc::new(rustls::crypto::ring::default_provider()));
        assert_eq!(v.observed(), None);
        verify(&v, &der).unwrap();
        assert_eq!(v.observed(), Some(fingerprint_of(&der)));
    }

    /// 跑一次真实的 TLS 握手：指纹对得上就连得通，对不上就连不通。
    ///
    /// 单测 `verify_server_cert` 只能证明"指纹比对写对了"，证明不了整套校验在真实
    /// 握手里成立——签名算法协商、验签委托这些都只有真握手才会走到。
    #[tokio::test]
    async fn real_handshake_honours_the_pin() {
        let (cert_der, server_config) = test_server_config();
        let good = fingerprint_of(&cert_der);
        let bad = {
            let (other, _) = test_server_config();
            fingerprint_of(&other)
        };

        assert!(
            handshake(server_config.clone(), &good).await.is_ok(),
            "指纹对得上却握手失败"
        );
        assert!(
            handshake(server_config, &bad).await.is_err(),
            "指纹对不上却握手成功了——校验没起作用"
        );
    }

    fn test_server_config() -> (CertificateDer<'static>, Arc<rustls::ServerConfig>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["chuanyun-test".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap();

        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .unwrap();

        (cert_der, Arc::new(config))
    }

    async fn handshake(
        server_config: Arc<rustls::ServerConfig>,
        pin: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(pin, provider)))
            .with_no_client_auth();

        let (client_io, server_io) = tokio::io::duplex(16 * 1024);

        let server = tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
            let _ = acceptor.accept(server_io).await;
        });

        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let name = rustls::pki_types::ServerName::try_from("chuanyun-test")?;
        let result = connector.connect(name, client_io).await;

        let _ = server.await;
        result.map(|_| ()).map_err(Into::into)
    }

    fn verify(
        v: &dyn ServerCertVerifier,
        der: &CertificateDer<'_>,
    ) -> Result<ServerCertVerified, TlsError> {
        v.verify_server_cert(
            der,
            &[],
            &ServerName::try_from("test").unwrap(),
            &[],
            UnixTime::now(),
        )
    }
}
