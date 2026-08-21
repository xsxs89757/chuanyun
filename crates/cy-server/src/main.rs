//! `chuanyun-server` 命令行入口。
//!
//! 薄壳：解析参数、初始化日志，剩下的都在 `cy_server` 库里。
//!
//! 管理命令有两条路径——服务在跑就走本机管理接口（踢人这类操作要立刻生效），
//! 没在跑就直接改数据库（新同事入职时还没来得及重启服务也能发凭证）。

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use cy_server::{Config, Server, Store};

#[derive(Parser)]
#[command(
    name = "chuanyun-server",
    about = "穿云服务端",
    version,
    long_about = None
)]
struct Cli {
    /// 配置文件路径
    #[arg(
        short,
        long,
        global = true,
        default_value = "/etc/chuanyun/server.toml"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动服务端
    Run,

    /// 用户与凭证管理
    #[command(subcommand)]
    User(UserCommand),

    /// 自定义域名管理
    #[command(subcommand)]
    Domain(DomainCommand),

    /// 把某个用户的所有连接踢下线
    Kick { user: String },

    /// 查看运行状态
    Status,

    /// 打印证书指纹（发客户端凭证时要连它一起给）
    Fingerprint,
}

#[derive(Subcommand)]
enum UserCommand {
    /// 新建用户，输出一次性凭证
    Add {
        name: String,
        /// 有效天数；不填则长期有效
        #[arg(long)]
        expire: Option<u32>,
        /// 最多能同时开几条隧道
        #[arg(long, default_value_t = 10)]
        max_tunnels: u32,
    },
    /// 吊销凭证（立刻断开该用户的连接）
    Revoke { name: String },
    /// 列出所有用户
    List,
}

#[derive(Subcommand)]
enum DomainCommand {
    /// 把一个自定义域名登记给某个用户
    Add { user: String, domain: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cy_server=info".into()),
        )
        .init();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run => serve(&cli.config).await,
        Command::User(cmd) => user_command(&cli.config, cmd).await,
        Command::Domain(DomainCommand::Add { user, domain }) => {
            let store = open_store(&cli.config)?;
            store.add_custom_domain(&user, &domain).await?;
            println!("已把 {domain} 登记给 {user}");
            println!();
            println!("还需要在 nginx 里为这个域名配好证书与反代，指向同一个 HTTP 入口端口。");
            Ok(())
        }
        Command::Kick { user } => {
            let config = Config::load(&cli.config)?;
            let url = format!("http://{}/kick/{}", config.admin.listen, user);
            match ureq_post(&url) {
                Ok(body) => {
                    println!("{body}");
                    Ok(())
                }
                Err(e) => {
                    anyhow::bail!("联系服务端失败（它在运行吗？）: {e}");
                }
            }
        }
        Command::Status => {
            let config = Config::load(&cli.config)?;
            let url = format!("http://{}/status", config.admin.listen);
            match ureq_get(&url) {
                Ok(body) => {
                    println!("{body}");
                    Ok(())
                }
                Err(e) => anyhow::bail!("联系服务端失败（它在运行吗？）: {e}"),
            }
        }
        Command::Fingerprint => {
            let config = Config::load(&cli.config)?;
            let dir = &config.storage.data_dir;
            let cert = cy_server::tls::cert_path(dir);
            // 只读不建：这条命令常以 root 跑，顺手生成会把文件属主搞成 root，
            // 服务下次反而起不来。没有证书说明服务还没启动过。
            if !cert.exists() {
                anyhow::bail!(
                    "{} 里还没有证书——先把服务端跑起来，它会自签一张",
                    dir.display()
                );
            }
            let identity =
                cy_server::tls::Identity::from_pem_files(&cert, &cy_server::tls::key_path(dir))
                    .with_context(|| format!("读取 {} 里的证书", dir.display()))?;
            println!("{}", identity.fingerprint);
            Ok(())
        }
    }
}

async fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config =
        Config::load(config_path).with_context(|| format!("读取配置 {}", config_path.display()))?;

    let handle = Server::start(config).await?;

    println!("穿云服务端已启动");
    println!("  控制通道  {}", handle.control_addr);
    println!("  HTTP 入口 {}", handle.http_addr);
    println!("  管理接口  {}", handle.admin_addr);
    println!();
    println!("客户端要核对的证书指纹：");
    println!("  {}", handle.fingerprint);
    println!();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("收到停止信号，正在收尾…");
        }
        _ = terminate_signal() => {
            println!("收到停止信号，正在收尾…");
        }
    }
    handle.shutdown().await;
    Ok(())
}

#[cfg(unix)]
async fn terminate_signal() {
    let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    sig.recv().await;
}

#[cfg(not(unix))]
async fn terminate_signal() {
    std::future::pending().await
}

async fn user_command(config_path: &std::path::Path, cmd: UserCommand) -> anyhow::Result<()> {
    let store = open_store(config_path)?;

    match cmd {
        UserCommand::Add {
            name,
            expire,
            max_tunnels,
        } => {
            let token = store.add_user(&name, expire, max_tunnels).await?;
            println!("已创建用户 {name}");
            println!();
            println!("凭证（只显示这一次，请立刻发给本人）：");
            println!("  {token}");
            println!();
            println!("凭证在库里只存哈希，丢了只能重新签发。");
        }
        UserCommand::Revoke { name } => {
            store.revoke_user(&name).await?;
            println!("已吊销 {name} 的凭证");
            // 吊销要立刻生效，不能等到对方下次握手
            let config = Config::load(config_path)?;
            let url = format!("http://{}/kick/{}", config.admin.listen, name);
            match ureq_post(&url) {
                Ok(_) => println!("已断开该用户的在线连接"),
                Err(_) => println!("（服务端没在跑，下次启动后该凭证自然失效）"),
            }
        }
        UserCommand::List => {
            let users = store.list_users().await?;
            if users.is_empty() {
                println!("还没有用户。用 `chuanyun-server user add <名字>` 创建。");
                return Ok(());
            }
            println!("{:<16} {:<12} 到期", "用户", "状态");
            for u in users {
                let status = if u.revoked_at.is_some() {
                    "已吊销"
                } else {
                    "正常"
                };
                let expires = match u.expires_at {
                    Some(t) => format_date(t),
                    None => "长期".to_string(),
                };
                println!("{:<16} {:<12} {}", u.name, status, expires);
            }
        }
    }
    Ok(())
}

fn open_store(config_path: &std::path::Path) -> anyhow::Result<Store> {
    let config =
        Config::load(config_path).with_context(|| format!("读取配置 {}", config_path.display()))?;
    std::fs::create_dir_all(&config.storage.data_dir)?;
    let store = Store::open(&config.storage.data_dir.join("chuanyun.db"))?;
    Ok(store)
}

fn format_date(unix: i64) -> String {
    use time::OffsetDateTime;
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| {
            t.format(&time::macros::format_description!("[year]-[month]-[day]"))
                .ok()
        })
        .unwrap_or_else(|| unix.to_string())
}

/// 极简的本机 HTTP 调用。
///
/// 管理命令只需要打本机的两三个接口，为此拉一个 HTTP 客户端库不划算。
fn ureq_get(url: &str) -> anyhow::Result<String> {
    local_http("GET", url)
}

fn ureq_post(url: &str) -> anyhow::Result<String> {
    local_http("POST", url)
}

fn local_http(method: &str, url: &str) -> anyhow::Result<String> {
    use std::io::{Read, Write};

    let rest = url.strip_prefix("http://").context("只支持 http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let mut socket = std::net::TcpStream::connect(authority)?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(
        socket,
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )?;

    let mut raw = String::new();
    socket.read_to_string(&mut raw)?;

    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let status_ok = raw.starts_with("HTTP/1.1 2") || raw.starts_with("HTTP/1.0 2");
    if !status_ok {
        anyhow::bail!("{}", body.trim());
    }
    Ok(body.trim().to_string())
}
