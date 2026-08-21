#!/usr/bin/env bash
#
# 服务端安装脚本的真实验证：在带 systemd 的 Linux 容器里，用真的发布产物
# 从零装一遍，再把出错的几条路径挨个走一遍。
#
#   ./scripts/verify-install.sh              # 用最新 release
#   ./scripts/verify-install.sh v0.1.0       # 用指定版本
#
# install-server.sh 是要用 root 在别人服务器上跑的东西，光 shellcheck 过了
# 不算数：useradd 在不同发行版参数不一样、StateDirectory 会覆盖目录权限、
# 服务起不来时该把日志摆出来——这些只有真跑一遍才知道。
#
# 事实上这个脚本抓出过：systemd 的 StateDirectory 把安装脚本设的 0700 覆盖成
# 0755，同机器其他 ssh 用户能列到私钥目录。
#
# 要 Docker。宿主机走 TUN 模式代理时容器可能出不去网，所以产物在宿主机上
# 下好再挂进去（CHUANYUN_BASE_URL 本来是给内网镜像用的，这里正好借用）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-}"
IMAGE=chuanyun-verify-systemd
WORK=$(mktemp -d)
PASS=0
FAIL=0

# shellcheck disable=SC2329  # 由 trap 调用
cleanup() {
    docker rm -f cy-verify cy-verify-bad >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n\033[1;32m▸ %s\033[0m\n' "$1"; }
ok()   { PASS=$((PASS+1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
no()   { FAIL=$((FAIL+1)); printf '  \033[31m✗\033[0m %s\n' "$1"; }

# 断言：命令跑通就算过
check() {
    local what="$1"; shift
    if "$@" >/dev/null 2>&1; then ok "$what"; else no "$what"; fi
}

# 断言：命令输出里必须出现某段文字
#
# 先把输出收进变量再 grep，不能写成 `"$@" | grep -q`：这里开了 pipefail，
# 被测命令报错退出（很多用例本来就该退出 1）会让整条管道判定为失败，
# 哪怕 grep 匹配上了也算没匹配。
expect() {
    local what="$1" want="$2"; shift 2
    local out
    out=$("$@" 2>&1 || true)
    if grep -q -- "$want" <<<"$out"; then
        ok "$what"
    else
        no "$what（没找到「$want」）"
        tail -8 <<<"$out" | awk '{print "      " $0}'
    fi
}

command -v docker >/dev/null || { echo "需要 Docker"; exit 1; }
docker info >/dev/null 2>&1 || { echo "Docker 没在跑"; exit 1; }

step "准备容器镜像"
cat > "$WORK/Dockerfile" <<'EOF'
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && \
    apt-get install -y --no-install-recommends systemd systemd-sysv curl ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    find /etc/systemd/system /lib/systemd/system -path '*.wants/*' -name '*getty*' -delete
CMD ["/lib/systemd/systemd"]
EOF
# 预编译产物是 x86_64 的，在 arm 机器上得让容器也跑 amd64
docker build --platform linux/amd64 -q -t "$IMAGE" "$WORK" >/dev/null
echo "  $IMAGE"

step "取发布产物"
mkdir -p "$WORK/rel"
if [ -z "$VERSION" ]; then
    u=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        https://github.com/xsxs89757/chuanyun/releases/latest)
    VERSION="${u##*/tag/}"
fi
NUM="${VERSION#v}"
TARBALL="chuanyun-server-$NUM-linux-x86_64.tar.gz"
BASE="https://github.com/xsxs89757/chuanyun/releases/download/$VERSION"
curl -fsSL "$BASE/$TARBALL" -o "$WORK/rel/$TARBALL"
curl -fsSL "$BASE/SHA256SUMS" -o "$WORK/rel/SHA256SUMS"
echo "  $VERSION  $TARBALL"

boot() {
    docker rm -f "$1" >/dev/null 2>&1 || true
    docker run -d --name "$1" --platform linux/amd64 --privileged \
        -v "$WORK/rel:/rel:ro" -v "$ROOT/scripts:/scripts:ro" "$IMAGE" >/dev/null
    for _ in $(seq 30); do
        docker exec "$1" systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded' && return 0
        sleep 1
    done
    echo "容器里的 systemd 没起来"; return 1
}
run() { docker exec -e CHUANYUN_BASE_URL=file:///rel cy-verify sh /scripts/install-server.sh "$@"; }
# shellcheck disable=SC2329  # 作为命令名传给 check，shellcheck 看不见
inside() { docker exec cy-verify sh -c "$1"; }

boot cy-verify

step "拦住不该放过去的"
expect "缺 --domain 时说清楚要怎么给" "首次安装要指定隧道域名后缀" \
    run --version "$VERSION"
expect "非 root 时提示加 sudo" "需要 root" \
    docker exec cy-verify sh -c "id -u tester >/dev/null 2>&1 || useradd -m tester; su tester -c 'sh /scripts/install-server.sh --domain t.example.com'"

step "正常安装"
if run --domain t.example.com --version "$VERSION" >"$WORK/out.txt" 2>&1; then
    ok "装完退出码 0"
else
    no "安装失败"; tail -20 "$WORK/out.txt" | awk '{print "      " $0}'
fi
check "打出了接下来该做什么" grep -q "还差四步才能用" "$WORK/out.txt"
check "打出了证书指纹" grep -qE "^     [0-9a-f]{64}$" "$WORK/out.txt"

step "装出来的东西对不对"
expect "服务在跑"            "active"      inside "systemctl is-active chuanyun-server"
expect "设了开机自启"        "enabled"     inside "systemctl is-enabled chuanyun-server"
expect "以 chuanyun 用户跑"  "chuanyun"    inside "systemctl show -p User --value chuanyun-server"
expect "数据目录 0700"       "drwx------"  inside "ls -ld /var/lib/chuanyun"
expect "别的用户进不去数据目录" "Permission denied" \
    inside "id -u tester >/dev/null 2>&1 || useradd -m tester; su tester -c 'ls /var/lib/chuanyun/' 2>&1"
expect "配置写的是给的域名"  "t.example.com" inside "cat /etc/chuanyun/server.toml"
expect "nginx 样例留在机器上" "nginx.conf.example" inside "ls /etc/chuanyun/"
expect "fingerprint 子命令能用" "^[0-9a-f]\{64\}$" inside "chuanyun-server fingerprint"
expect "管理接口有响应"      "domain_suffix" inside "chuanyun-server status"
expect "能发凭证"            "cy_zhangsan_"  inside "chuanyun-server user add zhangsan"
expect "控制通道在监听"      "1B58" inside "awk '\$4==\"0A\"{split(\$2,a,\":\");print a[2]}' /proc/net/tcp"

step "再跑一次是升级，不是重装"
run --version "$VERSION" --domain 换个域名.com >"$WORK/up.txt" 2>&1 || true
check "没覆盖已有配置" grep -q "配置已存在，没有改动" "$WORK/up.txt"
check "认出这是升级" grep -q "升级完成" "$WORK/up.txt"
expect "域名还是原来那个" "t.example.com" inside "cat /etc/chuanyun/server.toml"

step "校验和对不上要停下"
cp "$WORK/rel/SHA256SUMS" "$WORK/good"
printf '%064d  %s\n' 0 "$TARBALL" > "$WORK/rel/SHA256SUMS"
expect "拒绝装校验和不符的包" "校验和不符" run --version "$VERSION"
cp "$WORK/good" "$WORK/rel/SHA256SUMS"

step "卸载"
run --uninstall >"$WORK/un.txt" 2>&1
check "卸载报告完成" grep -q "卸载完成" "$WORK/un.txt"
check "二进制删了" inside "test ! -f /usr/local/bin/chuanyun-server"
check "数据目录留着（故意的）" inside "test -d /var/lib/chuanyun"

step "服务起不来时要把日志摆出来"
mkdir -p "$WORK/badstage"
tar xzf "$WORK/rel/$TARBALL" -C "$WORK/badstage"
printf '#!/bin/sh\necho "Error: 配置有问题" >&2\nexit 1\n' > "$WORK/badstage/chuanyun-server"
chmod +x "$WORK/badstage/chuanyun-server"
mkdir -p "$WORK/bad"
tar czf "$WORK/bad/$TARBALL" -C "$WORK/badstage" .
if command -v sha256sum >/dev/null 2>&1; then
    ( cd "$WORK/bad" && sha256sum -- "$TARBALL" > SHA256SUMS )
else
    ( cd "$WORK/bad" && shasum -a 256 -- "$TARBALL" > SHA256SUMS )
fi
docker rm -f cy-verify-bad >/dev/null 2>&1 || true
docker run -d --name cy-verify-bad --platform linux/amd64 --privileged \
    -v "$WORK/bad:/rel:ro" -v "$ROOT/scripts:/scripts:ro" "$IMAGE" >/dev/null
for _ in $(seq 30); do
    docker exec cy-verify-bad systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded' && break
    sleep 1
done
out=$(docker exec -e CHUANYUN_BASE_URL=file:///rel cy-verify-bad \
      sh /scripts/install-server.sh --domain t.example.com --version "$VERSION" 2>&1 || true)
check "说清楚了服务没起来" grep -q "服务没能起来" <<<"$out"
check "把服务的错误日志摆出来了" grep -q "配置有问题" <<<"$out"

printf '\n\033[1m%d 项通过'  "$PASS"
[ "$FAIL" -gt 0 ] && printf '，\033[31m%d 项失败\033[0m\033[1m' "$FAIL"
printf '\033[0m\n\n'
exit $(( FAIL > 0 ? 1 : 0 ))
