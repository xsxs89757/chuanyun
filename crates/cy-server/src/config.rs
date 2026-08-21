//! 服务端配置（单文件 TOML）。
//!
//! 尽量让每一项都有合理默认值：一份最小配置只需要写 `domain_suffix`，
//! 剩下的照默认跑就行。部署时要改的东西越少，出错的机会越少。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub control: ControlConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub tcp: TcpConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub admin: AdminConfig,
}

/// 控制通道：客户端出站连过来的地方，自带 TLS。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    #[serde(default = "default_control_listen")]
    pub listen: SocketAddr,
    /// 心跳间隔。客户端据此发 ping，连续 3 次没回应就判定连接已死。
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

impl ControlConfig {
    pub fn heartbeat(&self) -> Duration {
        Duration::from_secs(self.heartbeat_secs)
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            listen: default_control_listen(),
            heartbeat_secs: default_heartbeat_secs(),
        }
    }
}

fn default_control_listen() -> SocketAddr {
    "0.0.0.0:7000".parse().expect("字面量地址")
}

fn default_heartbeat_secs() -> u64 {
    15
}

/// HTTP 入口的两种形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpMode {
    /// 前置 nginx（默认）：自己只监听内网明文 HTTP，TLS 与证书都归 nginx 管。
    /// 这样 443 端口不用抢，证书续期沿用现有流程。
    #[default]
    Nginx,
    /// 直连：自己监听 443 并终止 TLS，需要配好证书。适合没有现成 nginx 的部署。
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default = "default_http_listen")]
    pub listen: SocketAddr,
    #[serde(default)]
    pub mode: HttpMode,
    /// 隧道域名后缀，如 `t.example.com`。子域名拼成 `{user}-{name}.{suffix}`。
    #[serde(default)]
    pub domain_suffix: String,
    /// 对外展示的协议。nginx 模式下我们收到的是明文，但用户看到的地址是 https。
    #[serde(default = "default_scheme")]
    pub public_scheme: String,
    /// 信任哪些对端传来的 `X-Forwarded-For`。默认只信本机（也就是前置 nginx）——
    /// 谁都能伪造这个头，不加限制的话审计日志里的来源 IP 就没有意义了。
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// 直连模式下的证书与私钥（PEM）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<PathBuf>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: default_http_listen(),
            mode: HttpMode::default(),
            domain_suffix: String::new(),
            public_scheme: default_scheme(),
            trusted_proxies: default_trusted_proxies(),
            cert: None,
            key: None,
        }
    }
}

fn default_http_listen() -> SocketAddr {
    "127.0.0.1:7080".parse().expect("字面量地址")
}

fn default_scheme() -> String {
    "https".into()
}

fn default_trusted_proxies() -> Vec<std::net::IpAddr> {
    vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()]
}

/// TCP 隧道的公网端口池（V1.5 功能，配置先占位）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpConfig {
    /// 展示给用户的主机名（端口池监听在本机，但用户要连的是这个域名）
    #[serde(default)]
    pub public_host: String,
    #[serde(default = "default_port_range")]
    pub port_range: (u16, u16),
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            public_host: String::new(),
            port_range: default_port_range(),
        }
    }
}

fn default_port_range() -> (u16, u16) {
    (20000, 20100)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// SQLite 库、自签证书都放这里。
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
        }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/chuanyun")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default = "default_max_tunnels")]
    pub max_tunnels_per_user: u32,
    /// 握手失败多少次后锁定该来源 IP。
    #[serde(default = "default_fail_count")]
    pub handshake_fail_count: u32,
    /// 锁定的观察窗口（秒）。
    #[serde(default = "default_fail_window")]
    pub handshake_fail_window_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tunnels_per_user: default_max_tunnels(),
            handshake_fail_count: default_fail_count(),
            handshake_fail_window_secs: default_fail_window(),
        }
    }
}

fn default_max_tunnels() -> u32 {
    10
}
fn default_fail_count() -> u32 {
    5
}
fn default_fail_window() -> u64 {
    600
}

/// 管理接口。只监听回环——服务器上常有多个 ssh 用户，别把它暴露出去。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default = "default_admin_listen")]
    pub listen: SocketAddr,
    /// 放客户端安装包的目录，下载页从这里取文件。
    /// 不填就用 `<data_dir>/downloads`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<PathBuf>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            listen: default_admin_listen(),
            download_dir: None,
        }
    }
}

fn default_admin_listen() -> SocketAddr {
    "127.0.0.1:7001".parse().expect("字面量地址")
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取配置文件失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置文件格式有误: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// 校验那些「格式合法但跑起来一定不对」的组合，早点报错好过运行时才发现。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.http.domain_suffix.is_empty() {
            return Err(ConfigError::Invalid(
                "[http] domain_suffix 不能为空——没有它就没法给隧道分配子域名".into(),
            ));
        }
        if self.http.mode == HttpMode::Direct
            && (self.http.cert.is_none() || self.http.key.is_none())
        {
            return Err(ConfigError::Invalid(
                "[http] mode = \"direct\" 时必须同时配置 cert 与 key".into(),
            ));
        }
        let (lo, hi) = self.tcp.port_range;
        if lo > hi {
            return Err(ConfigError::Invalid(format!(
                "[tcp] port_range 的起点 {lo} 大于终点 {hi}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_only_needs_domain_suffix() {
        let config: Config = toml::from_str(
            r#"
            [http]
            domain_suffix = "t.example.com"
            "#,
        )
        .unwrap();
        config.validate().unwrap();

        assert_eq!(config.control.listen.port(), 7000);
        assert_eq!(config.http.listen.to_string(), "127.0.0.1:7080");
        assert_eq!(config.http.mode, HttpMode::Nginx);
        assert_eq!(config.limits.max_tunnels_per_user, 10);
        // 默认只信本机转发来的 XFF
        assert!(config
            .http
            .trusted_proxies
            .contains(&"127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn missing_domain_suffix_is_rejected() {
        let err = Config::default().validate().unwrap_err();
        assert!(err.to_string().contains("domain_suffix"));
    }

    #[test]
    fn direct_mode_requires_certificate() {
        let config: Config = toml::from_str(
            r#"
            [http]
            domain_suffix = "t.example.com"
            mode = "direct"
            "#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("cert"));
    }

    #[test]
    fn typos_in_config_are_caught_not_ignored() {
        // 打错字被默默忽略是最难查的一类问题——宁可启动就报错
        let err = toml::from_str::<Config>(
            r#"
            [http]
            domain_sufix = "t.example.com"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("domain_sufix"),
            "应指出打错的键: {err}"
        );
    }

    #[test]
    fn download_dir_defaults_to_none_and_can_be_set() {
        let c: Config = toml::from_str(
            r#"
            [http]
            domain_suffix = "t.example.com"
            "#,
        )
        .unwrap();
        assert!(
            c.admin.download_dir.is_none(),
            "不填就是 None，由上层拼默认路径"
        );

        let c: Config = toml::from_str(
            r#"
            [http]
            domain_suffix = "t.example.com"
            [admin]
            download_dir = "/opt/chuanyun/pkgs"
            "#,
        )
        .unwrap();
        assert_eq!(
            c.admin.download_dir.unwrap(),
            PathBuf::from("/opt/chuanyun/pkgs")
        );
    }

    #[test]
    fn backwards_port_range_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [http]
            domain_suffix = "t.example.com"
            [tcp]
            port_range = [30000, 20000]
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
