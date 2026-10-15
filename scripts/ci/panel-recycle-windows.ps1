# scripts/ci/panel-recycle-windows.ps1：Windows 上「面板回收要不要打开」的对照数据（ci.yml 的 panel-recycle 作业）。
#
# Windows 上没有隔离的数据目录（measure-windows.ps1 的说明），所以用 runner 的真实用户目录（-UseRealProfile）——只在一次性的
# CI runner 上运行。步骤：
#   0. 写入测量设置 %APPDATA%\wisp\settings.json（exercise.mjs --write-settings：更新检查关闭——否则启动 30 s 后的检查会落进
#      第一段的采样窗口、成功后写下 last_update_check_ms 让后两段不再检查；日志级别 debug——exercise.mjs 与 smoke-gate 靠面板
#      事件核对 exercise 的最后状态）；
#   1. 热身：启动一次 target\release\wisp-desktop.exe、20 秒后退出（不采样）——WebView2 的用户数据目录与磁盘缓存先热起来，
#      三段测量的起点一致（否则只有第一段的用户数据目录是冷的）；
#   然后依次测三段空闲（fake-llm 构建：WISP_PANEL_RECYCLE_SECS 可以覆盖回收时长），每段都是新进程、显式预热 -Warmup 秒：
#   2. never     从未显示（预热之后直接采样）
#   3. used      用过之后（exercise.mjs：灌剪贴板、输入面板与剪贴板面板各唤出再关闭；当前默认：Windows 不回收）
#   4. recycled  用过并回收（WISP_PANEL_RECYCLE_SECS=45，与 macOS 的默认值相同；静置 60 s 后采样）
# 每段开始之前清空剪贴板历史（%LOCALAPPDATA%\wisp 下的 clipboard.db* 与 blobs\）：与 Linux / macOS 的隔离 HOME 一样，每段都从
# 空历史开始——否则 recycled 段灌入的 33 条与 used 段留下的内容相同，应用只把已有条目置顶（日志 `clipboard item touched`），
# 两段的剪贴板列表与内存不可比（会出现「写入 33 条、应用记录 0 条」）。
# 每段都过 smoke-gate --require-webview，后两段再核对 exercise 用到了两个视图、采样窗口里面板已隐藏、窗口的状态
# （--expect-views / --expect-state）。**一段无效不中断**：三段都测完、各段的判定与测量顺序写进摘要之后，有任何一段无效才以
# 非零退出（每个平台只跑一次）。记录与日志写到 target\panel-recycle\。判定方法见 scripts/perf/README.md「面板回收对照」。
[CmdletBinding()]
param(
  [ValidateRange(10, 3600)][int]$Idle = 120,
  [ValidateRange(0, 600)][int]$Warmup = 40
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$bin = Join-Path $root 'target\release\wisp-desktop.exe'
$out = Join-Path $root 'target\panel-recycle'
if (-not (Test-Path $bin)) { throw "$bin 不存在：先 pnpm tauri build --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json" }
New-Item -ItemType Directory -Force -Path $out | Out-Null
# exercise 里的相对路径（node scripts/perf/exercise.mjs）按仓库根目录解析
Set-Location $root

$exercise = 'node scripts/perf/exercise.mjs --views input,clipboard --texts 30 --images 3 --seed-optional'

# 上一次的进程（连同它的 WebView2 进程）退干净之后再启动下一次：否则新实例会经单实例锁转发给旧实例后退出，或者新实例的
# WebView2 挂到旧的浏览器进程上（同一个用户数据目录共用浏览器进程），后代进程的归属随之出错
function Wait-NoWisp {
  $deadline = [DateTime]::UtcNow.AddSeconds(30)
  while ($true) {
    $left = @(Get-Process -Name 'wisp-desktop' -ErrorAction SilentlyContinue)
    $left += @(Get-CimInstance Win32_Process -Filter "Name = 'msedgewebview2.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match 'wisp' })
    if ($left.Count -eq 0) { return }
    if ([DateTime]::UtcNow -gt $deadline) { throw "上一次的 wisp-desktop / WebView2 进程 30 秒内没有退出（$($left.Count) 个）" }
    Start-Sleep -Milliseconds 250
  }
}

# 0. 测量设置（真实用户目录：dirs::config_dir = FOLDERID_RoamingAppData）
$settings = Join-Path ([Environment]::GetFolderPath('ApplicationData')) 'wisp\settings.json'
& node (Join-Path $root 'scripts\perf\exercise.mjs') --write-settings $settings
if ($LASTEXITCODE -ne 0) { throw "写入测量设置失败（退出码 $LASTEXITCODE）" }

# 1. 热身（不采样）
Write-Host '::group::panel-recycle 热身（启动一次、20 秒后退出，不采样）'
Wait-NoWisp
$warm = Start-Process -FilePath $bin -ArgumentList '--autostart' -PassThru
Start-Sleep -Seconds 20
Stop-Process -Id $warm.Id -Force -ErrorAction SilentlyContinue
Write-Host '::endgroup::'

# 剪贴板历史（app::paths：data_dir = FOLDERID_LocalAppData\wisp）；在 Wisp 退出之后调用
$dataDir = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'wisp'
function Reset-ClipboardHistory {
  foreach ($n in @('clipboard.db', 'clipboard.db-wal', 'clipboard.db-shm', 'blobs')) {
    $p = Join-Path $dataDir $n
    if (Test-Path $p) { Remove-Item -Recurse -Force -Path $p }
  }
}

$results = New-Object System.Collections.Generic.List[string]
$failed = $false

function Invoke-Segment([string]$Name, [hashtable]$Extra, [string[]]$GateArgs) {
  Write-Host "::group::panel-recycle $Name"
  $verdict = '有效'
  $json = Join-Path $out "$Name.json"
  try {
    Wait-NoWisp
    Reset-ClipboardHistory
    & (Join-Path $root 'scripts\perf\measure-windows.ps1') -App $bin -UseRealProfile -Warmup $Warmup -Idle $Idle `
      -Out $json -LogOut (Join-Path $out "$Name.log") @Extra
    & node (Join-Path $root 'scripts\perf\smoke-gate.mjs') $json --ci --require-webview @GateArgs
    if ($LASTEXITCODE -ne 0) { $verdict = "无效：smoke-gate 判定测量无效（退出码 $LASTEXITCODE）" }
  } catch {
    $verdict = "测量失败：$($_.Exception.Message)"
  }
  Write-Host '::endgroup::'
  $script:results.Add("| $Name | $verdict |")
  if ($verdict -ne '有效') {
    Write-Host "::error::panel-recycle-windows：${Name}：$verdict"
    $script:failed = $true
  }
}

Invoke-Segment 'never' @{} @()
Invoke-Segment 'used' @{ Exercise = $exercise; Settle = 30 } @('--expect-state', 'hot', '--expect-views', 'input,clipboard')
$env:WISP_PANEL_RECYCLE_SECS = '45'
try {
  Invoke-Segment 'recycled' @{ Exercise = $exercise; Settle = 60 } @('--expect-state', 'recycled', '--expect-views', 'input,clipboard')
} finally {
  Remove-Item Env:WISP_PANEL_RECYCLE_SECS -ErrorAction SilentlyContinue
}
Wait-NoWisp

$summary = @(
  '### 面板回收对照：windows 各段的判定',
  '',
  ("测量顺序：写入测量设置 → 热身启动（20 s，不采样）→ never → used → recycled；每段新进程、runner 的真实用户目录、预热 $Warmup s、" +
    "采样 $Idle s；更新检查关闭（$settings：general.update_policy = off，各段记录的说明里有本次运行的更新检查次数）；" +
    '剪贴板由 powershell.exe（STA）灌入，每段之前清空剪贴板历史。'),
  '',
  '| 段 | 判定 |',
  '| --- | --- |'
) + $results
$text = $summary -join "`n"
Write-Host $text
if ($env:GITHUB_STEP_SUMMARY) { Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value ($text + "`n") -Encoding utf8 }
if ($failed) { exit 1 }
