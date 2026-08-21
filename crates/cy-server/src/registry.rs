//! 活动会话与隧道路由表。
//!
//! 这是服务端唯一的"现在谁在线、哪个域名通向谁"的真相来源：控制面往里写，
//! HTTP 入口从里读。全在内存里——重启后客户端会重连并重新开隧道，不需要持久化。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use cy_proto::mux::MuxHandle;
use cy_proto::TunnelKind;
use tokio_util::sync::CancellationToken;

/// 一条活动的客户端连接。
pub struct Session {
    pub id: String,
    pub user: String,
    /// 用来给这个客户端开数据流——HTTP 入口收到请求时就靠它
    pub mux: MuxHandle,
    /// 取消它 = 断开这个客户端（踢人、吊销时用）
    pub cancel: CancellationToken,
    /// 发消息给这个客户端的出口
    pub outbox: tokio::sync::mpsc::Sender<cy_proto::ServerMsg>,
}

impl Session {
    /// 断开这个会话。
    pub fn disconnect(&self) {
        self.cancel.cancel();
    }
}

/// 一条已开通的隧道。
#[derive(Clone)]
pub struct Tunnel {
    pub session: Arc<Session>,
    /// 客户端侧的隧道 ID，写进数据流头，客户端据此知道该转发到哪个本地端口
    pub tunnel_id: String,
    pub name: String,
    pub kind: TunnelKind,
    /// 访问口令（`user:pass`），入口处校验；`None` 表示不设防
    pub auth: Option<String>,
    /// TCP 隧道占用的公网端口。关闭隧道时要还回池子。
    pub tcp_port: Option<u16>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Opened,
    /// 这个主机名已经被占用了（可能是别人，也可能是自己在另一台机器上开的同名隧道）
    Taken,
    /// 超出该用户的隧道数量上限
    LimitReached,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Arc<Session>>,
    /// 主机名（小写）→ 隧道。HTTP 入口每个请求查一次。
    hosts: HashMap<String, Tunnel>,
    /// 用户 → 他的会话 ID 们。一个人可能同时在台式机和笔记本上登录。
    by_user: HashMap<String, HashSet<String>>,
}

