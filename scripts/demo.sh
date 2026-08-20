#!/usr/bin/env bash
#
# 一键演示：在本机起一整套穿云，跑通「外部请求 → 隧道 → 本地服务」。
#
# 不需要域名、不需要 nginx、不需要公网服务器——全在 127.0.0.1 上，
# 用 curl 的 --resolve 把隧道域名指到本地入口，效果和真实部署一样。
#
#   ./scripts/demo.sh
#
set -euo pipefail

cd "$(dirname "$0")/.."

WORK="$(mktemp -d)"
PIDS=()

cleanup() {
    # 收尾时 shell 会把「进程被终止」打到终端上，盖住演示结果。
    # 关掉作业控制的提示，安静地收摊。
    set +m
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n\033[1;32m▸ %s\033[0m\n' "$1"; }
info() { printf '  %s\n' "$1"; }
fail() { printf '\n\033[1;31m✗ %s\033[0m\n' "$1"; exit 1; }

step "编译"
cargo build --quiet -p cy-server --bin chuanyun-server
cargo build --quiet -p cy-core --example headless
SERVER=target/debug/chuanyun-server
HEADLESS=target/debug/examples/headless

step "准备配置"
mkdir -p "$WORK/data"
cat > "$WORK/server.toml" <<EOF
[control]
listen = "127.0.0.1:17000"
heartbeat_secs = 5

[http]
listen = "127.0.0.1:17080"
domain_suffix = "t.demo.local"
public_scheme = "http"

[storage]
data_dir = "$WORK/data"
EOF
info "配置写在 $WORK/server.toml"

step "创建用户"
TOKEN=$("$SERVER" -c "$WORK/server.toml" user add zhangsan | grep -o 'cy_zhangsan_[0-9a-f]*')
info "凭证 ${TOKEN:0:20}…"

step "启动服务端"
"$SERVER" -c "$WORK/server.toml" run > "$WORK/server.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
    grep -q "已启动" "$WORK/server.log" 2>/dev/null && break
    sleep 0.1
done
PIN=$(grep -A1 "证书指纹" "$WORK/server.log" | tail -1 | tr -d ' ')
[ -n "$PIN" ] || fail "服务端没起来，看看 $WORK/server.log"
info "证书指纹 ${PIN:0:16}…"

step "起一个本地服务（模拟你正在开发的项目）"
python3 -c "
import http.server, socketserver
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = ('你好！这是跑在 127.0.0.1:18080 的本地服务。\n'
                '你看到的这行字，是穿过隧道回来的。\n'
                f'本地服务收到的 Host 头是：{self.headers.get(\"Host\")}\n').encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
socketserver.TCPServer.allow_reuse_address = True
socketserver.TCPServer(('127.0.0.1', 18080), H).serve_forever()
" &
PIDS+=($!)
sleep 0.5
info "本地服务监听 127.0.0.1:18080"

step "启动客户端并开隧道"
"$HEADLESS" --server 127.0.0.1:17000 --token "$TOKEN" --pin "$PIN" \
            --tunnel demo=18080 --local-api > "$WORK/client.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
    grep -q "隧道 demo" "$WORK/client.log" 2>/dev/null && break
    sleep 0.1
done
grep "隧道 demo" "$WORK/client.log" | sed 's/^/  /' || fail "隧道没开起来，看看 $WORK/client.log"

step "从外部访问这个地址"
info "curl http://zhangsan-demo.t.demo.local/"
echo
RESPONSE=$(curl -s --resolve "zhangsan-demo.t.demo.local:17080:127.0.0.1" \
                "http://zhangsan-demo.t.demo.local:17080/")
echo "$RESPONSE" | sed 's/^/  │ /'
echo "$RESPONSE" | grep -q "穿过隧道回来的" || fail "响应内容不对"

step "本地 API：查这个端口现在对外的地址"
info "curl 'localhost:7075/api/resolve?port=18080&plain=1'"
RESOLVED=$(curl -s "http://127.0.0.1:7075/api/resolve?port=18080&plain=1")
info "→ $RESOLVED"
[ "$RESOLVED" = "http://zhangsan-demo.t.demo.local" ] || fail "resolve 返回的地址不对：$RESOLVED"

step "关掉隧道再查一次（这次应该回退到本地）"
curl -s -X DELETE "http://127.0.0.1:7075/api/tunnels/demo" > /dev/null
sleep 0.3
RESOLVED=$(curl -s "http://127.0.0.1:7075/api/resolve?port=18080&plain=1")
info "→ $RESOLVED"
[ "$RESOLVED" = "http://127.0.0.1:18080" ] || fail "关掉隧道后该回退到本地地址，实际是：$RESOLVED"

printf '\n\033[1;32m✓ 全部通过\033[0m\n\n'
echo "这就是同事装上客户端后会经历的全过程，区别只是："
echo "  · 真实部署里域名后缀是公司域名，由 nginx 在 443 终止 TLS"
echo "  · 服务器地址和证书指纹已经编译进安装包，同事只需要输入凭证"
echo
