//! 无界面的穿云客户端。
//!
//! 用途有三个：给没有图形环境的机器用（CI、服务器）、给排查问题时看日志用、
//! 以及给 `scripts/demo.sh` 当演示主角。
//!
//! ```bash
//! chuanyun-headless --server 127.0.0.1:7000 --token cy_zhangsan_xxx \
//!                   --pin <指纹> --tunnel wx=8082
//! ```

use std::time::Duration;

use cy_core::{Brand, Engine, Event};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cy_core=info".into()),
        )
        .init();

    let args = Args::parse()?;

    // 不落盘：headless 每次都从命令行拿参数，免得在服务器上留下凭证文件
    let engine = Engine::start(
        None,
        Brand {
            default_server: args.server.clone(),
            tls_pin: args.pin.clone(),
            update_url: String::new(),
        },
    );

    let mut events = engine.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                Event::Connected { .. } => println!("已连接"),
                Event::Disconnected { reason } => println!("连接断开：{reason}"),
                Event::TunnelOpened { name, url } => println!("隧道 {name} → {url}"),
                Event::TunnelFailed { name, reason } => println!("隧道 {name} 开通失败：{reason}"),
                Event::Kicked { reason } => println!("被管理员断开：{reason}"),
                Event::AuthRejected { reason } => println!("凭证被拒：{reason}"),
            }
        }
    });

    engine
        .login(&args.server, &args.token, &args.pin)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    for (name, port) in &args.tunnels {
        if let Err(e) = engine.add_tunnel(name, *port).await {
            eprintln!("隧道 {name} 开通失败：{e}");
        }
    }

    if args.local_api {
        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(e) =
                cy_core::local_api::serve(engine, cy_core::local_api::DEFAULT_PORT).await
            {
                eprintln!("本地 API 起不来：{e}");
            }
        });
    }

    // 等 Ctrl-C
    tokio::signal::ctrl_c().await?;
    println!("正在退出…");
    engine.shutdown().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

struct Args {
    server: String,
    token: String,
    pin: String,
    tunnels: Vec<(String, u16)>,
    local_api: bool,
}

impl Args {
    /// 手写参数解析：这个示例不该为了几个参数把 clap 拖进 cy-core 的依赖里。
    fn parse() -> anyhow::Result<Self> {
        let mut server = String::new();
        let mut token = String::new();
        let mut pin = String::new();
        let mut tunnels = Vec::new();
        let mut local_api = false;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--server" => server = args.next().unwrap_or_default(),
                "--token" => token = args.next().unwrap_or_default(),
                "--pin" => pin = args.next().unwrap_or_default(),
                "--local-api" => local_api = true,
                "--tunnel" => {
                    let spec = args.next().unwrap_or_default();
                    let (name, port) = spec
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--tunnel 要写成 名字=端口，收到 {spec}"))?;
                    tunnels.push((name.to_string(), port.parse()?));
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => anyhow::bail!("不认识的参数 {other}（用 --help 看用法）"),
            }
        }

        if server.is_empty() || token.is_empty() {
            print_help();
            anyhow::bail!("--server 和 --token 是必填的");
        }
        Ok(Self {
            server,
            token,
            pin,
            tunnels,
            local_api,
        })
    }
}

fn print_help() {
    println!(
        "穿云 headless 客户端

用法：
  headless --server <地址:端口> --token <凭证> [选项]

选项：
  --pin <指纹>        服务端证书指纹；不填则首次连接时信任对方（仅限可信网络）
  --tunnel 名字=端口   开一条隧道，可重复
  --local-api         同时启动本地 API（127.0.0.1:{port}）
  -h, --help          显示这段说明

例子：
  headless --server tunnel.example.com:7000 --token cy_zhangsan_abc \\
           --tunnel api=8082 --tunnel web=5173",
        port = cy_core::local_api::DEFAULT_PORT
    );
}
