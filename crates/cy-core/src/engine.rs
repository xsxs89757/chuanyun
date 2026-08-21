//! 引擎：把「连接」这件不稳定的事包起来，对外只暴露稳定的意图。
//!
//! 界面和脚本关心的是「我要开一条叫 wx 的隧道」，不关心当前有没有连上、
//! 是不是正在第三次重连。所以这里维护两份状态：
//!
//! - **期望态**：用户配了哪些隧道、哪些开着（存在磁盘上，重开应用要恢复）
//! - **实际态**：现在连上没有、每条隧道拿到的公网地址是什么
//!
//! 连接断了就退避重连，连上之后把期望态里开着的隧道全部重开一遍。用户不需要
//! 做任何事，也不该看到「请重新点一次开关」这种要求。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::backoff::Backoff;
use crate::client::{self, ConnectError, Connection, CoreConfig, Event, TunnelSpec, Verify};
use crate::inspector::Inspector;
use crate::state::State;

/// 对外的状态快照。界面和本地 API 都读它。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub connected: bool,
    /// 正在重连时的第几次尝试；0 表示不在重连
    pub reconnect_attempt: u32,
    pub domain_suffix: String,
    /// 最近一次连接失败的原因，连上后清空
    pub last_error: Option<String>,
    /// 凭证被拒——这种情况不会自动重连，要用户重新登录
    pub needs_login: bool,
    pub tunnels: Vec<TunnelStatus>,
}

impl Status {
    pub fn tunnel(&self, name: &str) -> Option<&TunnelStatus> {
        self.tunnels.iter().find(|t| t.name == name)
    }

