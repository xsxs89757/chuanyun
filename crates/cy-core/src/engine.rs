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
use crate::client::{
    self, ConnectError, Connection, CoreConfig, Event, OpenError, TunnelSpec, Verify,
};
use crate::connect::{ActiveConnect, ConnectSpec};
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
    /// 我接入的别人的服务
    pub connects: Vec<ConnectStatus>,
    /// 服务端上有比当前更新的客户端。
    ///
    /// 连上时服务端顺手告诉我们它那里最新的包是哪个版本（它看自己的下载目录），
    /// 比当前版本新就填上。不用另外去查 GitHub：国内办公室常常连不上，API
    /// 还按出口 IP 限流，全公司一个 IP 几下就用完了。
    pub update: Option<UpdateAvailable>,
}

/// 有新版本可用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAvailable {
    pub version: String,
    /// 去哪下载；服务端没配 download_url 时为空，界面就只提示不给链接
    pub url: Option<String>,
}

/// 一条接入的当前状态。
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectStatus {
    pub local_port: u16,
    pub from: String,
    /// 补全后的上游地址
    pub upstream: String,
    pub running: bool,
    pub error: Option<String>,
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
    /// 设了访问口令。
    ///
    /// 界面要靠它显示「已设口令」——设了口令却看不出来，忘了就要命：
    /// 发地址给别人却不知道对方会被要口令。口令本身不进状态（那是给界面看的）。
    pub protected: bool,
    /// 口令的用户名部分。浏览器弹框是「用户名 / 密码」两格，用户对着一串
    /// `demo:123456` 很容易把整串填进密码框——卡片上把用户名亮出来就不会猜错。
    /// 只有用户名，口令那半永远不放进状态。
    pub auth_user: Option<String>,
}

