# ferry 安装脚本（Windows）。用法（PowerShell）：
#   irm https://raw.githubusercontent.com/wuyan19/ferry/main/install.ps1 | iex
# 环境变量：FERRY_MIRROR  下载前缀（如 https://ghproxy.net/），用于受限网络
#           FERRY_BIN_DIR 安装目录，默认 %USERPROFILE%\.ferry\bin
#           FERRY_VERSION 指定版本（如 v0.1.0），默认最新 release
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Windows PowerShell 5.1 默认不启用 TLS 1.2，强制升级（PS 7+ 无此问题）
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'wuyan19/ferry'
$Mirror = if ($env:FERRY_MIRROR) { $env:FERRY_MIRROR } else { '' }
$BinDir = if ($env:FERRY_BIN_DIR) { $env:FERRY_BIN_DIR } else { Join-Path $env:USERPROFILE '.ferry\bin' }

# 版本解析：跟随 releases/latest 的 302 重定向取 tag（免 GitHub API，无限流）
if ($env:FERRY_VERSION) {
  $tag = $env:FERRY_VERSION
} else {
  $req = [Net.HttpWebRequest]::Create("${Mirror}https://github.com/$Repo/releases/latest")
  $req.AllowAutoRedirect = $false
  $resp = $req.GetResponse()
  $loc = $resp.Headers['Location']
  $resp.Close()
  if (-not $loc) { throw "cannot resolve latest release" }
  $tag = $loc.TrimEnd('/').Split('/')[-1]
}
if ($tag -notmatch '^v') { $tag = "v$tag" }

$name = "ferry-$tag-x86_64-pc-windows-msvc.exe"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$dst = Join-Path $BinDir 'ferry.exe'

Write-Host "fetching $name ..."
$base = "${Mirror}https://github.com/$Repo/releases/download/$tag"
# -UseBasicParsing：PS 5.1 的 IE DOM 解析在无 GUI/未初始化环境抛 NullReference
Invoke-WebRequest "$base/$name" -OutFile $dst -UseBasicParsing

# sha256 校验（PS 5.1 对小文本可能返回 Byte[]，需解码）
$raw = (Invoke-WebRequest "$base/$name.sha256" -UseBasicParsing).Content
if ($raw -is [byte[]]) { $raw = [Text.Encoding]::ASCII.GetString($raw) }
$expected = ($raw -replace '\s.*', '').ToLower()
$actual = (Get-FileHash $dst -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) {
  Remove-Item $dst -Force
  throw "checksum mismatch"
}

# 用户 PATH 幂等追加（新开终端生效）
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (@($userPath -split ';') -contains $BinDir)) {
  [Environment]::SetEnvironmentVariable('Path', "$userPath;$BinDir", 'User')
  Write-Host "note: added $BinDir to user PATH (new terminals only)"
}

& $dst --version
Write-Host "installed: $dst"
