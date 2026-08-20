#!/usr/bin/env bash
#
# 桌面客户端的端到端验证（不需要点界面）。
#
# 界面本身要靠眼睛看，但界面背后那一整套——读状态、自动连上、开隧道、
# 起本地 API——是可以自动验的：给它预置一份登录状态，然后从两头检查结果。
#
#   ./scripts/verify-desktop.sh
#
# 跑完会把状态文件恢复原样，不会让你的客户端留在一个测试服务器上。
set -euo pipefail

cd "$(dirname "$0")/.."

# 状态文件的位置由 directories 决定，不同平台不一样、还带反写域名前缀，
# 所以问应用本身要，别在这儿猜。
STATE_FILE=""
BACKUP=""

WORK="$(mktemp -d)"
PIDS=()

cleanup() {
    set +m
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    # 把用户原来的状态放回去——不能因为跑了个验证就把人家的客户端弄乱
    if [ -n "$STATE_FILE" ]; then
        if [ -n "$BACKUP" ]; then
            mv "$BACKUP" "$STATE_FILE"
        else
            rm -f "$STATE_FILE"
        fi
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n\033[1;32m▸ %s\033[0m\n' "$1"; }
info() { printf '  %s\n' "$1"; }
fail() { printf '\n\033[1;31m✗ %s\033[0m\n' "$1"; exit 1; }

step "编译"
cargo build --quiet -p cy-server --bin chuanyun-server
cargo build --quiet -p cy-desktop

step "起服务端"
mkdir -p "$WORK/data"
cat > "$WORK/server.toml" <<EOF
[control]
listen = "127.0.0.1:17100"
heartbeat_secs = 5
[http]
listen = "127.0.0.1:17180"
domain_suffix = "t.verify.local"
public_scheme = "http"
[admin]
listen = "127.0.0.1:17101"
[storage]
data_dir = "$WORK/data"
EOF

TOKEN=$(target/debug/chuanyun-server -c "$WORK/server.toml" user add zhangsan | grep -o 'cy_zhangsan_[0-9a-f]*')
target/debug/chuanyun-server -c "$WORK/server.toml" run > "$WORK/server.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 60); do grep -q "已启动" "$WORK/server.log" 2>/dev/null && break; sleep 0.1; done
PIN=$(grep -A1 "证书指纹" "$WORK/server.log" | tail -1 | tr -d ' ')
[ -n "$PIN" ] || fail "服务端没起来"
info "服务端就绪，指纹 ${PIN:0:16}…"

step "起一个本地服务"
python3 -c "
import http.server, socketserver
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        b=b'desktop-verify-ok'
        self.send_response(200); self.send_header('Content-Length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self,*a): pass
socketserver.TCPServer.allow_reuse_address=True
socketserver.TCPServer(('127.0.0.1',18100),H).serve_forever()" &
PIDS+=($!)
sleep 0.5

step "预置登录状态，模拟「上次已经登录过」"
STATE_FILE=$(target/debug/chuanyun --print-state-path)
info "配置文件在 $STATE_FILE"
[ -f "$STATE_FILE" ] && { BACKUP="$WORK/state.backup"; cp "$STATE_FILE" "$BACKUP"; info "已备份你原有的状态文件"; }
mkdir -p "$(dirname "$STATE_FILE")"
cat > "$STATE_FILE" <<EOF
{
  "server": "127.0.0.1:17100",
  "token": "$TOKEN",
  "tls_pin": "$PIN",
  "tunnels": { "verify": { "local_port": 18100, "enabled": true } },
  "settings": { "autostart": false, "local_api_port": 7075, "check_updates": false }
}
EOF

step "启动桌面客户端"
target/debug/chuanyun > "$WORK/app.log" 2>&1 &
PIDS+=($!)

step "等它自己连上并恢复隧道"
CONNECTED=""
for _ in $(seq 1 100); do
    RESP=$(curl -s --max-time 2 http://127.0.0.1:7075/api/status 2>/dev/null || true)
    if echo "$RESP" | grep -q '"connected":true'; then CONNECTED=1; break; fi
    sleep 0.2
done
[ -n "$CONNECTED" ] || fail "客户端没能自动连上（看看 $WORK/app.log）"
info "已连接：$(curl -s http://127.0.0.1:7075/api/status)"

step "确认隧道已恢复"
TUNNELS=$(curl -s http://127.0.0.1:7075/api/tunnels)
echo "$TUNNELS" | grep -q 'zhangsan-verify.t.verify.local' \
    || fail "隧道没恢复：$TUNNELS"
info "$TUNNELS"

step "从公网入口访问它"
BODY=$(curl -s --resolve "zhangsan-verify.t.verify.local:17180:127.0.0.1" \
            "http://zhangsan-verify.t.verify.local:17180/")
info "响应：$BODY"
[ "$BODY" = "desktop-verify-ok" ] || fail "响应不对：$BODY"

step "本地 API 的地址解析"
RESOLVED=$(curl -s "http://127.0.0.1:7075/api/resolve?port=18100&plain=1")
info "→ $RESOLVED"
[ "$RESOLVED" = "http://zhangsan-verify.t.verify.local" ] || fail "resolve 不对：$RESOLVED"

printf '\n\033[1;32m✓ 桌面客户端整套跑通\033[0m\n'
echo "  · 读状态文件、自动连上服务端"
echo "  · 恢复上次开着的隧道"
echo "  · 公网地址能访问到本地服务"
echo "  · 本地 API 正常"
echo
echo "界面长什么样还是得亲眼看：cargo run -p cy-desktop"
echo