/// `用户名:口令` 里的用户名那半。
fn auth_user_of(auth: Option<&str>) -> Option<String> {
    auth.and_then(|a| a.split_once(':'))
        .map(|(u, _)| u.to_string())
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
        spec: TunnelSpec,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveTunnel {
        name: String,
    },
    SetEnabled {
        name: String,
        enabled: bool,
        /// 处理完才回——脚本 PATCH {"enabled":false} 之后紧接着就可能退出，
        /// 得等隧道真的关掉了再返回，不然下一步读状态看到的还是开着的
        reply: oneshot::Sender<()>,
    },
    /// 改口令；`None` = 去掉口令
    SetAuth {
        name: String,
        auth: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    AddConnect {
        spec: ConnectSpec,
        reply: oneshot::Sender<Result<String, String>>,
    },
    RemoveConnect {
        local_port: u16,
        reply: oneshot::Sender<()>,
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
    /// 主循环退出后变成 true。退出应用前要等到它，见 [`Engine::shutdown`]
    stopped: tokio::sync::watch::Receiver<bool>,
}

impl Engine {
    /// 启动引擎。状态从 `state_path` 读，之后的改动也写回那里。
    pub fn start(state_path: Option<PathBuf>, brand: Brand) -> Engine {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        let status = Arc::new(RwLock::new(Status::default()));
        let inspector = Inspector::new();
        let (stopped_tx, stopped) = tokio::sync::watch::channel(false);

        let engine = Engine {
            cmds: cmd_tx,
            events: events.clone(),
            status: status.clone(),
            inspector: inspector.clone(),
            stopped,
        };

        tokio::spawn(async move {
            // 主循环不管从哪条路退出，都要让等着它的人知道
            let _stopped = StoppedOnDrop(stopped_tx);
            supervisor(cmd_rx, events, status, state_path, brand, inspector).await;
        });
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
        self.add_tunnel_spec(TunnelSpec::http(name, local_port))
            .await
    }

    /// 新增一条隧道，可以带访问口令或自定义域名。
    ///
    /// 走完整的 spec，不是 (name, port) 两个参数——访问口令和自定义域名
    /// 就是因为这条路只传了名字和端口，一路被丢到了协议层之前。
    pub async fn add_tunnel_spec(&self, spec: TunnelSpec) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::AddTunnel { spec, reply })
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
        let (reply, rx) = oneshot::channel();
        let _ = self
            .cmds
            .send(Cmd::SetEnabled {
                name: name.into(),
                enabled,
                reply,
            })
            .await;
        let _ = rx.await;
    }

    /// 改一条隧道的访问口令（`None` 去掉口令）。地址不变。
    ///
    /// 这是和「删了重建」不同的路：隧道地址是固定的、要填进微信后台的东西，
    /// 为了换个口令把它删掉再建，意味着公网地址瞬断、而且用户得重新确认地址没变。
    pub async fn set_auth(
        &self,
        name: impl Into<String>,
        auth: Option<String>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::SetAuth {
                name: name.into(),
                auth,
                reply,
            })
            .await
            .map_err(|_| "引擎已停止".to_string())?;
        rx.await.map_err(|_| "引擎已停止".to_string())?
    }

    /// 接入同事的服务：把上游映射成本地的一个端口。
    ///
    /// 返回补全后的上游地址，方便界面显示"你现在连的是哪儿"。
    pub async fn add_connect(&self, spec: ConnectSpec) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::AddConnect { spec, reply })
            .await
            .map_err(|_| "引擎已停止".to_string())?;
        rx.await.map_err(|_| "引擎已停止".to_string())?
    }

    /// 停掉一条接入。返回时端口已经放开——调用方可以立刻拿它做别的事。
    pub async fn remove_connect(&self, local_port: u16) {
        let (reply, rx) = oneshot::channel();
        if self
            .cmds
            .send(Cmd::RemoveConnect { local_port, reply })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    /// 停下引擎：断开连接，等主循环真的退出了才返回。
    ///
    /// 必须等：退出应用紧跟在这之后。以前只是把命令投进队列就返回，进程随即结束，
    /// 连接有没有正常关掉全凭运气——没关掉的话服务端那边的旧会话还占着隧道名，
    /// 下次打开就撞名。
    ///
    /// 最多等多久由调用方定：主循环正卡在连服务器的 TCP 握手里时，要等握手
    /// 有了结果才轮得到处理这条命令。
    pub async fn shutdown(&self) {
        let _ = self.cmds.send(Cmd::Shutdown).await;
        let mut stopped = self.stopped.clone();
        let _ = stopped.wait_for(|s| *s).await;
    }
}

/// 引擎主循环的任务一结束（正常退出或被运行时丢弃）就置位。
struct StoppedOnDrop(tokio::sync::watch::Sender<bool>);

