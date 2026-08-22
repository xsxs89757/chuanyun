//! 客户端的持久化状态：凭证、隧道列表、设置。
//!
//! 存在平台标准的配置目录里（macOS 的 Application Support、Windows 的 AppData）。
//! 重开应用要能恢复上次开着的隧道——用户不该每天早上重新配一遍。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::TunnelSpec;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// 服务器地址。留空表示用品牌配置里的默认值。
    pub server: String,
    /// 凭证。
    pub token: String,
    /// 服务端证书指纹（TOFU 确认过的，或用户手填的）。
    pub tls_pin: String,
    /// 隧道配置，按名字排序存放，好让文件 diff 稳定。
    pub tunnels: BTreeMap<String, TunnelEntry>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelEntry {
    pub local_port: u16,
    /// 上次退出时是不是开着的——重开应用时据此自动恢复
    pub enabled: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// 访问口令（`用户名:口令`）。
    ///
    /// 必须存下来：断线重连和重开应用都要拿它重新开隧道，不存的话隧道回来了
    /// 但门没了——那比一开始就没设口令更糟，因为用户以为它还锁着。
    /// 状态文件是 0600，并且已经存着登录凭证，多存这一项不改变风险面。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// 自定义域名（需管理员先登记给本人）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_domain: Option<String>,
}

fn spec_of(name: &str, e: &TunnelEntry) -> TunnelSpec {
    TunnelSpec {
        name: name.to_string(),
        local_port: e.local_port,
        kind: cy_proto::TunnelKind::Http,
        auth: e.auth.clone(),
        custom_domain: e.custom_domain.clone(),
    }
}

impl Default for TunnelEntry {
    fn default() -> Self {
        Self {
            local_port: 0,
            enabled: true,
            auth: None,
            custom_domain: None,
            note: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub autostart: bool,
    /// 本地 API 端口。固定值，不做「被占就换一个」——
    /// 脚本里写死 7075 却发现应用漂到了 7076，比直接报错难查得多。
    pub local_api_port: u16,
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: false,
            local_api_port: crate::local_api::DEFAULT_PORT,
            check_updates: true,
        }
    }
}

impl State {
    /// 状态文件的默认位置。
    pub fn default_path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("cn", "chuanyun", "chuanyun")?;
        Some(dirs.config_dir().join("state.json"))
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                // 配置文件坏了不该让应用起不来——退回默认值，用户重新登录即可。
                // 旁边留一份原件，万一是我们解析写错了还能捞回来。
                tracing::warn!(error = %e, "状态文件解析失败，已备份为 state.json.bak 并重置");
                let _ = std::fs::rename(path, path.with_extension("json.bak"));
                State::default()
            }),
            Err(_) => State::default(),
        }
    }

    /// 原子写入：先写临时文件再改名。
    ///
    /// 直接覆写的话，写到一半断电就会留下半个 JSON，下次启动全部配置丢失。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, path)
    }

    /// 上次开着的那些隧道。
    pub fn enabled_tunnels(&self) -> Vec<TunnelSpec> {
        self.tunnels
            .iter()
            .filter(|(_, e)| e.enabled)
            .map(|(name, e)| spec_of(name, e))
            .collect()
    }

    /// 按名字取回完整的隧道定义。
    ///
    /// 重开隧道（重连、重新打开开关）一律走这里，别在调用处现拼
    /// `TunnelSpec::http(name, port)`——那样每加一个字段都会漏掉一处，
    /// 访问口令和自定义域名就是这么丢的。
    pub fn spec(&self, name: &str) -> Option<TunnelSpec> {
        self.tunnels.get(name).map(|e| spec_of(name, e))
    }

    /// 记下一条隧道。整个 spec 进来，免得新增字段时漏存。
    ///
    /// 返回实际生效的 spec——口令和自定义域名可能沿用了已有的那份。
    ///
    /// **同名隧道已存在、而这次没给口令，就保留原来的口令。** 项目脚本（base 的
    /// dev.sh、vite 插件）每次启动都按「名字 + 端口」重新注册一遍，它们不知道也
    /// 不该知道口令——那是隧道主人在客户端里设的。如果注册一次就把口令抹掉，
    /// 同事设好的门会被自己的启动脚本静默拆掉，还没有任何提示。
    /// 要真的去掉口令，删掉隧道重建。
    pub fn upsert_tunnel(&mut self, spec: &TunnelSpec, enabled: bool) -> TunnelSpec {
        let entry = self.tunnels.entry(spec.name.clone()).or_default();
        entry.local_port = spec.local_port;
        entry.enabled = enabled;
        if spec.auth.is_some() {
            entry.auth = spec.auth.clone();
        }
        if spec.custom_domain.is_some() {
            entry.custom_domain = spec.custom_domain.clone();
        }
        spec_of(&spec.name, entry)
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if let Some(e) = self.tunnels.get_mut(name) {
            e.enabled = enabled;
        }
    }

    pub fn remove_tunnel(&mut self, name: &str) {
        self.tunnels.remove(name);
    }
}

