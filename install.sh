#!/bin/sh
# ferry 安装脚本（Linux / macOS）。
#
# 安全提示：管道执行前无法校验脚本自身。对安全敏感的环境，建议先下载检查：
#   curl -fsSLO https://raw.githubusercontent.com/wuyan19/ferry/main/install.sh
#   less install.sh   # 检查内容
#   sh install.sh
#
# 便捷用法：
#   curl -fsSL https://raw.githubusercontent.com/wuyan19/ferry/main/install.sh | sh
#
# 环境变量：
#   FERRY_MIRROR       下载前缀（如 https://ghproxy.net/），用于受限网络
#   FERRY_BIN_DIR      安装目录，默认 ~/.ferry/bin
#                      （仅改变二进制位置，不改变 ferry 运行时的数据目录）
#   FERRY_VERSION      指定版本（如 v0.1.0），默认最新 release
#   FERRY_NO_PATH_MOD=1  跳过 PATH 配置（CI/容器等不想动 rc 的场景）
set -eu

REPO="wuyan19/ferry"
MIRROR="${FERRY_MIRROR:-}"
BIN_DIR="${FERRY_BIN_DIR:-$HOME/.ferry/bin}"
VERSION="${FERRY_VERSION:-}"

die() { echo "ferry install: $*" >&2; exit 1; }

fetch() { # fetch URL DEST —— 连接超时快速失败，受限网络不至于挂死
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --connect-timeout 10 --retry 2 "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -T 10 -O "$2" "$1"
  else
    die "neither curl nor wget found"
  fi
}

detect_target() {
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
}

resolve_version() {
  if [ -n "$VERSION" ]; then
    case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
    return 0
  fi

  command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 \
    || die "neither curl nor wget found"

  if command -v curl >/dev/null 2>&1; then
    # 跟随 releases/latest 的 302 重定向取 tag（免 GitHub API，无限流）
    final=$(curl -fsSL --connect-timeout 10 -o /dev/null -w '%{url_effective}' \
      "${MIRROR}https://github.com/$REPO/releases/latest") \
      || die "cannot reach github.com (try FERRY_MIRROR=https://ghproxy.net/)"
    VERSION=${final##*/tag/}
  else
    # wget 回退：走 GitHub API，受 60 次/小时速率限制（best-effort）
    VERSION=$(wget -qO- "${MIRROR}https://api.github.com/repos/$REPO/releases/latest" \
      | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1) || true
  fi
  [ -n "$VERSION" ] || die "cannot resolve latest version"
  case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
}

download_and_verify() { # download_and_verify ASSET TMPDIR -> 设置 ASSET_PATH
  _asset=$1; _tmp=$2
  _url="${MIRROR}https://github.com/$REPO/releases/download/$VERSION/$_asset"
  echo "fetching $_asset ..."
  fetch "$_url" "$_tmp/$_asset"
  fetch "$_url.sha256" "$_tmp/$_asset.sha256"

  (
    cd "$_tmp"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c "$_asset.sha256" || die "checksum mismatch"
    elif command -v shasum >/dev/null 2>&1; then
      shasum -a 256 -c "$_asset.sha256" || die "checksum mismatch"
    else
      echo "ferry install: no sha256 tool found, skipping verification" >&2
    fi
  )
  ASSET_PATH="$_tmp/$_asset"
}

install_binary() {
  mkdir -p "$BIN_DIR"
  if command -v install >/dev/null 2>&1; then
    install -m 0755 "$ASSET_PATH" "$BIN_DIR/ferry"
  else
    cp "$ASSET_PATH" "$BIN_DIR/ferry"
    chmod 0755 "$BIN_DIR/ferry"
  fi
}

# 返回 0 表示指定文件已 source .bashrc（此时不应再写它，登录 shell 会拼两次 PATH）
profile_sources_bashrc() {
  [ -f "$1" ] || return 1
  grep -qE '(^|[[:space:]])(source|\.)[[:space:]]+.*\.bashrc' "$1" 2>/dev/null
}

configure_path() {
  in_path=0
  case ":$PATH:" in *":$BIN_DIR:"*) in_path=1 ;; esac
  if [ "$in_path" = "1" ] || [ "${FERRY_NO_PATH_MOD:-0}" = "1" ]; then
    return 0
  fi

  # 精确匹配完整 export 行，避免被注释或别处的路径提及误判为“已配置”
  line="export PATH=\"\$PATH:$BIN_DIR\""
  touched=""

  add_line() {
    _file=$1
    grep -qF "$line" "$_file" 2>/dev/null && return 0
    printf '\n# added by ferry installer\n%s\n' "$line" >> "$_file"
    touched="$touched $_file"
  }

  case "$(basename "${SHELL:-/bin/sh}")" in
    bash)
      add_line "$HOME/.bashrc"
      # 登录 bash 只读 .bash_profile/.bash_login/.profile 三者其一：
      # 已有 .bash_profile 时写它（.profile 根本不会被读）；否则写 .profile，
      # 但若该文件已 source .bashrc 则跳过（避免 PATH 拼接两次）
      login_rc="$HOME/.profile"
      [ -f "$HOME/.bash_profile" ] && login_rc="$HOME/.bash_profile"
      if ! profile_sources_bashrc "$login_rc"; then
        add_line "$login_rc"
      fi
      ;;
    zsh)
      add_line "$HOME/.zshrc"
      ;;
    fish)
      echo "ferry install: fish detected. Add to PATH with:"
      echo "  fish_add_path $BIN_DIR"
      echo "or append to ~/.config/fish/config.fish:"
      echo "  set -gx PATH \$PATH $BIN_DIR"
      return 0
      ;;
    *)
      add_line "$HOME/.profile"
      ;;
  esac

  if [ -n "$touched" ]; then
    echo "PATH configured in:$touched (new terminals)"
  fi
  echo "use now ->  export PATH=\"\$PATH:$BIN_DIR\""
}

main() {
  detect_target
  resolve_version

  asset="ferry-$VERSION-$target"
  tmp=$(mktemp -d 2>/dev/null || mktemp -d -t ferry 2>/dev/null || echo "/tmp/ferry-install.$$")
  mkdir -p "$tmp"
  trap 'rm -rf "$tmp"' EXIT

  download_and_verify "$asset" "$tmp"
  install_binary
  configure_path

  "$BIN_DIR/ferry" --version
  echo "installed: $BIN_DIR/ferry ($asset)"
}

# 所有逻辑包进 main()：curl | sh 中途截断时最多定义一个不完整函数，不会执行半截逻辑
main "$@"