    /// 某个本地端口现在对外的地址。没开隧道就返回 `None`。
    pub fn public_url_for_port(&self, port: u16) -> Option<&str> {
        self.tunnels
            .iter()
            .filter(|t| t.local_port == port)
            .find_map(|t| t.url.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TunnelStatus {
    pub name: String,
    pub local_port: u16,
    pub enabled: bool,
    /// 已开通时的公网地址
    pub url: Option<String>,
    /// 开通失败的原因
    pub error: Option<String>,
}

enum Cmd {
    Login {
        server: String,
        token: String,
        pin: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Logout,
    AddTunnel {
        name: String,
        local_port: u16,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveTunnel {
        name: String,
    },
    SetEnabled {
        name: String,
        enabled: bool,
    },
    Shutdown,
}

/// 引擎句柄，可克隆，跨线程共享。
#[derive(Clone)]
pub struct Engine {
    cmds: mpsc::Sender<Cmd>,
    events: broadcast::Sender<Event>,
    status: Arc<RwLock<Status>>,
    inspector: Inspector,
}

impl Engine {
    /// 启动引擎。状态从 `state_path` 读，之后的改动也写回那里。
    pub fn start(state_path: Option<PathBuf>, brand: Brand) -> Engine {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        let status = Arc::new(RwLock::new(Status::default()));
        let inspector = Inspector::new();

        let engine = Engine {
            cmds: cmd_tx,
            events: events.clone(),
            status: status.clone(),
            inspector: inspector.clone(),
        };

        tokio::spawn(supervisor(
            cmd_rx, events, status, state_path, brand, inspector,
        ));
        engine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// 请求记录本：观测面板和重放都从这里取。
    pub fn inspector(&self) -> &Inspector {
        &self.inspector
    }

    pub fn status(&self) -> Status {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub async fn login(
        &self,
        server: impl Into<String>,
        token: impl Into<String>,
        pin: impl Into<String>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Login {
                server: server.into(),
                token: token.into(),
                pin: pin.into(),
                reply,
            })
            .await
            .map_err(|_| "引擎已停止".to_string())?;
        rx.await.map_err(|_| "引擎已停止".to_string())?
    }

    pub async fn logout(&self) {
        let _ = self.cmds.send(Cmd::Logout).await;
    }

    /// 新增一条隧道并立刻开通。
    pub async fn add_tunnel(&self, name: impl Into<String>, local_port: u16) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::AddTunnel {
                name: name.into(),
                local_port,
                reply,
            })
            .await
            .map_err(|_| "引擎已停止".to_string())?;
        rx.await.map_err(|_| "引擎已停止".to_string())?
    }

    pub async fn remove_tunnel(&self, name: impl Into<String>) {
        let _ = self
            .cmds
            .send(Cmd::RemoveTunnel { name: name.into() })
            .await;
    }

    pub async fn set_enabled(&self, name: impl Into<String>, enabled: bool) {
        let _ = self
            .cmds
            .send(Cmd::SetEnabled {
                name: name.into(),
                enabled,
            })
            .await;
    }

    pub async fn shutdown(&self) {
        let _ = self.cmds.send(Cmd::Shutdown).await;
    }
}

/// 编译进二进制的默认值（品牌注入）。
#[derive(Debug, Clone, Default)]
pub struct Brand {
    pub default_server: String,
    pub tls_pin: String,
    pub update_url: String,
}

/// 主循环：维持连接、执行命令、把两份状态对齐。
async fn supervisor(
    mut cmds: mpsc::Receiver<Cmd>,
    events: broadcast::Sender<Event>,
    status: Arc<RwLock<Status>>,
    state_path: Option<PathBuf>,
    brand: Brand,
    inspector: Inspector,
) {
    let mut state = match &state_path {
        Some(p) => State::load(p),
        None => State::default(),
    };
    let mut backoff = Backoff::default();
    // 保存待回复的登录请求：连上（或确定连不上）之后才回复，
    // 这样界面上的「登录」按钮能一直转到有结果为止。
    let mut login_reply: Option<oneshot::Sender<Result<(), String>>> = None;

    loop {
        // ---- 没有凭证：等登录 ----
        if state.token.is_empty() {
            set_status(&status, |s| {
                s.connected = false;
                s.needs_login = true;
            });
            match cmds.recv().await {
                Some(cmd) => {
                    if matches!(
                        handle_offline_cmd(cmd, &mut state, &state_path, &status, &mut login_reply),
                        CmdOutcome::Stop
                    ) {
                        break;
                    }
                }
                None => break,
            }
            continue;
        }

        // ---- 尝试连接 ----
        let config = CoreConfig {
            server: effective_server(&state, &brand),
            token: state.token.clone(),
            verify: effective_verify(&state, &brand),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        };

        match client::connect(&config, events.clone(), inspector.clone()).await {
            Ok(conn) => {
                backoff.reset();
                if let Some(reply) = login_reply.take() {
                    let _ = reply.send(Ok(()));
                }
                set_status(&status, |s| {
                    s.connected = true;
                    s.needs_login = false;
                    s.reconnect_attempt = 0;
                    s.last_error = None;
                    s.domain_suffix = conn.domain_suffix.clone();
                });

                let outcome = session(conn, &mut cmds, &mut state, &state_path, &status).await;

                set_status(&status, |s| {
                    s.connected = false;
                    for t in &mut s.tunnels {
                        t.url = None;
                    }
                });

                match outcome {
                    SessionEnd::Shutdown => break,
                    SessionEnd::LoggedOut => continue,
                    SessionEnd::Dropped(reason) => {
                        set_status(&status, |s| s.last_error = Some(reason));
                    }
                }
            }
            Err(ConnectError::Rejected(reason)) => {
                // 凭证有问题，重连一万次也没用——停下来等用户处理
                if let Some(reply) = login_reply.take() {
                    let _ = reply.send(Err(reason.clone()));
                }
                set_status(&status, |s| {
                    s.connected = false;
                    s.needs_login = true;
                    s.last_error = Some(reason.clone());
                });
                let _ = events.send(Event::AuthRejected { reason });
                state.token.clear();
                continue;
            }
            Err(e) => {
                let reason = e.to_string();
                if let Some(reply) = login_reply.take() {
                    // 第一次登录就连不上，直接告诉用户，别让按钮一直转
                    let _ = reply.send(Err(reason.clone()));
                    state.token.clear();
                    set_status(&status, |s| {
                        s.needs_login = true;
                        s.last_error = Some(reason);
                    });
                    continue;
                }
                set_status(&status, |s| s.last_error = Some(reason));
            }
        }

        // ---- 退避重连 ----
        let delay = backoff.next_delay();
        set_status(&status, |s| s.reconnect_attempt = backoff.attempt());
        tracing::info!(?delay, attempt = backoff.attempt(), "准备重连");

        // 等待期间仍然要能响应命令：用户可能在这时候改配置、加隧道，
        // 甚至换了个账号登录。让他干等到退避结束才有反应是很糟的体验。
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => match handle_offline_cmd(
                        cmd, &mut state, &state_path, &status, &mut login_reply,
                    ) {
                        CmdOutcome::Stop => return,
                        // 换了凭证，没必要再等——立刻试
                        CmdOutcome::RetryNow => break,
                        // 加隧道之类的，记下就好，接着等这一轮退避
                        CmdOutcome::Continue => {}
                    },
                    None => return,
                }
            }
        }
    }

    tracing::info!("引擎已停止");
}

enum SessionEnd {
    /// 连接断了，该重连
    Dropped(String),
    /// 用户主动退出登录
    LoggedOut,
    Shutdown,
}

/// 连上之后的日常：开隧道、执行命令、盯着连接是否还活着。
async fn session(
    conn: Connection,
    cmds: &mut mpsc::Receiver<Cmd>,
    state: &mut State,
    state_path: &Option<PathBuf>,
    status: &Arc<RwLock<Status>>,
) -> SessionEnd {
    // 重连之后把期望态里开着的隧道全部重开——用户不该被要求「重新点一次开关」。
    // 单条失败（比如撞名）不影响其余的，失败原因已经记进状态里给界面展示。
    for spec in state.enabled_tunnels() {
        let _ = open_and_record(&conn, &spec, status).await;
    }

    // 睡眠唤醒检测：合盖再打开时，单调时钟没走多少但墙上时间跳了一大截。
    // 这时连接多半已经死了，与其等心跳超时，不如立刻去探一下。
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_check = (Instant::now(), SystemTime::now());

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = (Instant::now(), SystemTime::now());
                let mono = now.0.duration_since(last_check.0);
                let wall = now.1.duration_since(last_check.1).unwrap_or_default();
                last_check = now;

                if wall > mono + Duration::from_secs(5) {
                    tracing::info!(?wall, ?mono, "检测到系统休眠唤醒，立刻检查连接");
                    if !conn.is_alive() {
                        return SessionEnd::Dropped("休眠唤醒后连接已失效".into());
                    }
                }
                if !conn.is_alive() {
                    return SessionEnd::Dropped("连接已断开".into());
                }
            }

            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return SessionEnd::Shutdown };
                match cmd {
                    Cmd::Shutdown => {
                        conn.disconnect();
                        return SessionEnd::Shutdown;
                    }
                    Cmd::Logout => {
                        conn.disconnect();
                        state.token.clear();
                        save(state, state_path);
                        set_status(status, |s| {
                            s.needs_login = true;
                            s.tunnels.iter_mut().for_each(|t| t.url = None);
                        });
                        return SessionEnd::LoggedOut;
                    }
                    Cmd::Login { server, token, pin, reply } => {
                        // 换账号：断开旧连接，让外层循环用新凭证重连
                        conn.disconnect();
                        state.server = server;
                        state.token = token;
                        state.tls_pin = pin;
                        save(state, state_path);
                        let _ = reply.send(Ok(()));
                        return SessionEnd::Dropped("切换账号".into());
                    }
                    Cmd::AddTunnel { name, local_port, reply } => {
                        if let Err(e) = cy_proto::naming::validate_name(&name) {
                            let _ = reply.send(Err(cy_proto::error::human(e).to_string()));
                            continue;
                        }
                        state.upsert_tunnel(&name, local_port, true);
                        save(state, state_path);
                        let spec = TunnelSpec::http(&name, local_port);
                        let result = open_and_record(&conn, &spec, status).await;
                        let _ = reply.send(result);
                    }
                    Cmd::RemoveTunnel { name } => {
                        conn.close_tunnel(&name).await;
                        state.remove_tunnel(&name);
                        save(state, state_path);
                        set_status(status, |s| s.tunnels.retain(|t| t.name != name));
                    }
                    Cmd::SetEnabled { name, enabled } => {
                        state.set_enabled(&name, enabled);
                        save(state, state_path);
                        if enabled {
                            if let Some(entry) = state.tunnels.get(&name) {
                                let spec = TunnelSpec::http(&name, entry.local_port);
                                let _ = open_and_record(&conn, &spec, status).await;
                            }
                        } else {
                            conn.close_tunnel(&name).await;
                            set_status(status, |s| {
                                if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == name) {
                                    t.enabled = false;
                                    t.url = None;
                                }
                            });
                        }
                    }
                }
            }
        }
    }
}