impl Drop for StoppedOnDrop {
    fn drop(&mut self) {
        self.0.send_replace(true);
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
    // 接入是纯客户端的事，和连没连上服务端无关——所以它活在 supervisor 这一层，
    // 不随会话生灭。
    let mut connects: Vec<ActiveConnect> = Vec::new();
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
                        handle_offline_cmd(
                            cmd,
                            &mut state,
                            &state_path,
                            &status,
                            &mut login_reply,
                            &mut connects,
                        )
                        .await,
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
                // 服务端那边最新的包比我新，就记下来给界面提示。
                // 比较用 update 模块那套（去 v 前缀、按数字比），别按字符串比——
                // "0.1.10" 按字符串是小于 "0.1.9" 的。
                let update = update_from(conn.latest_client.as_deref(), conn.download_url.clone());
                if let Some(u) = &update {
                    tracing::info!(version = %u.version, "服务端上有新版客户端");
                }
                set_status(&status, |s| {
                    s.connected = true;
                    s.needs_login = false;
                    s.reconnect_attempt = 0;
                    s.last_error = None;
                    s.domain_suffix = conn.domain_suffix.clone();
                    s.update = update;
                });

                let outcome = session(
                    conn,
                    &mut cmds,
                    &mut state,
                    &state_path,
                    &status,
                    &mut connects,
                )
                .await;

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
                        cmd,
                        &mut state,
                        &state_path,
                        &status,
                        &mut login_reply,
                        &mut connects,
                    )
                    .await
                    {
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
    connects: &mut Vec<ActiveConnect>,
) -> SessionEnd {
    // 撞名的隧道过一会儿再试：旧连接可能还没被服务端清掉
    let mut taken = TakenRetries::new(conn.heartbeat_secs);

    // 重连之后把期望态里开着的隧道全部重开——用户不该被要求「重新点一次开关」。
    // 单条失败不影响其余的，失败原因已经记进状态里给界面展示。
    for spec in state.enabled_tunnels() {
        let _ = open_tracked(&conn, &spec, status, &mut taken).await;
    }

    // 睡眠唤醒检测：合盖再打开时，单调时钟没走多少但墙上时间跳了一大截。
    // 这时连接多半已经死了，与其等心跳超时，不如立刻去探一下。
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 心跳每次都带最新版本。连着不断的客户端就靠它知道管理员刚放了新包——
    // 否则只有重连那一下看得到，一连一星期的人永远收不到提示。
    let mut latest = conn.latest_client_updates();
    let mut last_check = (Instant::now(), SystemTime::now());

    loop {
        tokio::select! {
            changed = latest.changed() => {
                if changed.is_ok() {
                    let l = latest.borrow_and_update().clone();
                    let update = update_from(l.version.as_deref(), l.download_url);
                    if let Some(u) = &update {
                        tracing::info!(version = %u.version, "心跳里得知服务端上有新版客户端");
                    }
                    set_status(status, |s| s.update = update);
                }
            }
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

                for name in taken.due(Instant::now()) {
                    // 等的这段时间里用户可能已经把它关了或删了
                    match state.spec(&name).filter(|_| spec_enabled(state, &name)) {
                        Some(spec) => {
                            let _ = open_tracked(&conn, &spec, status, &mut taken).await;
                        }
                        None => taken.forget(&name),
                    }
                }
            }

            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return SessionEnd::Shutdown };
                match cmd {
                    Cmd::Shutdown => {
                        // 等控制流真的关掉再走：服务端据此当场放掉这条连接占着的
                        // 隧道名，下次打开（或新版本接管）就不会撞名
                        conn.close(Duration::from_millis(500)).await;
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
                    Cmd::AddTunnel { spec, reply } => {
                        if let Err(e) = cy_proto::naming::validate_name(&spec.name) {
                            let _ = reply.send(Err(cy_proto::error::human(e).to_string()));
                            continue;
                        }
                        let spec = state.upsert_tunnel(&spec, true);
                        save(state, state_path);
                        // 同名隧道已经开着：端口没变就直接回成功——脚本每次启动都
                        // 注册一遍，别每次都关了重开（公网地址会瞬断）；端口变了
                        // （dev.sh 自动避让挪到了 5669）就关掉重开指向新端口。
                        // 两种情况口令都在 state 里原样保留着。
                        // 这样脚本就不需要「先 DELETE 再 POST」——那会把 state 里的
                        // 条目连口令一起删掉。
                        let (is_up, same_port) = status
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .tunnel(&spec.name)
                            .map(|t| (t.url.is_some(), t.local_port == spec.local_port))
                            .unwrap_or((false, false));
                        let result = if is_up && same_port {
                            Ok(())
                        } else {
                            if is_up {
                                conn.close_tunnel(&spec.name).await;
                            }
                            open_tracked(&conn, &spec, status, &mut taken).await
                        };
                        let _ = reply.send(result);
                    }
                    Cmd::RemoveTunnel { name } => {
                        taken.forget(&name);
                        conn.close_tunnel(&name).await;
                        state.remove_tunnel(&name);
                        save(state, state_path);
                        set_status(status, |s| s.tunnels.retain(|t| t.name != name));
                    }
                    Cmd::AddConnect { spec, reply } => {
                        let suffix = status
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .domain_suffix
                            .clone();
                        let result = add_connect(spec, &suffix, connects, status).await;
                        let _ = reply.send(result);
                    }
                    Cmd::RemoveConnect { local_port, reply } => {
                        remove_connect(local_port, connects, status).await;
                        let _ = reply.send(());
                    }
                    Cmd::SetAuth { name, auth, reply } => {
                        if !state.set_auth(&name, auth) {
                            let _ = reply.send(Err(format!("没有叫 {name} 的隧道")));
                            continue;
                        }
                        save(state, state_path);
                        // 口令是服务端在入口处校验的，得把新的送过去：关掉重开这条隧道。
                        // 地址由名字决定，重开之后还是同一个。
                        let result = match state.spec(&name) {
                            Some(spec) if spec_enabled(state, &name) => {
                                conn.close_tunnel(&name).await;
                                open_tracked(&conn, &spec, status, &mut taken).await
                            }
                            _ => Ok(()),
                        };
                        let auth_now = state.spec(&name).and_then(|sp| sp.auth);
                        set_status(status, |s| {
                            if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == name) {
                                t.protected = auth_now.is_some();
                                t.auth_user = auth_user_of(auth_now.as_deref());
                            }
                        });
                        let _ = reply.send(result);
                    }
                    Cmd::SetEnabled { name, enabled, reply } => {
                        state.set_enabled(&name, enabled);
                        save(state, state_path);
                        if enabled {
                            // 从 state 取完整定义，别现拼——口令和自定义域名都在里面
                            if let Some(spec) = state.spec(&name) {
                                let _ = open_tracked(&conn, &spec, status, &mut taken).await;
                            }
                        } else {
                            taken.forget(&name);
                            conn.close_tunnel(&name).await;
                            set_status(status, |s| {
                                if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == name) {
                                    t.enabled = false;
                                    t.url = None;
                                }
                            });
                        }
                        let _ = reply.send(());
                    }
                }
            }
        }
    }
}

