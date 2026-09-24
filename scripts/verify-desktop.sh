#!/usr/bin/env bash
#
# 桌面客户端的端到端验证（不需要点界面）。
#
# 界面本身要靠眼睛看，但界面背后那一整套——读状态、自动连上、开隧道、
# 起本地 API、单实例、退出——是可以自动验的：给它预置一份登录状态，然后从两头检查结果。
#
#   ./scripts/verify-desktop.sh
#
# 全程用临时目录里的配置（CHUANYUN_CONFIG_DIR）和另一个本地 API 端口，
# 不碰你正在用的穿云：它的配置文件、它占着的 7075 都不动。
set -euo pipefail

cd "$(dirname "$0")/.."

WORK="$(mktemp -d)"
PIDS=()
API_PORT=17075

cleanup() {
    set +m
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n\033[1;32m▸ %s\033[0m\n' "$1"; }
info() { printf '  %s\n' "$1"; }
fail() { printf '\n\033[1;31m✗ %s\033[0m\n' "$1"; exit 1; }

api() { curl -s --max-time 2 "http://127.0.0.1:$API_PORT$1"; }
alive() { kill -0 "$1" 2>/dev/null; }

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
export CHUANYUN_CONFIG_DIR="$WORK/config"
STATE_FILE=$(target/debug/chuanyun --print-state-path)
[ "$STATE_FILE" = "$WORK/config/state.json" ] || fail "CHUANYUN_CONFIG_DIR 没生效：$STATE_FILE"
info "配置文件在 $STATE_FILE（临时目录，你自己的那份不受影响）"
mkdir -p "$(dirname "$STATE_FILE")"
cat > "$STATE_FILE" <<EOF
{
  "server": "127.0.0.1:17100",
  "token": "$TOKEN",
  "tls_pin": "$PIN",
  "tunnels": { "verify": { "local_port": 18100, "enabled": true } },
  "settings": { "autostart": false, "local_api_port": $API_PORT, "check_updates": false }
}
EOF

step "启动桌面客户端"
target/debug/chuanyun > "$WORK/app.log" 2>&1 &
APP=$!
PIDS+=($APP)

step "等它自己连上并恢复隧道"
CONNECTED=""
for _ in $(seq 1 100); do
    if api /api/status | grep -q '"connected":true'; then CONNECTED=1; break; fi
    sleep 0.2
done
[ -n "$CONNECTED" ] || fail "客户端没能自动连上（看看 $WORK/app.log）"
info "已连接：$(api /api/status)"

step "确认隧道已恢复"
TUNNELS=$(api /api/tunnels)
echo "$TUNNELS" | grep -q 'zhangsan-verify.t.verify.local' \
    || fail "隧道没恢复：$TUNNELS"
info "$TUNNELS"

step "从公网入口访问它"
BODY=$(curl -s --resolve "zhangsan-verify.t.verify.local:17180:127.0.0.1" \
            "http://zhangsan-verify.t.verify.local:17180/")
info "响应：$BODY"
[ "$BODY" = "desktop-verify-ok" ] || fail "响应不对：$BODY"

step "本地 API 的地址解析"
RESOLVED=$(api "/api/resolve?port=18100&plain=1")
info "→ $RESOLVED"
[ "$RESOLVED" = "http://zhangsan-verify.t.verify.local" ] || fail "resolve 不对：$RESOLVED"

step "再打开一次：应该把已经在跑的那个叫出来，自己退出"
# 关窗只是收进托盘。以前这时再双击快捷方式会起第二个进程，登录成第二个会话，
# 去开同名隧道被服务端拒绝——窗口里全是「已被占用」。
target/debug/chuanyun > "$WORK/second.log" 2>&1 &
SECOND=$!
PIDS+=($SECOND)
for _ in $(seq 1 50); do alive "$SECOND" || break; sleep 0.2; done
alive "$SECOND" && fail "第二个进程没有自己退出（看看 $WORK/second.log）"
wait "$SECOND" && info "第二个进程已退出（退出码 0）" || fail "第二个进程退出码不是 0"
grep -q "把窗口叫到前面" "$WORK/app.log" || fail "已在运行的那个没收到「叫出窗口」"
info "已在运行的那个把窗口叫到了前面"
alive "$APP" || fail "已在运行的那个不该被挤掉"
api /api/tunnels | grep -q 'zhangsan-verify.t.verify.local' || fail "隧道不该受影响"
info "隧道照常开着"

step "请它退出（新版本接班时就是这么请的）：连接当场断开、隧道名当场放掉"
curl -s -o /dev/null -w '  /api/quit → %{http_code}\n' -X POST "http://127.0.0.1:$API_PORT/api/quit"
for _ in $(seq 1 50); do alive "$APP" || break; sleep 0.1; done
alive "$APP" && fail "请它退出后进程还在（看看 $WORK/app.log）"
info "进程已退出"
LEFT=$(curl -s "http://127.0.0.1:17101/tunnels")
[ "$LEFT" = "[]" ] || fail "服务端还挂着它的隧道：$LEFT"
info "服务端的隧道表已经空了，马上重开不会撞名"

step "重开：隧道回来，不撞名"
target/debug/chuanyun > "$WORK/app2.log" 2>&1 &
APP2=$!
PIDS+=($APP2)
UP=""
for _ in $(seq 1 100); do
    if api /api/tunnels | grep -q '"url":"http://zhangsan-verify.t.verify.local"'; then UP=1; break; fi
    sleep 0.2
done
[ -n "$UP" ] || fail "重开之后隧道没回来：$(api /api/tunnels)"
info "$(api /api/tunnels)"
curl -s -o /dev/null -X POST "http://127.0.0.1:$API_PORT/api/quit"
for _ in $(seq 1 50); do alive "$APP2" || break; sleep 0.1; done
alive "$APP2" && fail "第二次请它退出没退掉"

step "老版本（0.1.13 及以前，不认锁文件）占着本地 API 端口：请它断开再接班"
# 用一个假的老版本本地 API 冒充：status 里没有 pid，只认 /api/shutdown
python3 - "$API_PORT" "$WORK/legacy.log" <<'PY' &
import http.server, json, socketserver, sys
port, log = int(sys.argv[1]), sys.argv[2]
state = {"connected": True}
class H(http.server.BaseHTTPRequestHandler):
    def _send(self, code, body=b""):
        self.send_response(code); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if self.path == "/api/status":
            self._send(200, json.dumps({"connected": state["connected"], "domain_suffix": "t.verify.local",
                "needs_login": False, "reconnect_attempt": 0, "version": "0.1.12"}).encode())
        else:
            self._send(404)
    def do_POST(self):
        open(log, "a").write(self.path + "\n")
        if self.path == "/api/shutdown":
            state["connected"] = False; self._send(204)
        else:
            self._send(404)
    def log_message(self, *a): pass
socketserver.TCPServer.allow_reuse_address = True
socketserver.TCPServer(("127.0.0.1", port), H).serve_forever()
PY
LEGACY=$!
PIDS+=($LEGACY)
sleep 0.5
target/debug/chuanyun > "$WORK/app3.log" 2>&1 &
APP3=$!
PIDS+=($APP3)
for _ in $(seq 1 50); do grep -q "/api/shutdown" "$WORK/legacy.log" 2>/dev/null && break; sleep 0.2; done
grep -q "/api/shutdown" "$WORK/legacy.log" 2>/dev/null || fail "没有请老版本断开（看看 $WORK/app3.log）"
info "已请老版本断开（Windows 上随后还会结束它的进程）"
alive "$APP3" || fail "新版本自己不该退出"
kill "$APP3" 2>/dev/null || true

printf '\n\033[1;32m✓ 桌面客户端整套跑通\033[0m\n'
echo "  · 读状态文件、自动连上服务端"
echo "  · 恢复上次开着的隧道"
echo "  · 公网地址能访问到本地服务"
echo "  · 本地 API 正常"
echo "  · 再打开一次只会叫出已在运行的那个，不会撞名"
echo "  · 退出时当场放掉隧道名，重开马上能用"
echo "  · 老版本占着端口时请它让位"
echo
echo "界面长什么样还是得亲眼看：cargo run -p cy-desktop"
echo