async fn open_and_record(
    conn: &Connection,
    spec: &TunnelSpec,
    status: &Arc<RwLock<Status>>,
) -> Result<(), String> {
    let result = conn.open_tunnel(spec.clone()).await;
    set_status(status, |s| {
        let entry = match s.tunnels.iter_mut().find(|t| t.name == spec.name) {
            Some(e) => e,
            None => {
                s.tunnels.push(TunnelStatus {
                    name: spec.name.clone(),
                    local_port: spec.local_port,
                    enabled: true,
                    url: None,
                    error: None,
                });
                s.tunnels.last_mut().expect("刚推入")
            }
        };
        entry.local_port = spec.local_port;
        entry.enabled = true;
        match &result {
            Ok(url) => {
                entry.url = Some(url.clone());
                entry.error = None;
            }
            Err(e) => {
                entry.url = None;
                entry.error = Some(e.clone());
            }
        }
    });
    result.map(|_| ())
}

/// 离线命令处理完之后，主循环该怎么走。
#[derive(Debug, PartialEq, Eq)]
enum CmdOutcome {
    /// 引擎该停了
    Stop,
    /// 凭证或服务器变了，别再等退避，立刻重试
    RetryNow,
    /// 记下就行，接着等
    Continue,
}

/// 离线时也能执行的命令。
fn handle_offline_cmd(
    cmd: Cmd,
    state: &mut State,
    state_path: &Option<PathBuf>,
    status: &Arc<RwLock<Status>>,
    login_reply: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> CmdOutcome {
    match cmd {
        Cmd::Shutdown => return CmdOutcome::Stop,
        Cmd::Login {
            server,
            token,
            pin,
            reply,
        } => {
            state.server = server;
            state.token = token;
            state.tls_pin = pin;
            save(state, state_path);
            // 连上（或失败）之后才回复
            *login_reply = Some(reply);
            return CmdOutcome::RetryNow;
        }
        Cmd::Logout => {
            state.token.clear();
            save(state, state_path);
        }
        Cmd::AddTunnel {
            name,
            local_port,
            reply,
        } => {
            // 没连上也让加——记进期望态，连上后自动开通。
            // 报错说"请先登录"然后把用户输入丢掉是最烦人的那种交互。
            match cy_proto::naming::validate_name(&name) {
                Ok(()) => {
                    state.upsert_tunnel(&name, local_port, true);
                    save(state, state_path);
                    set_status(status, |s| {
                        if !s.tunnels.iter().any(|t| t.name == name) {
                            s.tunnels.push(TunnelStatus {
                                name: name.clone(),
                                local_port,
                                enabled: true,
                                url: None,
                                error: None,
                            });
                        }
                    });
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(cy_proto::error::human(e).to_string()));
                }
            }
        }
        Cmd::RemoveTunnel { name } => {
            state.remove_tunnel(&name);
            save(state, state_path);
            set_status(status, |s| s.tunnels.retain(|t| t.name != name));
        }
        Cmd::SetEnabled { name, enabled } => {
            state.set_enabled(&name, enabled);
            save(state, state_path);
            set_status(status, |s| {
                if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == name) {
                    t.enabled = enabled;
                }
            });
        }
    }
    CmdOutcome::Continue
}

