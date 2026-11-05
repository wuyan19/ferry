# ferry 安装脚本（Windows）。
#
# 安全提示：irm | iex 会在下载的同时执行，无法在运行前审查脚本内容。
# 对安全敏感的环境，建议先下载检查：
#   irm https://raw.githubusercontent.com/wuyan19/ferry/main/install.ps1 -OutFile install.ps1
#   notepad install.ps1   # 检查内容
#   Unblock-File .\install.ps1   # 解除 Mark-of-the-Web（如执行策略为 RemoteSigned）
#   .\install.ps1
#
# 便捷用法：
#   irm https://raw.githubusercontent.com/wuyan19/ferry/main/install.ps1 | iex
#
# 环境变量：
#   FERRY_MIRROR        下载前缀（如 https://ghproxy.net/），用于受限网络
#   FERRY_BIN_DIR       安装目录，默认 %USERPROFILE%\.ferry\bin
#                       （仅改变二进制位置，不改变 ferry 运行时的数据目录）
#   FERRY_VERSION       指定版本（如 v0.1.0），默认最新 release
#   FERRY_NO_PATH_MOD=1 跳过用户 PATH 修改（CI/容器等不想动环境的场景）

function Main {
  $ErrorActionPreference = 'Stop'
  # 仅在函数作用域内生效；函数返回后父会话的进度条行为不受影响
  $ProgressPreference = 'SilentlyContinue'

  # Windows PowerShell 5.1 默认不启用 TLS 1.2，强制升级（PS 7+ 无此问题）
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

  $Repo   = 'wuyan19/ferry'
  $Mirror = if ($env:FERRY_MIRROR) { $env:FERRY_MIRROR } else { '' }
  $BinDir = if ($env:FERRY_BIN_DIR) { $env:FERRY_BIN_DIR } else { Join-Path $env:USERPROFILE '.ferry\bin' }

  # ---- 版本解析 ----------------------------------------------------------
  function Resolve-LatestTag {
    # 首选：跟随 releases/latest 的 302 重定向取 tag（免 API，无限流）
    $resp = $null
    try {
      $req = [Net.HttpWebRequest]::Create("${Mirror}https://github.com/$Repo/releases/latest")
      $req.AllowAutoRedirect = $false
      $req.Timeout = 10000
      $resp = $req.GetResponse()
      $loc = $resp.Headers['Location']   # 先提取，再关闭（finally 里关闭）
    } catch {
      $loc = $null
    } finally {
      if ($resp) { $resp.Close() }
    }

    if ($loc) {
      return $loc.TrimEnd('/').Split('/')[-1]
    }

    # 兜底：Location 为空（镜像/代理行为差异导致返回 200）时回退 API
    # 注意：API 受 60 次/小时速率限制，仅作兜底
    try {
      $api = Invoke-RestMethod "${Mirror}https://api.github.com/repos/$Repo/releases/latest" `
        -UseBasicParsing -Headers @{ 'User-Agent' = 'ferry-installer' }
      if ($api.tag_name) { return $api.tag_name }
    } catch { }

    throw "cannot resolve latest release (try setting FERRY_VERSION or FERRY_MIRROR)"
  }

  if ($env:FERRY_VERSION) {
    $tag = $env:FERRY_VERSION
  } else {
    $tag = Resolve-LatestTag
  }
  if ($tag -notmatch '^v') { $tag = "v$tag" }

  $name = "ferry-$tag-x86_64-pc-windows-msvc.exe"
  $dst  = Join-Path $BinDir 'ferry.exe'
  New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

  # ---- 下载 + 校验（先写临时文件，成功后再原子替换） -----------------------
  $base    = "${Mirror}https://github.com/$Repo/releases/download/$tag"
  $tmpFile = Join-Path $env:TEMP ("ferry-" + [guid]::NewGuid().ToString('N') + ".exe")

  try {
    Write-Host "fetching $name ..."
    # -UseBasicParsing：PS 5.1 的 IE DOM 解析在无 GUI/未初始化环境抛 NullReference
    # -TimeoutSec：PS 5.1 默认无超时，镜像卡死会无限挂起（与 sh 版快速失败对齐）
    Invoke-WebRequest "$base/$name" -OutFile $tmpFile -UseBasicParsing -TimeoutSec 300

    # sha256 校验（PS 5.1 对小文本可能返回 Byte[]，需解码）
    $raw = (Invoke-WebRequest "$base/$name.sha256" -UseBasicParsing -TimeoutSec 60).Content
    if ($raw -is [byte[]]) { $raw = [Text.Encoding]::ASCII.GetString($raw) }
    $expected = ($raw -replace '\s.*', '').ToLower()
    $actual   = (Get-FileHash $tmpFile -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) { throw "checksum mismatch" }

    # 校验通过后才落盘到目标路径，失败不影响已有的 ferry.exe
    Move-Item -Force -Path $tmpFile -Destination $dst
  } finally {
    # 任何失败路径（下载中断、校验失败）都清理临时文件
    if (Test-Path $tmpFile) {
      Remove-Item $tmpFile -Force -ErrorAction SilentlyContinue
    }
  }

  # ---- 用户 PATH 幂等追加（新开终端生效） ---------------------------------
  if ($env:FERRY_NO_PATH_MOD -ne '1') {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    if (-not (@($userPath -split ';') -contains $BinDir)) {
      $newPath = if ($userPath) { "$userPath;$BinDir" } else { $BinDir }
      [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "note: added $BinDir to user PATH (new terminals)"
    Write-Host "use in this terminal ->  `$env:Path += ';$BinDir'"
    }
  }

  & $dst --version
  Write-Host "installed: $dst"
}

# 全部逻辑包进 Main：$ErrorActionPreference/$ProgressPreference 等只影响函数作用域，
# 不污染 irm | iex 的父会话状态
Main