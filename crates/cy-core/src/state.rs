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
}

impl Default for TunnelEntry {
    fn default() -> Self {
        Self {
            local_port: 0,
            enabled: true,
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
            .map(|(name, e)| TunnelSpec::http(name, e.local_port))
            .collect()
    }

    pub fn upsert_tunnel(&mut self, name: &str, local_port: u16, enabled: bool) {
        let entry = self.tunnels.entry(name.to_string()).or_default();
        entry.local_port = local_port;
        entry.enabled = enabled;
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
        state.upsert_tunnel("wx", 8082, true);
        state.upsert_tunnel("admin", 5173, false);
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
