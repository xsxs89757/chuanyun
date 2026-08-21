#!/bin/sh
#
# 穿云服务端一键安装。
#
#   curl -fsSL https://raw.githubusercontent.com/xsxs89757/chuanyun/main/scripts/install-server.sh \
#     | sudo sh -s -- --domain t.example.com
#
# 再跑一次就是升级：二进制换新，配置和数据都留着。
#
# 用 POSIX sh 写（不是 bash）：curl | sh 里的 sh 是什么由系统说了算，
# Debian 系上它是 dash，bash 那套语法在那里会挂。
set -eu

REPO="${CHUANYUN_REPO:-xsxs89757/chuanyun}"
BIN_DIR="${CHUANYUN_BIN_DIR:-/usr/local/bin}"
CONF_DIR=/etc/chuanyun
CONF="$CONF_DIR/server.toml"
DATA_DIR=/var/lib/chuanyun
SVC_USER=chuanyun
SVC=chuanyun-server
UNIT="/etc/systemd/system/$SVC.service"

# 内网镜像 / 离线安装：把产物放到自己的 HTTP 服务上，用这个变量指过去。
# 那个目录里要有 chuanyun-server-<版本>-linux-x86_64.tar.gz，有 SHA256SUMS 更好。
# 设了它就必须同时给 --version：版本号查不了 GitHub 了。
BASE_URL="${CHUANYUN_BASE_URL:-}"
VERSION="${CHUANYUN_VERSION:-}"
DOMAIN=""
CONTROL_PORT=7000
ACTION=install
START=1

# ── 输出 ──────────────────────────────────────────────────────────────
# 用 printf 生成真正的转义字符，而不是留着 \033 字面量——
# 后面的说明是 cat 出去的，cat 不解释反斜杠。
if [ -t 1 ]; then
    B=$(printf '\033[1m');   G=$(printf '\033[1;32m')
    Y=$(printf '\033[1;33m'); R=$(printf '\033[1;31m')
    N=$(printf '\033[0m')
else
    B=''; G=''; Y=''; R=''; N=''
fi
step() { printf '\n%s▸ %s%s\n' "$G" "$1" "$N"; }
info() { printf '  %s\n' "$1"; }
warn() { printf '%s! %s%s\n' "$Y" "$1" "$N"; }
die()  { printf '\n%s✗ %s%s\n' "$R" "$1" "$N" >&2; exit 1; }

usage() {
    cat <<EOF
穿云服务端安装脚本

用法：
  curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install-server.sh \\
    | sudo sh -s -- --domain t.example.com

选项：
  --domain <后缀>     隧道域名后缀，如 t.example.com（首次安装必填）
  --version <版本>    装指定版本，如 v0.1.0（默认装最新）
  --control-port <口> 控制通道端口（默认 7000）
  --no-start          装完不启动
  --uninstall         卸载（数据目录会保留）
  -h, --help          看这段

环境变量：
  CHUANYUN_BASE_URL   从内网镜像装（要同时给 --version）

再跑一次就是升级，配置和数据不动。
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --domain)        DOMAIN="${2:-}"; shift 2 ;;
        --version)       VERSION="${2:-}"; shift 2 ;;
        --control-port)  CONTROL_PORT="${2:-}"; shift 2 ;;
        --no-start)      START=0; shift ;;
        --uninstall)     ACTION=uninstall; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               die "不认识的参数：$1（--help 看用法）" ;;
    esac
done

# ── 环境检查 ──────────────────────────────────────────────────────────
[ "$(id -u)" = 0 ] || die "需要 root。在 sh 前面加 sudo：
  curl -fsSL … | sudo sh -s -- --domain 你的域名"

[ "$(uname -s)" = Linux ] || die "服务端只支持 Linux（客户端才有 macOS / Windows 版）"

case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64)
        die "暂时只提供 x86_64 的预编译包。ARM 服务器请从源码构建：

    git clone https://github.com/$REPO.git && cd chuanyun
    cargo build --release -p cy-server --bin chuanyun-server

  然后把 target/release/chuanyun-server 拷到 $BIN_DIR/，其余步骤照 docs/部署.md" ;;
    *) die "不支持的架构：$(uname -m)" ;;