pub struct Registry {
    inner: RwLock<Inner>,
    next_session: AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            next_session: AtomicU64::new(1),
        }
    }

    /// 分配一个会话 ID。够用来在审计日志里把一次连接的前后行为串起来。
    pub fn new_session_id(&self) -> String {
        format!("s{}", self.next_session.fetch_add(1, Ordering::Relaxed))
    }

    pub fn register(&self, session: Arc<Session>) {
        let mut inner = self.write();
        inner
            .by_user
            .entry(session.user.clone())
            .or_default()
            .insert(session.id.clone());
        inner.sessions.insert(session.id.clone(), session);
    }

    /// 会话结束：连同它开的所有隧道一起移除。
    ///
    /// 按 session id 精确匹配来删隧道，而不是按主机名遍历——同一个人在另一台机器上
    /// 重连时可能已经接管了同名主机，不能把新会话的隧道误删。
    pub fn unregister(&self, session_id: &str) {
        let mut inner = self.write();
        if let Some(session) = inner.sessions.remove(session_id) {
            if let Some(ids) = inner.by_user.get_mut(&session.user) {
                ids.remove(session_id);
                if ids.is_empty() {
                    inner.by_user.remove(&session.user);
                }
            }
            inner.hosts.retain(|_, t| t.session.id != session_id);
        }
    }

    /// 开一条隧道，占住这个主机名。
    pub fn open_tunnel(&self, host: &str, tunnel: Tunnel, max_tunnels: u32) -> OpenOutcome {
        let host = host.to_ascii_lowercase();
        let mut inner = self.write();

        if inner.hosts.contains_key(&host) {
            return OpenOutcome::Taken;
        }

        let used = inner
            .hosts
            .values()
            .filter(|t| t.session.user == tunnel.session.user)
            .count();
        if used >= max_tunnels as usize {
            return OpenOutcome::LimitReached;
        }

        inner.hosts.insert(host, tunnel);
        OpenOutcome::Opened
    }

    /// 关掉某个会话名下、指定 ID 的隧道，返回它占用的主机名。
    pub fn close_tunnel(&self, session_id: &str, tunnel_id: &str) -> Option<String> {
        let mut inner = self.write();
        let host = inner
            .hosts
            .iter()
            .find(|(_, t)| t.session.id == session_id && t.tunnel_id == tunnel_id)
            .map(|(h, _)| h.clone())?;
        inner.hosts.remove(&host);
        Some(host)
    }

    /// HTTP 入口的路由查询。
    /// 记下这条隧道借到的公网端口。
    pub fn set_tcp_port(&self, host: &str, port: u16) {
        let host = host.to_ascii_lowercase();
        if let Some(t) = self.write().hosts.get_mut(&host) {
            t.tcp_port = Some(port);
        }
    }

    /// 某条隧道占用的公网端口（TCP 隧道才有）。
    pub fn tcp_port_of(&self, session_id: &str, tunnel_id: &str) -> Option<u16> {
        self.read()
            .hosts
            .values()
            .find(|t| t.session.id == session_id && t.tunnel_id == tunnel_id)
            .and_then(|t| t.tcp_port)
    }

    /// 某个会话占用的所有公网端口。会话结束时要一并归还。
    pub fn tcp_ports_of_session(&self, session_id: &str) -> Vec<u16> {
        self.read()
            .hosts
            .values()
            .filter(|t| t.session.id == session_id)
            .filter_map(|t| t.tcp_port)
            .collect()
    }

    pub fn lookup(&self, host: &str) -> Option<Tunnel> {
        self.read().hosts.get(&host.to_ascii_lowercase()).cloned()
    }

    /// 踢掉某个用户的所有连接，返回踢了几个。
    ///
    /// 只发取消信号；真正的清理由各自的连接任务在退出时做（[`unregister`](Self::unregister)）。
    pub fn kick_user(&self, user: &str) -> usize {
        let inner = self.read();
        let Some(ids) = inner.by_user.get(user) else {
            return 0;
        };
        let mut kicked = 0;
        for id in ids {
            if let Some(s) = inner.sessions.get(id) {
                s.disconnect();
                kicked += 1;
            }
        }
        kicked
    }

    pub fn session_count(&self) -> usize {
        self.read().sessions.len()
    }

    pub fn tunnel_count(&self) -> usize {
        self.read().hosts.len()
    }

    /// 当前所有隧道的 (主机名, 用户, 隧道名)，供管理接口展示。
    pub fn list_tunnels(&self) -> Vec<(String, String, String)> {
        let mut list: Vec<_> = self
            .read()
            .hosts
            .iter()
            .map(|(h, t)| (h.clone(), t.session.user.clone(), t.name.clone()))
            .collect();
        list.sort();
        list
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_session(reg: &Registry, user: &str) -> Arc<Session> {
        // 造一个已经断开的 mux：注册表本身不碰它，只有数据面才会用
        let (a, _b) = tokio::io::duplex(64);
        let (mux, _inbound) = cy_proto::mux::spawn(
            tokio_util::compat::TokioAsyncReadCompatExt::compat(a),
            yamux::Mode::Client,
        );
        let (outbox, _rx) = tokio::sync::mpsc::channel(8);
        Arc::new(Session {
            id: reg.new_session_id(),
            user: user.into(),
            mux,
            cancel: CancellationToken::new(),
            outbox,
        })
    }

    fn tunnel(session: Arc<Session>, name: &str) -> Tunnel {
        Tunnel {
            session,
            tunnel_id: format!("t-{name}"),
            name: name.into(),
            kind: TunnelKind::Http,
            auth: None,
            tcp_port: None,
        }
    }

    #[tokio::test]
    async fn open_and_route() {
        let reg = Registry::new();
        let s = fake_session(&reg, "zhangsan");
        reg.register(s.clone());

        assert_eq!(
            reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(s.clone(), "wx"), 10),
            OpenOutcome::Opened
        );
        assert!(reg.lookup("zhangsan-wx.t.example.com").is_some());
        // 查询对大小写不敏感——Host 头里的大小写不该影响路由
        assert!(reg.lookup("ZhangSan-WX.T.Example.Com").is_some());
        assert!(reg.lookup("nobody.t.example.com").is_none());
    }

    #[tokio::test]
    async fn same_host_cannot_be_opened_twice() {
        let reg = Registry::new();
        let a = fake_session(&reg, "zhangsan");
        let b = fake_session(&reg, "zhangsan"); // 同一个人的另一台机器
        reg.register(a.clone());
        reg.register(b.clone());

        assert_eq!(
            reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(a, "wx"), 10),
            OpenOutcome::Opened
        );
        // 第二台机器开同名隧道要被拒——否则谁最后开的谁截流量，行为不可预测
        assert_eq!(
            reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(b, "wx"), 10),
            OpenOutcome::Taken
        );
    }

    #[tokio::test]
    async fn respects_per_user_limit() {
        let reg = Registry::new();
        let s = fake_session(&reg, "zhangsan");
        reg.register(s.clone());

        assert_eq!(
            reg.open_tunnel("zhangsan-a.t.example.com", tunnel(s.clone(), "a"), 2),
            OpenOutcome::Opened
        );
        assert_eq!(
            reg.open_tunnel("zhangsan-b.t.example.com", tunnel(s.clone(), "b"), 2),
            OpenOutcome::Opened
        );
        assert_eq!(
            reg.open_tunnel("zhangsan-c.t.example.com", tunnel(s, "c"), 2),
            OpenOutcome::LimitReached
        );
    }

    #[tokio::test]
    async fn limit_counts_per_user_not_globally() {
        let reg = Registry::new();
        let a = fake_session(&reg, "zhangsan");
        let b = fake_session(&reg, "lisi");
        reg.register(a.clone());
        reg.register(b.clone());

        reg.open_tunnel("zhangsan-x.t.example.com", tunnel(a, "x"), 1);
        // 李四的额度不该被张三用掉
        assert_eq!(
            reg.open_tunnel("lisi-x.t.example.com", tunnel(b, "x"), 1),
            OpenOutcome::Opened
        );
    }

    #[tokio::test]
    async fn unregister_takes_tunnels_with_it() {
        let reg = Registry::new();
        let s = fake_session(&reg, "zhangsan");
        reg.register(s.clone());
        reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(s.clone(), "wx"), 10);

        reg.unregister(&s.id);
        assert!(reg.lookup("zhangsan-wx.t.example.com").is_none());
        assert_eq!(reg.session_count(), 0);
    }

    #[tokio::test]
    async fn stale_session_cleanup_does_not_evict_the_new_one() {
        // 张三笔记本断线重连：新会话接管同一个主机名，旧会话随后才清理。
        // 如果按主机名删，就会把刚接管的新隧道误删掉。
        let reg = Registry::new();
        let old = fake_session(&reg, "zhangsan");
        let new = fake_session(&reg, "zhangsan");
        reg.register(old.clone());
        reg.register(new.clone());

        reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(old.clone(), "wx"), 10);
        reg.close_tunnel(&old.id, "t-wx");
        reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(new.clone(), "wx"), 10);

        reg.unregister(&old.id);

        let routed = reg
            .lookup("zhangsan-wx.t.example.com")
            .expect("新隧道应还在");
        assert_eq!(routed.session.id, new.id);
    }

    #[tokio::test]
    async fn kick_signals_every_session_of_that_user() {
        let reg = Registry::new();
        let a = fake_session(&reg, "zhangsan");
        let b = fake_session(&reg, "zhangsan");
        let other = fake_session(&reg, "lisi");
        reg.register(a.clone());
        reg.register(b.clone());
        reg.register(other.clone());

        assert_eq!(reg.kick_user("zhangsan"), 2);
        assert!(a.cancel.is_cancelled());
        assert!(b.cancel.is_cancelled());
        assert!(!other.cancel.is_cancelled(), "不该殃及其他用户");
        assert_eq!(reg.kick_user("nobody"), 0);
    }

    #[tokio::test]
    async fn close_tunnel_only_touches_its_own_session() {
        let reg = Registry::new();
        let a = fake_session(&reg, "zhangsan");
        let b = fake_session(&reg, "lisi");
        reg.register(a.clone());
        reg.register(b.clone());
        reg.open_tunnel("zhangsan-wx.t.example.com", tunnel(a.clone(), "wx"), 10);
        reg.open_tunnel("lisi-wx.t.example.com", tunnel(b.clone(), "wx"), 10);

        // 用别人的会话 ID 关不掉我的隧道
        assert_eq!(
            reg.close_tunnel(&b.id, "t-wx"),
            Some("lisi-wx.t.example.com".into())
        );
        assert!(reg.lookup("zhangsan-wx.t.example.com").is_some());
    }
}
