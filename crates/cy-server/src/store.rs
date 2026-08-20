//! SQLite 存储：用户凭证、自定义域名、审计日志。
//!
//! 规模就是几十个人、每天几百条审计，SQLite 完全够用，还省掉一个要运维的进程。
//! rusqlite 是同步 API，所以每次访问都放进 `spawn_blocking`，别把异步运行时的
//! 工作线程堵住。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("用户 {0} 不存在")]
    NoSuchUser(String),
    #[error("用户 {0} 已存在")]
    UserExists(String),
    #[error("域名 {0} 已被其他用户绑定")]
    DomainTaken(String),
    #[error("{0}")]
    Invalid(String),
}

/// 凭证校验的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Ok {
        user: String,
        max_tunnels: u32,
    },
    /// 格式不对、或查无此人、或哈希对不上——对外一律报"无效"，
    /// 不区分是哪种，免得帮攻击者确认用户名是否存在。
    Invalid,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    pub name: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub max_tunnels: u32,
}

impl UserRow {
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|exp| exp > now)
    }
}

/// 一条审计事件。
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub user: String,
    pub action: &'static str,
    pub tunnel_name: Option<String>,
    pub public: Option<String>,
    pub peer_ip: Option<String>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl AuditEvent {
    pub fn new(user: impl Into<String>, action: &'static str) -> Self {
        Self {
            user: user.into(),
            action,
            tunnel_name: None,
            public: None,
            peer_ip: None,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    pub fn tunnel(mut self, name: impl Into<String>, public: impl Into<String>) -> Self {
        self.tunnel_name = Some(name.into());
        self.public = Some(public.into());
        self
    }

    pub fn peer(mut self, ip: impl Into<String>) -> Self {
        self.peer_ip = Some(ip.into());
        self
    }

    pub fn bytes(mut self, r#in: u64, out: u64) -> Self {
        self.bytes_in = r#in;
        self.bytes_out = out;
        self
    }
}

pub mod action {
    pub const OPEN: &str = "open";
    pub const CLOSE: &str = "close";
    pub const LOGIN: &str = "login";
    pub const KICK: &str = "kick";
    pub const AUTH_FAIL: &str = "auth_fail";
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// 打开（必要时创建）数据库。
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// 内存库，仅供测试。
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        // WAL：读写不互相阻塞。隧道开关（写）和入口路由（读）是并发的。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id           INTEGER PRIMARY KEY,
                name         TEXT    NOT NULL UNIQUE,
                token_sha256 TEXT    NOT NULL,
                created_at   INTEGER NOT NULL,
                expires_at   INTEGER,
                revoked_at   INTEGER,
                max_tunnels  INTEGER NOT NULL DEFAULT 10
            );
            -- 每次握手都按哈希查一次，建索引
            CREATE INDEX IF NOT EXISTS idx_users_token ON users(token_sha256);

            CREATE TABLE IF NOT EXISTS custom_domains (
                domain     TEXT    PRIMARY KEY,
                user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS audit (
                id          INTEGER PRIMARY KEY,
                ts          INTEGER NOT NULL,
                user        TEXT    NOT NULL,
                action      TEXT    NOT NULL,
                tunnel_name TEXT,
                public      TEXT,
                peer_ip     TEXT,
                bytes_in    INTEGER NOT NULL DEFAULT 0,
                bytes_out   INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit(ts);
            CREATE INDEX IF NOT EXISTS idx_audit_user ON audit(user, ts);
            "#,
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 在阻塞线程池里执行一段数据库操作。
    async fn run<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().expect("数据库锁被毒化");
            f(&mut guard)
        })
        .await
        .expect("数据库任务 panic")
    }

    /// 新建用户，返回**只会出现这一次**的明文凭证。
    ///
    /// 库里只存哈希，所以凭证丢了只能重新签发——这是有意的，
    /// 拿到数据库的人不该能顺手拿到所有人的凭证。
    pub async fn add_user(
        &self,
        name: impl Into<String>,
        expire_days: Option<u32>,
        max_tunnels: u32,
    ) -> Result<String, StoreError> {
        let name = name.into();
        cy_proto::naming::validate_user(&name)
            .map_err(|code| StoreError::Invalid(cy_proto::error::human(code).to_string()))?;

        let token = generate_token(&name);
        let hash = hash_token(&token);
        let now = unix_now();
        let expires_at = expire_days.map(|d| now + i64::from(d) * 86_400);
        let insert_name = name.clone();

        self.run(move |conn| {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM users WHERE name = ?1",
                    params![insert_name],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if exists {
                return Err(StoreError::UserExists(insert_name));
            }
            conn.execute(
                "INSERT INTO users (name, token_sha256, created_at, expires_at, max_tunnels)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![insert_name, hash, now, expires_at, max_tunnels],
            )?;
            Ok(())
        })
        .await?;

        Ok(token)
    }

    /// 吊销用户。调用方随后应把该用户的活动连接踢掉——吊销要立刻生效，
    /// 不能等到下次握手。
    pub async fn revoke_user(&self, name: impl Into<String>) -> Result<(), StoreError> {
        let name = name.into();
        let now = unix_now();
        self.run(move |conn| {
            let n = conn.execute(
                "UPDATE users SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
                params![now, name],
            )?;
            if n == 0 {
                // 要么没这个人，要么早就吊销了；后者算幂等成功
                let exists: bool = conn
                    .query_row("SELECT 1 FROM users WHERE name = ?1", params![name], |_| {
                        Ok(true)
                    })
                    .optional()?
                    .unwrap_or(false);
                if !exists {
                    return Err(StoreError::NoSuchUser(name));
                }
            }
            Ok(())
        })
        .await
    }

    pub async fn list_users(&self) -> Result<Vec<UserRow>, StoreError> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name, created_at, expires_at, revoked_at, max_tunnels
                 FROM users ORDER BY name",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(UserRow {
                        name: r.get(0)?,
                        created_at: r.get(1)?,
                        expires_at: r.get(2)?,
                        revoked_at: r.get(3)?,
                        max_tunnels: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// 校验凭证。
    pub async fn authenticate(&self, token: &str) -> Result<Auth, StoreError> {
        let hash = hash_token(token);
        let now = unix_now();
        self.run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT name, created_at, expires_at, revoked_at, max_tunnels
                     FROM users WHERE token_sha256 = ?1",
                    params![hash],
                    |r| {
                        Ok(UserRow {
                            name: r.get(0)?,
                            created_at: r.get(1)?,
                            expires_at: r.get(2)?,
                            revoked_at: r.get(3)?,
                            max_tunnels: r.get(4)?,
                        })
                    },
                )
                .optional()?;

            Ok(match row {
                None => Auth::Invalid,
                Some(u) if u.revoked_at.is_some() => Auth::Revoked,
                Some(u) if u.expires_at.is_some_and(|e| e <= now) => Auth::Expired,
                Some(u) => Auth::Ok {
                    user: u.name,
                    max_tunnels: u.max_tunnels,
                },
            })
        })
        .await
    }

    /// 登记一个自定义域名。同一个域名不能被两个人抢——否则谁先开隧道谁截流量。
    pub async fn add_custom_domain(
        &self,
        user: impl Into<String>,
        domain: impl Into<String>,
    ) -> Result<(), StoreError> {
        let user = user.into();
        let domain = domain.into().to_ascii_lowercase();
        let now = unix_now();
        self.run(move |conn| {
            let user_id: Option<i64> = conn
                .query_row("SELECT id FROM users WHERE name = ?1", params![user], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(user_id) = user_id else {
                return Err(StoreError::NoSuchUser(user));
            };

            let owner: Option<i64> = conn
                .query_row(
                    "SELECT user_id FROM custom_domains WHERE domain = ?1",
                    params![domain],
                    |r| r.get(0),
                )
                .optional()?;
            match owner {
                Some(id) if id == user_id => return Ok(()), // 重复登记，幂等
                Some(_) => return Err(StoreError::DomainTaken(domain)),
                None => {}
            }

            conn.execute(
                "INSERT INTO custom_domains (domain, user_id, created_at) VALUES (?1, ?2, ?3)",
                params![domain, user_id, now],
            )?;
            Ok(())
        })
        .await
    }

    /// 查一个自定义域名归谁。
    pub async fn custom_domain_owner(
        &self,
        domain: impl Into<String>,
    ) -> Result<Option<String>, StoreError> {
        let domain = domain.into().to_ascii_lowercase();
        self.run(move |conn| {
            let owner = conn
                .query_row(
                    "SELECT u.name FROM custom_domains d
                     JOIN users u ON u.id = d.user_id
                     WHERE d.domain = ?1",
                    params![domain],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            Ok(owner)
        })
        .await
    }

    /// 写一条审计。失败只记日志不影响主流程——审计不该拖垮隧道。
    pub async fn audit(&self, event: AuditEvent) {
        let now = unix_now();
        let result = self
            .run(move |conn| {
                conn.execute(
                    "INSERT INTO audit (ts, user, action, tunnel_name, public, peer_ip, bytes_in, bytes_out)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        now,
                        event.user,
                        event.action,
                        event.tunnel_name,
                        event.public,
                        event.peer_ip,
                        event.bytes_in as i64,
                        event.bytes_out as i64,
                    ],
                )?;
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "写审计日志失败");
        }
    }

    /// 最近的审计记录（新的在前），供管理接口查看。
    pub async fn recent_audit(&self, limit: u32) -> Result<Vec<(i64, String, String)>, StoreError> {
        self.run(move |conn| {
            let mut stmt = conn
                .prepare("SELECT ts, user, action FROM audit ORDER BY ts DESC, id DESC LIMIT ?1")?;
            let rows = stmt
                .query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}

/// 生成凭证：`cy_{user}_{32 位十六进制}`。
///
/// 前缀带用户名是为了在日志里一眼看出是谁的（只打印前缀部分，见调用处），
/// 随机段用 128 位——足够长，不需要再套慢哈希。
fn generate_token(user: &str) -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    format!("cy_{user}_{}", hex::encode(bytes))
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// 凭证的可打印形式：只留前缀和随机段的头几位，够定位又不泄露。
pub fn token_hint(token: &str) -> String {
    let cut = token.len().min(token.rfind('_').map_or(0, |i| i + 4));
    format!("{}…", &token[..cut])
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        Store::in_memory().unwrap()
    }

    #[tokio::test]
    async fn add_then_authenticate() {
        let s = store().await;
        let token = s.add_user("zhangsan", None, 10).await.unwrap();
        assert!(token.starts_with("cy_zhangsan_"));

        assert_eq!(
            s.authenticate(&token).await.unwrap(),
            Auth::Ok {
                user: "zhangsan".into(),
                max_tunnels: 10
            }
        );
    }

    #[tokio::test]
    async fn wrong_token_is_invalid() {
        let s = store().await;
        s.add_user("zhangsan", None, 10).await.unwrap();
        assert_eq!(
            s.authenticate("cy_zhangsan_deadbeef").await.unwrap(),
            Auth::Invalid
        );
    }

    #[tokio::test]
    async fn revoked_user_is_rejected() {
        let s = store().await;
        let token = s.add_user("zhangsan", None, 10).await.unwrap();
        s.revoke_user("zhangsan").await.unwrap();
        assert_eq!(s.authenticate(&token).await.unwrap(), Auth::Revoked);
    }

    #[tokio::test]
    async fn expired_user_is_rejected() {
        let s = store().await;
        let token = s.add_user("zhangsan", Some(0), 10).await.unwrap();
        // expire_days = 0 意味着 expires_at == 创建时刻，已经到期
        assert_eq!(s.authenticate(&token).await.unwrap(), Auth::Expired);
    }

    #[tokio::test]
    async fn plaintext_token_is_never_stored() {
        let s = store().await;
        let token = s.add_user("zhangsan", None, 10).await.unwrap();
        let leaked = s
            .run(move |conn| {
                let hits: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM users WHERE token_sha256 LIKE '%' || ?1 || '%'",
                    params![token],
                    |r| r.get(0),
                )?;
                Ok(hits)
            })
            .await
            .unwrap();
        assert_eq!(leaked, 0, "库里不该出现明文凭证");
    }

    #[tokio::test]
    async fn duplicate_user_is_rejected() {
        let s = store().await;
        s.add_user("zhangsan", None, 10).await.unwrap();
        assert!(matches!(
            s.add_user("zhangsan", None, 10).await,
            Err(StoreError::UserExists(_))
        ));
    }

    #[tokio::test]
    async fn reserved_name_cannot_be_a_user() {
        let s = store().await;
        assert!(matches!(
            s.add_user("admin", None, 10).await,
            Err(StoreError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn revoke_is_idempotent_but_unknown_user_errors() {
        let s = store().await;
        s.add_user("zhangsan", None, 10).await.unwrap();
        s.revoke_user("zhangsan").await.unwrap();
        s.revoke_user("zhangsan").await.unwrap(); // 再来一次不报错
        assert!(matches!(
            s.revoke_user("nobody").await,
            Err(StoreError::NoSuchUser(_))
        ));
    }

    #[tokio::test]
    async fn custom_domain_cannot_be_hijacked() {
        let s = store().await;
        s.add_user("zhangsan", None, 10).await.unwrap();
        s.add_user("lisi", None, 10).await.unwrap();

        s.add_custom_domain("zhangsan", "Demo.Example.Com")
            .await
            .unwrap();
        // 大小写归一后能查到
        assert_eq!(
            s.custom_domain_owner("demo.example.com").await.unwrap(),
            Some("zhangsan".into())
        );
        // 别人抢不走
        assert!(matches!(
            s.add_custom_domain("lisi", "demo.example.com").await,
            Err(StoreError::DomainTaken(_))
        ));
        // 本人重复登记是幂等的
        s.add_custom_domain("zhangsan", "demo.example.com")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn audit_records_are_readable_back() {
        let s = store().await;
        s.audit(
            AuditEvent::new("zhangsan", action::OPEN)
                .tunnel("wx", "https://zhangsan-wx.t.example.com")
                .peer("1.2.3.4"),
        )
        .await;
        let rows = s.recent_audit(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "zhangsan");
        assert_eq!(rows[0].2, action::OPEN);
    }

    #[test]
    fn token_hint_keeps_enough_to_identify_but_not_to_use() {
        let hint = token_hint("cy_zhangsan_0123456789abcdef0123456789abcdef");
        assert!(hint.starts_with("cy_zhangsan_012"));
        assert!(!hint.contains("abcdef0123"), "不该泄露完整随机段: {hint}");
    }
}
