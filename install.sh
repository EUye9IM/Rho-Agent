#!/bin/sh
# Install the latest rho release.
set -eu

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

REPO=EUye9IM/Rho-Agent
BASE_URL="https://github.com/$REPO/releases/latest/download"
[ -n "${HOME:-}" ] || die "HOME 未设置"
INSTALL_DIR="$HOME/.local/bin"

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

printf '下载 %s ...\n' "$ASSET"
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

printf '已安装 rho 到 %s/rho\n' "$INSTALL_DIR"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) printf '提示：把 %s 加入 PATH 后即可直接运行 rho\n' "$INSTALL_DIR" ;;
esac