fn effective_server(state: &State, brand: &Brand) -> String {
    if state.server.is_empty() {
        brand.default_server.clone()
    } else {
        state.server.clone()
    }
}

fn effective_verify(state: &State, brand: &Brand) -> Verify {
    let pin = if state.tls_pin.is_empty() {
        &brand.tls_pin
    } else {
        &state.tls_pin
    };
    if pin.is_empty() {
        // 没有可比对的指纹，只能先信任并记下来（TOFU）
        Verify::Tofu
    } else {
        Verify::Pin(pin.clone())
    }
}

fn save(state: &State, path: &Option<PathBuf>) {
    if let Some(p) = path {
        if let Err(e) = state.save(p) {
            tracing::warn!(error = %e, "保存状态失败");
        }
    }
}

fn set_status(status: &Arc<RwLock<Status>>, f: impl FnOnce(&mut Status)) {
    let mut guard = status.write().unwrap_or_else(|e| e.into_inner());
    f(&mut guard);
}

/// 把状态里的隧道配置转成对外的快照（未连接时用）。
pub fn tunnels_from_state(state: &State) -> Vec<TunnelStatus> {
    state
        .tunnels
        .iter()
        .map(|(name, e)| TunnelStatus {
            name: name.clone(),
            local_port: e.local_port,
            enabled: e.enabled,
            url: None,
            error: None,
        })
        .collect()
}

