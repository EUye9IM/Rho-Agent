#!/bin/sh
# Install the latest stable or nightly rho release.
set -eu

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

REPO=EUye9IM/Rho-Agent
CHANNEL=stable

usage() {
    cat <<'EOF'
用法: install.sh [--nightly]

  --nightly  安装 nightly（dev 分支的滚动构建）
  -h, --help 显示帮助
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --nightly) CHANNEL=nightly ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "未知选项：$1（支持 --nightly）" ;;
    esac
    shift
done

[ -n "${HOME:-}" ] || die "HOME 未设置"
INSTALL_DIR="$HOME/.local/bin"

# nightly 是 prerelease，`latest` 不会指向它，固定用 nightly tag。
if [ "$CHANNEL" = nightly ]; then
    BASE_URL="https://github.com/$REPO/releases/download/nightly"
else
    BASE_URL="https://github.com/$REPO/releases/latest/download"
fi

[ "$(uname -s)" = Linux ] || die "只支持 Linux"

case "$(uname -m)" in
    x86_64 | amd64) ASSET=rho-x86_64-unknown-linux-musl.tar.gz ;;
    aarch64 | arm64) ASSET=rho-aarch64-unknown-linux-musl.tar.gz ;;
    *) die "暂无 $(uname -m) 架构的产物" ;;
esac

command -v curl >/dev/null 2>&1 || die "需要 curl"
command -v tar >/dev/null 2>&1 || die "需要 tar"
command -v sha256sum >/dev/null 2>&1 || die "需要 sha256sum"
command -v install >/dev/null 2>&1 || die "需要 install"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/rho-install.XXXXXX") \
    || die "无法创建临时目录"
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

if [ "$CHANNEL" = nightly ]; then
    printf '下载 nightly %s ...\n' "$ASSET"
else
    printf '下载 %s ...\n' "$ASSET"
fi
curl -fL --retry 2 --connect-timeout 10 \
    -o "$TMP/$ASSET" "$BASE_URL/$ASSET"
curl -fL --retry 2 --connect-timeout 10 \
    -o "$TMP/$ASSET.sha256" "$BASE_URL/$ASSET.sha256"

(
    cd "$TMP"
    sha256sum -c "$ASSET.sha256"
) || die "校验失败"

tar -xzf "$TMP/$ASSET" -C "$TMP" || die "解压失败"
[ -f "$TMP/rho" ] || die "产物中没有 rho 二进制"

mkdir -p "$INSTALL_DIR" || die "无法创建 $INSTALL_DIR"
install -m 0755 "$TMP/rho" "$INSTALL_DIR/rho"

if [ "$CHANNEL" = nightly ]; then
    printf '已安装 rho nightly 到 %s/rho\n' "$INSTALL_DIR"
else
    printf '已安装 rho 到 %s/rho\n' "$INSTALL_DIR"
fi
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) printf '提示：把 %s 加入 PATH 后即可直接运行 rho\n' "$INSTALL_DIR" ;;
esac