esac

if command -v curl >/dev/null 2>&1; then
    fetch()      { curl -fsSL --max-time 60 "$1"; }
    fetch_file() { curl -fsSL --max-time 300 "$1" -o "$2"; }
    fetch_quick() { curl -fsSL --max-time 3 "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch()      { wget -qO- --timeout=60 "$1"; }
    fetch_file() { wget -qO "$2" --timeout=300 "$1"; }
    fetch_quick() { wget -qO- --timeout=3 --tries=1 "$1"; }
else
    die "需要 curl 或 wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    sha256() { echo ""; }
fi

HAS_SYSTEMD=0
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    HAS_SYSTEMD=1
fi

# ── 卸载 ──────────────────────────────────────────────────────────────
if [ "$ACTION" = uninstall ]; then
    step "卸载"
    if [ "$HAS_SYSTEMD" = 1 ] && [ -f "$UNIT" ]; then
        systemctl disable --now "$SVC" >/dev/null 2>&1 || true
        rm -f "$UNIT"
        systemctl daemon-reload
        info "服务已停止并移除"
    fi
    rm -f "$BIN_DIR/chuanyun-server"
    info "二进制已删除"
    printf '\n%s✓ 卸载完成%s\n\n' "$G" "$N"
    info "配置和数据故意留着，确认不要了再删："
    info "  rm -rf $CONF_DIR $DATA_DIR && userdel $SVC_USER"
    echo
    exit 0
fi

# ── 首次安装需要域名 ──────────────────────────────────────────────────
FIRST_INSTALL=1
[ -f "$CONF" ] && FIRST_INSTALL=0

if [ "$FIRST_INSTALL" = 1 ] && [ -z "$DOMAIN" ]; then
    die "首次安装要指定隧道域名后缀，比如：

    curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install-server.sh \\
      | sudo sh -s -- --domain t.example.com

  这个域名要能加泛解析（*.t.example.com 指向本机），隧道地址会长成
  https://张三-api.t.example.com。用独立子域，别用主域。"
fi

# ── 找版本 ────────────────────────────────────────────────────────────
step "确认版本"
if [ -z "$VERSION" ]; then
    [ -z "$BASE_URL" ] || die "用了 CHUANYUN_BASE_URL 就必须同时给 --version（这边查不到有哪些版本）"
    VERSION=$(fetch "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1) || true
    [ -n "$VERSION" ] || die "查不到最新版本（网络不通？）。可以用 --version v0.1.0 指定，
  版本号看 https://github.com/$REPO/releases"
fi
NUM="${VERSION#v}"
TARBALL="chuanyun-server-$NUM-linux-$ARCH.tar.gz"
if [ -n "$BASE_URL" ]; then
    BASE="${BASE_URL%/}"
else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
fi
info "$VERSION"

# ── 下载 ──────────────────────────────────────────────────────────────
step "下载"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

fetch_file "$BASE/$TARBALL" "$TMP/$TARBALL" \
    || die "下载失败：$BASE/$TARBALL
  这个版本可能没有 $ARCH 的包，去 https://github.com/$REPO/releases 看看有哪些"
info "$TARBALL"

# 校验：拿得到 SHA256SUMS 就必须对上。宁可装不上，也别装一个被换过的二进制。
if fetch_file "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
    WANT=$(grep " \*\{0,1\}$TARBALL\$" "$TMP/SHA256SUMS" 2>/dev/null | cut -d' ' -f1 || true)
    GOT=$(sha256 "$TMP/$TARBALL")
    if [ -z "$WANT" ]; then
        warn "SHA256SUMS 里没有 $TARBALL 这一项，跳过校验"
    elif [ -z "$GOT" ]; then
        warn "系统里没有 sha256sum，跳过校验"
    elif [ "$WANT" != "$GOT" ]; then
        die "校验和不符，已停止。
  期望 $WANT
  实际 $GOT"
    else
        info "校验和一致"
    fi
else
    warn "这个版本没提供 SHA256SUMS，跳过校验"
fi

tar xzf "$TMP/$TARBALL" -C "$TMP"
[ -f "$TMP/chuanyun-server" ] || die "包里没有 chuanyun-server，下载的可能不是服务端包"

# ── 停旧的 ────────────────────────────────────────────────────────────
WAS_RUNNING=0
if [ "$HAS_SYSTEMD" = 1 ] && systemctl is-active --quiet "$SVC" 2>/dev/null; then
    WAS_RUNNING=1
    step "停止正在运行的服务"
    systemctl stop "$SVC"
fi

# ── 装二进制 ──────────────────────────────────────────────────────────
step "安装"
install -m 0755 "$TMP/chuanyun-server" "$BIN_DIR/chuanyun-server"
info "$BIN_DIR/chuanyun-server"

# ── 用户与目录 ────────────────────────────────────────────────────────
if ! id "$SVC_USER" >/dev/null 2>&1; then
    NOLOGIN=/usr/sbin/nologin
    [ -x "$NOLOGIN" ] || NOLOGIN=/sbin/nologin
    [ -x "$NOLOGIN" ] || NOLOGIN=/bin/false
    useradd --system --no-create-home --home-dir "$DATA_DIR" --shell "$NOLOGIN" "$SVC_USER" \
        2>/dev/null || useradd -r -M -d "$DATA_DIR" -s "$NOLOGIN" "$SVC_USER"
    info "已创建系统用户 $SVC_USER"
fi

mkdir -p "$CONF_DIR" "$DATA_DIR"
# 数据目录里有自签证书的私钥和 SQLite 库，别让同机器的其他 ssh 用户读到。
# 只改属主不改属组：有些发行版的 useradd 不建同名组，chown user:user 会失败。
chown "$SVC_USER" "$DATA_DIR"
chmod 0700 "$DATA_DIR"

# nginx 样例留一份在机器上，省得再去仓库翻
install -m 0644 "$TMP/nginx.conf.example" "$CONF_DIR/nginx.conf.example" 2>/dev/null || true

# ── 配置 ──────────────────────────────────────────────────────────────
if [ "$FIRST_INSTALL" = 1 ]; then
    step "写配置"
    cat > "$CONF" <<EOF
# 穿云服务端配置
# 完整选项见 https://github.com/$REPO/blob/main/docs/部署.md

[http]
# 隧道域名后缀。子域名拼成 {用户}-{隧道名}.$DOMAIN
# 需要一条泛解析：*.$DOMAIN → 本机 IP
domain_suffix = "$DOMAIN"
# 只监听回环，明文。443 和证书归前置 nginx 管，不跟它抢端口。
listen = "127.0.0.1:7080"

[control]
# 客户端出站连这里，这条连接自带 TLS，不经 nginx。
# 记得在防火墙和云安全组里放行这个端口。
listen = "0.0.0.0:$CONTROL_PORT"

[storage]
data_dir = "$DATA_DIR"

# TCP 隧道（连数据库、SSH）要用的话，填一个解析到本机的主机名，
# 再把 port_range 这段端口在防火墙放行。
# [tcp]
# public_host = "$DOMAIN"
# port_range = [20000, 20100]
EOF
    # 配置里没有秘密（域名和端口而已），建组失败就退回 0644
    if chown "root:$SVC_USER" "$CONF" 2>/dev/null; then
        chmod 0640 "$CONF"
    else
        chmod 0644 "$CONF"
    fi
    info "$CONF"
else
    info "配置已存在，没有改动：$CONF"
    if [ -n "$DOMAIN" ]; then
        warn "--domain 被忽略了（要改域名请直接编辑 $CONF 再重启服务）"
    fi
fi

# ── systemd ───────────────────────────────────────────────────────────
if [ "$HAS_SYSTEMD" = 1 ]; then
    step "注册系统服务"
    if [ -f "$UNIT" ] && ! cmp -s "$TMP/chuanyun-server.service" "$UNIT"; then
        cp "$UNIT" "$UNIT.bak"
        # 新版本自己改了 unit 也会走到这里，所以别写成「你改过它」
        warn "unit 文件有变化，旧的备份在 $UNIT.bak"
        warn "手改过的话改动不会保留——建议改用 drop-in：systemctl edit $SVC"
    fi
    install -m 0644 "$TMP/chuanyun-server.service" "$UNIT"
    systemctl daemon-reload

    if [ "$START" = 1 ]; then
        systemctl enable "$SVC" >/dev/null 2>&1 || true
        systemctl restart "$SVC"

        # 等它真起来。起不来就直接把日志摆出来，不用用户再去查一遍。
        i=0
        while [ "$i" -lt 25 ]; do
            systemctl is-active --quiet "$SVC" && break
            i=$((i + 1))
            sleep 0.2
        done
        if systemctl is-active --quiet "$SVC"; then
            info "已启动，并设为开机自启"
        else
            printf '\n%s✗ 服务没能起来%s\n\n' "$R" "$N"
            journalctl -u "$SVC" -n 20 --no-pager 2>/dev/null | sed 's/^/  /'
            echo
            die "看上面的日志。改完配置后重启：systemctl restart $SVC"
        fi
    else
        info "已注册（带了 --no-start，没有启动）"
    fi
else
    warn "这台机器没有 systemd，跳过服务注册。手动跑："
    info "  $BIN_DIR/chuanyun-server -c $CONF run"
fi

# ── 装完了，接下来干嘛 ────────────────────────────────────────────────
if [ "$FIRST_INSTALL" = 0 ]; then
    printf '\n%s✓ 升级完成，现在是 %s%s\n\n' "$G" "$VERSION" "$N"
    if [ "$WAS_RUNNING" = 1 ]; then
        info "服务已重启，隧道会在几秒内自动恢复，客户端那边不用管"
        echo
    fi
    exit 0
fi

SUFFIX=$(sed -n 's/^domain_suffix[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$CONF" | head -1)

# 公网 IP：云服务器的网卡上通常是内网地址，本地路由查出来是错的，所以先问外部。
IP=$(fetch_quick https://api.ipify.org 2>/dev/null || true)
if [ -z "$IP" ]; then
    IP=$(fetch_quick https://4.ipw.cn 2>/dev/null || true)
fi
if [ -z "$IP" ]; then
    IP="你的服务器公网IP"
fi

FP=""
if [ "$START" = 1 ] && [ "$HAS_SYSTEMD" = 1 ]; then
    FP=$("$BIN_DIR/chuanyun-server" -c "$CONF" fingerprint 2>/dev/null || true)
fi
[ -n "$FP" ] || FP="（服务起来后跑 chuanyun-server fingerprint 查看）"

printf '\n%s✓ 安装完成%s\n' "$G" "$N"

cat <<EOF

${B}还差四步才能用${N}

${B}1. 域名解析${N}
   加一条泛解析记录：

     *.$SUFFIX    A    $IP

${B}2. 防火墙放行控制通道${N}
   客户端出站连这个端口，这条连接不经 nginx：

     ufw allow $CONTROL_PORT/tcp
     # 或 firewall-cmd --add-port=$CONTROL_PORT/tcp --permanent && firewall-cmd --reload

   ${Y}云服务器还要去控制台的安全组里同样放行，只开 ufw 不够。${N}

${B}3. nginx 反代${N}
   服务端只听 127.0.0.1:7080 的明文，443 和证书还是归 nginx。
   样例已经放在这台机器上了：

     $CONF_DIR/nginx.conf.example

   四个要点：透传 Host 头、透传 Upgrade（WebSocket 要用）、
   proxy_buffering off（否则 SSE 不实时）、client_max_body_size 调大（传文件）。

   宝塔面板要放进 /www/server/panel/vhost/nginx/ 下的独立 conf，
   别直接改面板生成的站点配置——面板重写时会盖掉。

${B}4. 给同事发凭证${N}

     chuanyun-server user add 张三

   凭证只显示一次。把它和下面这串证书指纹一起发给本人：

     ${B}$FP${N}

${B}常用命令${N}
  systemctl status $SVC      服务在不在跑
  journalctl -u $SVC -f      看日志
  chuanyun-server status              谁在线、开了几条隧道
  chuanyun-server user list           谁有凭证、过没过期
  chuanyun-server user add <名字>     再发一个人
  chuanyun-server fingerprint         再看一次证书指纹

完整说明：https://github.com/$REPO/blob/main/docs/部署.md

EOF
