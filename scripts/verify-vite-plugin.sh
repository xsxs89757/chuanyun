#!/usr/bin/env bash
#
# vite 插件的真实联调：起真服务端 + 真穿云客户端 + 真 Vite 项目，
# 验证插件确实把 dev server 接进了隧道。
#
#   ./scripts/verify-vite-plugin.sh
#
# 单元测试只能验插件的逻辑（名字转换、allowedHosts 合并），验不了「Vite 真的
# 会放行这个域名吗」「注册的端口真的对吗」——那些只有跑一遍真的才知道。
# 事实上这个脚本抓出过两个单元测试抓不到的 bug：本地 API 把 Node 的 fetch
# 当成浏览器挡掉了，以及 Vite 只绑 IPv6 而数据面只连 IPv4。

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
WORK=$(mktemp -d); PIDS=()
cleanup(){ set +m; for p in "${PIDS[@]:-}"; do kill $p 2>/dev/null||true; done; wait 2>/dev/null||true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "▸ 构建插件"
(cd "$ROOT/integrations/vite-plugin-chuanyun" && npm install --silent >/dev/null 2>&1 && npm run build --silent >/dev/null 2>&1)

echo "▸ 起服务端"
mkdir -p "$WORK/data"
cat > "$WORK/s.toml" <<EOF
[control]
listen = "127.0.0.1:17300"
[http]
listen = "127.0.0.1:17380"
domain_suffix = "t.vite.local"
public_scheme = "http"
[admin]
listen = "127.0.0.1:17301"
[storage]
data_dir = "$WORK/data"
EOF
cargo build --quiet -p cy-server --bin chuanyun-server
cargo build --quiet -p cy-core --example headless
TOKEN=$(target/debug/chuanyun-server -c "$WORK/s.toml" user add zhangsan | grep -o 'cy_zhangsan_[0-9a-f]*')
target/debug/chuanyun-server -c "$WORK/s.toml" run > "$WORK/s.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 60); do grep -q 已启动 "$WORK/s.log" 2>/dev/null && break; sleep 0.1; done
PIN=$(grep -A1 证书指纹 "$WORK/s.log" | tail -1 | tr -d ' ')

echo "▸ 起穿云客户端（带本地 API）"
target/debug/examples/headless --server 127.0.0.1:17300 --token "$TOKEN" --pin "$PIN" --local-api > "$WORK/c.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 60); do curl -sf localhost:7075/api/status >/dev/null 2>&1 && break; sleep 0.1; done
curl -s localhost:7075/api/status | sed 's/^/  /'

PLUGIN_DIST="$ROOT/integrations/vite-plugin-chuanyun/dist/index.js"
echo "▸ 造一个真 Vite 项目"
mkdir -p "$WORK/app/src"
cd "$WORK/app"
cat > package.json <<'EOF'
{ "name": "@company/admin-panel", "private": true, "type": "module" }
EOF
echo '<h1>hello from vite</h1>' > index.html
cat > vite.config.js <<EOF
import chuanyun from '$PLUGIN_DIST'
export default { plugins: [chuanyun({})], server: { port: 15173 } }
EOF
npm install --silent vite@^6 >/dev/null 2>&1

echo "▸ 启动 vite dev server"
npx vite --port 15173 > "$WORK/vite.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 150); do grep -q "穿云" "$WORK/vite.log" 2>/dev/null && break; sleep 0.2; done
grep -E "Local|穿云" "$WORK/vite.log" | sed 's/^/  /'

echo "▸ 隧道注册情况"
curl -s localhost:7075/api/tunnels | sed 's/^/  /'

echo "▸ 经隧道访问 vite（这一步会撞 allowedHosts，如果插件没做事的话）"
BODY=$(curl -s --resolve "zhangsan-admin-panel.t.vite.local:17380:127.0.0.1" \
     "http://zhangsan-admin-panel.t.vite.local:17380/")
echo "$BODY" | grep -oE "<title>[^<]*|<h1>[^<]*" | sed "s/^/  /"
echo "$BODY" | grep -q "hello from vite" && echo "  ✓ 经隧道拿到了页面" || { echo "  ✗ 没拿到"; exit 1; }
echo "$BODY" | grep -qi "not allowed" && { echo "  ✗ 被 allowedHosts 拦了"; exit 1; } || true

printf "\n\033[1;32m✓ vite 插件联调通过\033[0m\n\n"
