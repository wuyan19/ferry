#!/bin/sh
# ferry 安装脚本（Linux / macOS）。用法：
#   curl -fsSL https://raw.githubusercontent.com/wuyan19/ferry/main/install.sh | sh
# 环境变量：FERRY_MIRROR  下载前缀（如 https://ghproxy.net/），用于受限网络
#           FERRY_BIN_DIR 安装目录，默认 ~/.ferry/bin
#           FERRY_VERSION 指定版本（如 v0.1.0），默认最新 release
set -eu

REPO="wuyan19/ferry"
MIRROR="${FERRY_MIRROR:-}"
BIN_DIR="${FERRY_BIN_DIR:-$HOME/.ferry/bin}"
VERSION="${FERRY_VERSION:-}"

die() { echo "ferry install: $*" >&2; exit 1; }

os=$(uname -s); arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64)        target=x86_64-unknown-linux-musl ;;
      aarch64|arm64) target=aarch64-unknown-linux-musl ;;
      *) die "unsupported Linux arch: $arch (build from source instead)" ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64)  target=aarch64-apple-darwin ;;
      x86_64) target=x86_64-apple-darwin ;;
      *) die "unsupported macOS arch: $arch" ;;
    esac ;;
  MINGW*|MSYS*|CYGWIN*) die "Windows detected: use install.ps1 instead" ;;
  *) die "unsupported OS: $os" ;;
esac

fetch() { # fetch URL DEST —— 连接超时快速失败，受限网络不至于挂死
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --connect-timeout 10 --retry 2 "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -T 10 -O "$2" "$1"
  else
    die "neither curl nor wget found"
  fi
}

# 版本解析：跟随 releases/latest 的 302 重定向取 tag（免 GitHub API，无限流）
if [ -z "$VERSION" ]; then
  if command -v curl >/dev/null 2>&1; then
    final=$(curl -fsSL --connect-timeout 10 -o /dev/null -w '%{url_effective}' \
      "${MIRROR}https://github.com/$REPO/releases/latest") || die "cannot reach github.com (try FERRY_MIRROR=https://ghproxy.net/)"
    VERSION=${final##*/tag/}
  else
    VERSION=$(wget -qO- "${MIRROR}https://api.github.com/repos/$REPO/releases/latest" \
      | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  fi
fi
[ -n "$VERSION" ] || die "cannot resolve latest version"
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac

asset="ferry-$VERSION-$target"
tmp=$(mktemp -d 2>/dev/null || echo "/tmp/ferry-install.$$")
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

url="${MIRROR}https://github.com/$REPO/releases/download/$VERSION/$asset"
echo "fetching $asset ..."
fetch "$url" "$tmp/$asset"
fetch "$url.sha256" "$tmp/$asset.sha256"

# sha256 校验（sha256sum 缺席时退到 shasum，再缺席则警告跳过）
(
  cd "$tmp"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$asset.sha256" || die "checksum mismatch"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$asset.sha256" || die "checksum mismatch"
  else
    echo "ferry install: no sha256 tool found, skipping verification" >&2
  fi
)

mkdir -p "$BIN_DIR"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$tmp/$asset" "$BIN_DIR/ferry"
else
  cp "$tmp/$asset" "$BIN_DIR/ferry"
  chmod 0755 "$BIN_DIR/ferry"
fi

# PATH 配置：自动写入 shell 配置文件（幂等，重跑不重复）。
# 与 rustup/deno 同法：新开终端即可用；当前会话需手动 export（管道安装改不了父 shell）。
# FERRY_NO_PATH_MOD=1 跳过（CI/容器等不想动 rc 的场景）。
in_path=0
case ":$PATH:" in *":$BIN_DIR:"*) in_path=1 ;; esac
if [ "$in_path" = "0" ] && [ "${FERRY_NO_PATH_MOD:-0}" != "1" ]; then
  line="export PATH=\"\$PATH:$BIN_DIR\""
  touched=""
  add_line() {
    grep "$BIN_DIR" "$1" >/dev/null 2>&1 && return 0
    { printf '\n# added by ferry installer\n%s\n' "$line"; } >> "$1"
    touched="$touched $1"
  }
  case "$(basename "${SHELL:-/bin/sh}")" in
    bash) add_line "$HOME/.bashrc"; add_line "$HOME/.profile" ;;
    zsh)  add_line "$HOME/.zshrc" ;;
    fish) touched="(fish: add $BIN_DIR to fish_user_paths manually)" ;;
    *)    add_line "$HOME/.profile" ;;
  esac
  [ -n "$touched" ] && echo "PATH configured in:$touched (new terminals)"
  echo "use now ->  export PATH=\"\$PATH:$BIN_DIR\""
fi

"$BIN_DIR/ferry" --version
echo "installed: $BIN_DIR/ferry ($asset)"