/// 供本地 API 与界面共享的类型别名。
pub type TunnelMap = BTreeMap<String, TunnelStatus>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_supplies_defaults_until_the_user_overrides() {
        let brand = Brand {
            default_server: "tunnel.company.com:7000".into(),
            tls_pin: "aa".repeat(32),
            ..Default::default()
        };

        // 全新安装：用品牌里的值，同事什么都不用填
        let fresh = State::default();
        assert_eq!(effective_server(&fresh, &brand), "tunnel.company.com:7000");
        assert!(matches!(effective_verify(&fresh, &brand), Verify::Pin(_)));

        // 用户改过：以用户的为准（临时连测试环境的场景）
        let custom = State {
            server: "staging.company.com:7000".into(),
            ..Default::default()
        };
        assert_eq!(
            effective_server(&custom, &brand),
            "staging.company.com:7000"
        );
    }

    #[test]
    fn without_a_pin_we_fall_back_to_tofu() {
        // 开源发行版没有内置指纹，首连时问用户
        let state = State::default();
        assert!(matches!(
            effective_verify(&state, &Brand::default()),
            Verify::Tofu
        ));
    }

    #[test]
    fn status_finds_the_url_for_a_port() {
        let status = Status {
            tunnels: vec![
                TunnelStatus {
                    name: "api".into(),
                    local_port: 8082,
                    enabled: true,
                    url: Some("https://zhangsan-api.t.example.com".into()),
                    error: None,
                },
                TunnelStatus {
                    name: "web".into(),
                    local_port: 5173,
                    enabled: true,
                    url: None,
                    error: None,
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            status.public_url_for_port(8082),
            Some("https://zhangsan-api.t.example.com")
        );
        // 隧道没开通就没有公网地址——调用方据此回退到本地
        assert_eq!(status.public_url_for_port(5173), None);
        assert_eq!(status.public_url_for_port(9999), None);
    }
}