/// 服务端报的最新版本比我新，就是一次可提示的更新。
///
/// 比较用 update 模块那套（去 v 前缀、按数字比），别按字符串比——
/// "0.1.10" 按字符串是小于 "0.1.9" 的。
fn update_from(latest: Option<&str>, url: Option<String>) -> Option<UpdateAvailable> {
    let latest = latest?;
    crate::update::is_newer(latest, env!("CARGO_PKG_VERSION")).then(|| UpdateAvailable {
        version: latest.to_string(),
        url,
    })
}

fn spec_enabled(state: &State, name: &str) -> bool {
    state.tunnels.get(name).map(|e| e.enabled).unwrap_or(false)
}

/// 撞名之后隔多久再试。
const TAKEN_RETRY_EVERY: Duration = Duration::from_secs(3);

/// 撞名之后最多再等几个心跳。服务端连续 3 次心跳没回应才认定旧连接已死，
/// 第 4 下才把它清掉；再留两个心跳的余量。
const TAKEN_RETRY_HEARTBEATS: u32 = 6;

/// 撞名、正在等服务端放手时卡片上的话。
const TAKEN_RETRYING: &str = "隧道名还被上一条连接占着，等服务端清掉后会自动开通（正在重试）";

/// 撞了名的隧道：过一会儿再试。
///
/// 断线重连、或者刚退出又马上打开时，服务端未必已经知道旧连接没了——TCP 断开的
/// 信号可能半路丢了，那就要等旧连接连续几次心跳没回应。在那之前新连接去开同名
/// 隧道只会被拒。以前拒了就不再试，隧道一直挂着「已被占用」，要用户手动拨一下开关。
///
/// 只管「名字被占」这一种错误：名字不合法、超出上限这些，重试多少次都一样。
struct TakenRetries {
    window: Duration,
    every: Duration,
    pending: BTreeMap<String, TakenRetry>,
}