/// 状态文件里有凭证，别让同机其他用户读到。
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
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let mut state = State {
            server: "tunnel.example.com:7000".into(),
            token: "cy_zhangsan_abc".into(),
            ..Default::default()
        };
        state.upsert_tunnel(&TunnelSpec::http("wx", 8082), true);
        state.upsert_tunnel(&TunnelSpec::http("admin", 5173), false);
        state.save(&path).unwrap();

        let loaded = State::load(&path);
        assert_eq!(loaded.token, "cy_zhangsan_abc");
        assert_eq!(loaded.tunnels.len(), 2);
        // 只恢复上次开着的那条
        let enabled = loaded.enabled_tunnels();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "wx");
        assert_eq!(enabled[0].local_port, 8082);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::load(&dir.path().join("nope.json"));
        assert!(state.token.is_empty());
        assert_eq!(
            state.settings.local_api_port,
            crate::local_api::DEFAULT_PORT
        );
    }

    #[test]
    fn corrupt_file_is_backed_up_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        // 配置坏了不该让应用起不来
        let state = State::load(&path);
        assert!(state.token.is_empty());
        assert!(
            path.with_extension("json.bak").exists(),
            "原件应当备份下来，万一是我们解析写错了还能捞回"
        );
    }

    /// 访问口令必须跟着状态一起存下来。它曾经根本没进 TunnelEntry，
    /// 于是断线重连、重新打开开关、重开应用之后隧道回来了但门没了——
    /// 比一开始就没设口令更糟，因为用户以为它还锁着。
    #[test]
    fn access_password_and_custom_domain_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let mut state = State::default();
        state.upsert_tunnel(
            &TunnelSpec::http("wx", 8082)
                .with_auth("demo:s3cret")
                .with_domain("pay.example.com"),
            true,
        );
        state.save(&path).unwrap();

        let loaded = State::load(&path);
        let spec = loaded.spec("wx").expect("隧道应该还在");
        assert_eq!(spec.auth.as_deref(), Some("demo:s3cret"), "口令要存下来");
        assert_eq!(
            spec.custom_domain.as_deref(),
            Some("pay.example.com"),
            "自定义域名要存下来"
        );

        // 重连时全量重开走的是这条路，它也得带着口令
        let reopened = loaded.enabled_tunnels();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0].auth.as_deref(), Some("demo:s3cret"));
    }

    /// 脚本每次启动都按「名字 + 端口」重新注册——不带口令的注册不能把已有口令抹掉。
    #[test]
    fn re_registering_without_a_password_keeps_the_existing_one() {
        let mut state = State::default();
        state.upsert_tunnel(
            &TunnelSpec::http("api", 8082).with_auth("demo:s3cret"),
            true,
        );

        // dev.sh 那种写法：只给名字和端口
        let effective = state.upsert_tunnel(&TunnelSpec::http("api", 8090), true);
        assert_eq!(effective.local_port, 8090, "端口要更新");
        assert_eq!(effective.auth.as_deref(), Some("demo:s3cret"), "口令要保留");
        assert_eq!(
            state.spec("api").unwrap().auth.as_deref(),
            Some("demo:s3cret")
        );

        // 明确给了新口令就换
        state.upsert_tunnel(&TunnelSpec::http("api", 8090).with_auth("demo:new"), true);
        assert_eq!(state.spec("api").unwrap().auth.as_deref(), Some("demo:new"));
    }

    /// 没设口令的隧道不该在状态文件里留下 auth 这个键。
    #[test]
    fn a_tunnel_without_a_password_writes_no_auth_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::default();
        state.upsert_tunnel(&TunnelSpec::http("wx", 8082), true);
        state.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("auth"), "不该写空字段: {text}");
    }

    #[test]
    fn save_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        State::default().save(&path).unwrap();
        // 临时文件不该留下
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        State::default().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "状态文件里有凭证，实际权限 {mode:o}");
    }

    #[test]
    fn unknown_fields_do_not_break_older_clients() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"token":"cy_a_b","future_setting":true,"tunnels":{}}"#,
        )
        .unwrap();
        assert_eq!(State::load(&path).token, "cy_a_b");
    }
}