struct TakenRetry {
    give_up_at: Instant,
    next_at: Instant,
}

/// 一次开通之后，这条隧道该怎么办。
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// 开通了，或者是别的错误——都不用再试
    Done,
    /// 还被占着，过一会儿再试；`first` 表示这是头一回撞上
    Retrying { first: bool },
    /// 等够了还被占着，多半真有另一个地方开着它
    GaveUp,
}

impl TakenRetries {
    fn new(heartbeat_secs: u64) -> Self {
        let heartbeat = Duration::from_secs(heartbeat_secs.max(1));
        Self {
            window: heartbeat * TAKEN_RETRY_HEARTBEATS,
            every: TAKEN_RETRY_EVERY.min(heartbeat),
            pending: BTreeMap::new(),
        }
    }

    fn record(&mut self, name: &str, result: &Result<(), OpenError>, now: Instant) -> Verdict {
        match result {
            Err(e) if e.is_taken() => {
                let first = !self.pending.contains_key(name);
                let retry = self.pending.entry(name.to_string()).or_insert(TakenRetry {
                    give_up_at: now + self.window,
                    next_at: now,
                });
                if now >= retry.give_up_at {
                    self.pending.remove(name);
                    Verdict::GaveUp
                } else {
                    retry.next_at = now + self.every;
                    Verdict::Retrying { first }
                }
            }
            _ => {
                self.pending.remove(name);
                Verdict::Done
            }
        }
    }

    /// 到点该再试的隧道。
    fn due(&self, now: Instant) -> Vec<String> {
        self.pending
            .iter()
            .filter(|(_, r)| r.next_at <= now)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// 用户关掉或删掉了它，不用再等。
    fn forget(&mut self, name: &str) {
        self.pending.remove(name);
    }
}

/// 开一条隧道；撞名的话记下来，稍后自动再试。
async fn open_tracked(
    conn: &Connection,
    spec: &TunnelSpec,
    status: &Arc<RwLock<Status>>,
    taken: &mut TakenRetries,
) -> Result<(), String> {
    let result = open_and_record(conn, spec, status).await;
    match taken.record(&spec.name, &result, Instant::now()) {
        Verdict::Retrying { first: true } => {
            tracing::info!(
                name = %spec.name,
                window = ?taken.window,
                "隧道名被占着，可能是服务端还没清掉上一条连接，稍后自动重试"
            );
        }
        Verdict::GaveUp => {
            tracing::info!(name = %spec.name, "隧道名一直被占着，不再重试");
            let text = cy_proto::error::human(cy_proto::error::code::SUBDOMAIN_TAKEN).to_string();
            set_status(status, |s| {
                if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == spec.name) {
                    t.error = Some(text.clone());
                }
            });
            return Err(text);
        }
        Verdict::Retrying { first: false } | Verdict::Done => {}
    }
    result.map_err(|e| shown_error(&e))
}

/// 卡片上给这个错误显示什么。
fn shown_error(e: &OpenError) -> String {
    if e.is_taken() {
        TAKEN_RETRYING.to_string()
    } else {
        e.message.clone()
    }
}

async fn open_and_record(
    conn: &Connection,
    spec: &TunnelSpec,
    status: &Arc<RwLock<Status>>,
) -> Result<(), OpenError> {
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
                    protected: spec.auth.is_some(),
                    auth_user: auth_user_of(spec.auth.as_deref()),
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
                entry.error = Some(shown_error(e));
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
async fn handle_offline_cmd(
    cmd: Cmd,
    state: &mut State,
    state_path: &Option<PathBuf>,
    status: &Arc<RwLock<Status>>,
    login_reply: &mut Option<oneshot::Sender<Result<(), String>>>,
    connects: &mut Vec<ActiveConnect>,
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
        Cmd::AddTunnel { spec, reply } => {
            // 没连上也让加——记进期望态，连上后自动开通。
            // 报错说"请先登录"然后把用户输入丢掉是最烦人的那种交互。
            match cy_proto::naming::validate_name(&spec.name) {
                Ok(()) => {
                    let spec = state.upsert_tunnel(&spec, true);
                    save(state, state_path);
                    set_status(status, |s| {
                        if !s.tunnels.iter().any(|t| t.name == spec.name) {
                            s.tunnels.push(TunnelStatus {
                                name: spec.name.clone(),
                                local_port: spec.local_port,
                                enabled: true,
                                url: None,
                                error: None,
                                protected: spec.auth.is_some(),
                                auth_user: auth_user_of(spec.auth.as_deref()),
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
        Cmd::SetEnabled {
            name,
            enabled,
            reply,
        } => {
            state.set_enabled(&name, enabled);
            save(state, state_path);
            set_status(status, |s| {
                if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == name) {
                    t.enabled = enabled;
                }
            });
            let _ = reply.send(());
        }
        Cmd::SetAuth { name, auth, reply } => {
            // 没连上也让改——记进期望态，连上后按新口令开
            if !state.set_auth(&name, auth) {
                let _ = reply.send(Err(format!("没有叫 {name} 的隧道")));
                return CmdOutcome::Continue;
            }
            save(state, state_path);
            let auth_now = state.spec(&name).and_then(|sp| sp.auth);
            set_status(status, |s| {
                if let Some(t) = s.tunnels.iter_mut().find(|t| t.name == name) {
                    t.protected = auth_now.is_some();
                    t.auth_user = auth_user_of(auth_now.as_deref());
                }
            });
            let _ = reply.send(Ok(()));
        }
        Cmd::AddConnect { spec, reply } => {
            // 接入不需要先登录：上游是个普通的公网地址，本地代理直接就能起。
            let suffix = status
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .domain_suffix
                .clone();
            let result = add_connect(spec, &suffix, connects, status).await;
            let _ = reply.send(result);
        }
        Cmd::RemoveConnect { local_port, reply } => {
            remove_connect(local_port, connects, status).await;
            let _ = reply.send(());
        }
    }
    CmdOutcome::Continue
}

async fn add_connect(
    spec: ConnectSpec,
    domain_suffix: &str,
    connects: &mut Vec<ActiveConnect>,
    status: &Arc<RwLock<Status>>,
) -> Result<String, String> {
    // 同一个本地端口只能有一条接入——后来的替换先前的。
    // 这里必须等旧的真的让出端口（remove_connect 内部会等任务退出），
    // 否则紧接着的 bind 会撞上还没关掉的监听器。
    remove_connect(spec.local_port, connects, status).await;

    match crate::connect::start(spec.clone(), domain_suffix).await {
        Ok(active) => {
            let upstream = active.upstream.clone();
            connects.push(active);
            set_status(status, |s| {
                s.connects.push(ConnectStatus {
                    local_port: spec.local_port,
                    from: spec.from.clone(),
                    upstream: upstream.clone(),
                    running: true,
                    error: None,
                });
            });
            Ok(upstream)
        }
        Err(e) => {
            let msg = e.to_string();
            set_status(status, |s| {
                s.connects.push(ConnectStatus {
                    local_port: spec.local_port,
                    from: spec.from.clone(),
                    upstream: spec
                        .upstream_url(domain_suffix)
                        .unwrap_or_else(|_| spec.from.clone()),
                    running: false,
                    error: Some(msg.clone()),
                });
            });
            Err(msg)
        }
    }
}

async fn remove_connect(
    local_port: u16,
    connects: &mut Vec<ActiveConnect>,
    status: &Arc<RwLock<Status>>,
) {
    // 逐个停并等它放开端口，不能用 retain——那里面没法 await
    let mut i = 0;
    while i < connects.len() {
        if connects[i].spec.local_port == local_port {
            let mut c = connects.remove(i);
            c.stop().await;
        } else {
            i += 1;
        }
    }
    set_status(status, |s| {
        s.connects.retain(|c| c.local_port != local_port);
    });
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
            protected: e.auth.is_some(),
            auth_user: auth_user_of(e.auth.as_deref()),
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

    fn taken() -> Result<(), OpenError> {
        Err(OpenError {
            code: Some(cy_proto::error::code::SUBDOMAIN_TAKEN.into()),
            message: "该隧道名已被占用".into(),
        })
    }

    #[test]
    fn taken_names_are_retried_until_the_window_runs_out() {
        // 心跳 15 秒：每 3 秒试一次，最多等 6 个心跳（90 秒）
        let mut retries = TakenRetries::new(15);
        let t0 = Instant::now();

        assert_eq!(
            retries.record("wx", &taken(), t0),
            Verdict::Retrying { first: true }
        );
        assert!(retries.due(t0).is_empty(), "刚撞上不该马上再试");
        assert_eq!(retries.due(t0 + Duration::from_secs(3)), vec!["wx"]);

        let t1 = t0 + Duration::from_secs(3);
        assert_eq!(
            retries.record("wx", &taken(), t1),
            Verdict::Retrying { first: false }
        );

        // 窗口从头一回撞上算起，不因为每次重试往后推
        let late = t0 + Duration::from_secs(90);
        assert_eq!(retries.record("wx", &taken(), late), Verdict::GaveUp);
        assert!(
            retries.due(late + Duration::from_secs(60)).is_empty(),
            "放弃之后就别再试了"
        );
    }

    #[test]
    fn success_or_other_errors_stop_the_retries() {
        let mut retries = TakenRetries::new(15);
        let t0 = Instant::now();
        retries.record("wx", &taken(), t0);
        assert_eq!(retries.record("wx", &Ok(()), t0), Verdict::Done);
        assert!(retries.due(t0 + Duration::from_secs(60)).is_empty());

        // 名字不合法这类，重试多少次都一样
        let invalid = Err(OpenError {
            code: Some(cy_proto::error::code::NAME_INVALID.into()),
            message: "名称不合法".into(),
        });
        assert_eq!(retries.record("Bad", &invalid, t0), Verdict::Done);
        assert!(retries.due(t0 + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn forgetting_a_tunnel_stops_its_retries() {
        // 用户在等的时候把它关了或删了
        let mut retries = TakenRetries::new(15);
        let t0 = Instant::now();
        retries.record("wx", &taken(), t0);
        retries.forget("wx");
        assert!(retries.due(t0 + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn a_fast_heartbeat_shortens_the_retry_interval() {
        // 测试环境心跳 1 秒：3 秒一试就只够试两次，间隔要跟着缩短
        let mut retries = TakenRetries::new(1);
        let t0 = Instant::now();
        retries.record("wx", &taken(), t0);
        assert_eq!(retries.due(t0 + Duration::from_secs(1)), vec!["wx"]);
        assert_eq!(
            retries.record("wx", &taken(), t0 + Duration::from_secs(6)),
            Verdict::GaveUp
        );
    }

    #[test]
    fn taken_errors_read_as_a_wait_not_as_rename_it() {
        // 以前这里显示「请换一个名称」：照着改名的话，公网地址就跟着变了
        let shown = shown_error(&taken().unwrap_err());
        assert!(shown.contains("自动"), "撞名时该说在重试：{shown}");
        assert!(!shown.contains("请换一个名称"), "{shown}");
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
                    protected: false,
                    auth_user: None,
                },
                TunnelStatus {
                    name: "web".into(),
                    local_port: 5173,
                    enabled: true,
                    url: None,
                    error: None,
                    protected: false,
                    auth_user: None,
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
