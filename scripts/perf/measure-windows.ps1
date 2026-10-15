# scripts/perf/measure-windows.ps1
#
# Windows 空闲测量：主进程与 WebView2 进程各自的工作集、累计 CPU、上下文切换（唤醒的近似），按秒采样，
# 汇总为 wisp.perf/1 性能记录（默认 target/perf/<date>-windows.json，格式见 scripts/perf/README.md）。统计与 schema 校验由 record.mjs 完成。
#
# 用法（PowerShell 7+ 或 Windows PowerShell 5.1）：
#   ./scripts/perf/measure-windows.ps1 [-ProcessId <pid>] [-App <wisp-desktop.exe> -UseRealProfile] [-AppArgs '--autostart']
#                                      [-Idle 600] [-Warmup 30] [-Exercise <命令>] [-Settle 30] [-AppLog <wisp.log>]
#                                      [-LogOut <文件>] [-Interval 1] [-Build release|debug] [-LatencyLog <日志>]
#                                      [-Smoke <报告>[,<报告>]] [-Out <文件>] [-DryRun]
#   默认测量名为 wisp-desktop 的进程；-App 先启动再测量（结束后退出它），默认带 --autostart（静默启动、不弹窗）。
#   **Windows 上没有隔离的数据目录**：Wisp 用 dirs（SHGetKnownFolderPath）找配置、数据与日志目录，WebView2 的用户数据
#   目录同样走已知文件夹，都不读 %APPDATA% / %LOCALAPPDATA% 环境变量——-App 启动的 Wisp 读写的就是当前用户真实的
#   settings.json、clipboard.db、WebView2 配置与日志，测量结果也受这些设置影响。所以 -App 必须显式加 -UseRealProfile
#   确认（在专用的测量账户、虚拟机或 CI runner 上运行）；Wisp 目前没有覆盖数据根目录的开关，无法隔离。
#   -Exercise 在预热之后、采样之前执行一次（PowerShell 脚本块；$env:WISP_PID 为被测进程、$env:WISP_BIN 为它的可执行文件、
#   $env:WISP_LOG 为它的应用日志），之后静置 -Settle 秒再采样——测「用过之后」的唤醒与 WebView2 内存（Windows 面板回收
#   的取舍依据）。推荐 'node scripts/perf/exercise.mjs'：灌剪贴板（powershell.exe -STA）、依次唤出 / 关闭输入与剪贴板面板，
#   每一步由应用日志核对，结束时面板确实已隐藏——它读 debug 级别的面板事件，所以先在设置里把日志级别改成 debug（或
#   `node scripts/perf/exercise.mjs --write-settings "$env:APPDATA\wisp\settings.json"` 写入测量设置；这是真实的配置文件）。
#   只想手动触发时用 `Invoke-WispTrigger <动作>`（Start-Process -Wait 等第二实例转发完，退出码非 0 即失败——Wisp 是 GUI
#   子系统程序，直接 `& $env:WISP_BIN --trigger …` 不会等它结束、也拿不到退出码）。应用日志（-AppLog，-App 时默认
#   %LOCALAPPDATA%\wisp\logs\wisp.log）交给 record.mjs 得出 idle.session：exercise 唤出了哪些视图、采样窗口里面板是否已隐藏、
#   是回收前还是回收后（smoke-gate --expect-state 据此判定）；-Smoke 可给 release、debug 各一份；
#   -DryRun 不需要 Wisp：启动一个临时进程，它带一个周期性占用少量 CPU 的 cmd.exe 子进程（dry-run 把它当作
#   WebView 进程），采样 3 秒，走完整流程并校验主进程与 WebView 的工作集 / CPU / 上下文切换（CI 用）。
#
# 数据来源：RSS 取 WorkingSet64；CPU 取 TotalProcessorTime（累计，取相邻样本之差）；WebView 为主进程的
# 后代中名为 msedgewebview2.exe 的进程（WebView2 的浏览器进程是应用的子进程，渲染 / GPU / 网络进程又是它的子进程），
# 一个都找不到时 WebView 指标记为 null；唤醒次数：精确值需要管理员权限的 ETW（Windows Performance Recorder 的 CPU 配置 + Windows Performance Analyzer 的
# ReadyThread），这里用不需要权限的线程上下文切换累计计数（Win32_PerfRawData_PerfProc_Thread.ContextSwitchesPersec
# 的原始值）之差近似——按线程求差（窗口里退出的线程不计，见 Measure-SwitchDelta），主进程与 WebView 进程分别统计。
# 要按线程定位唤醒来源用 scripts/perf/windows-thread-wakeups.ps1（线程名、起始模块、等待原因、cdb 等待栈）。
[CmdletBinding()]
param(
  [int]$ProcessId = 0,
  [string]$App = '',
  [string]$AppArgs = '--autostart',
  [switch]$UseRealProfile,
  [ValidateRange(1, 86400)][int]$Idle = 600,
  [int]$Warmup = -1,
  [string]$Exercise = '',
  [ValidateRange(0, 3600)][int]$Settle = 30,
  [string]$AppLog = '',
  [string]$LogOut = '',
  [ValidateRange(1, 3600)][int]$Interval = 1,
  [ValidateSet('', 'release', 'debug')][string]$Build = '',
  [string]$LatencyLog = '',
  [string[]]$Smoke = @(),
  [string]$Out = '',
  [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { throw '缺少 node（汇总与校验由 scripts/perf/record.mjs 完成）' }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("wisp-perf-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$launched = $null
$launchMs = $null
# WebView 进程的映像名（dry-run 时换成临时子进程的 cmd.exe）
$webViewName = 'msedgewebview2.exe'

# 主进程的全部后代（按 ParentProcessId 递归）中映像名为 $webViewName 的进程 id
function Get-WebViewPids([int]$Id) {
  $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, Name
  $children = @{}
  foreach ($p in $all) {
    $key = [int]$p.ParentProcessId
    if (-not $children.ContainsKey($key)) { $children[$key] = New-Object System.Collections.Generic.List[object] }
    $children[$key].Add($p)
  }
  $result = New-Object System.Collections.Generic.List[int]
  $queue = New-Object System.Collections.Generic.Queue[int]
  $seen = @{}
  $queue.Enqueue($Id)
  while ($queue.Count -gt 0) {
    $cur = $queue.Dequeue()
    if ($seen.ContainsKey($cur)) { continue }
    $seen[$cur] = $true
    if (-not $children.ContainsKey($cur)) { continue }
    foreach ($c in $children[$cur]) {
      $cid = [int]$c.ProcessId
      if ($cid -eq $cur) { continue }
      if ($c.Name -ieq $webViewName) { $result.Add($cid) }
      $queue.Enqueue($cid)
    }
  }
  return , $result.ToArray()
}

# 进程各线程的上下文切换累计数：线程 id → 次数；读不到时为 $null
function Get-ThreadSwitches([int]$Id) {
  $threads = Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess = $Id" -Property IDThread, ContextSwitchesPersec -ErrorAction SilentlyContinue
  if (-not $threads) { return $null }
  $map = @{}
  foreach ($t in $threads) { $map[[long]$t.IDThread] = [long]$t.ContextSwitchesPersec }
  return $map
}

# 两次读数之间的上下文切换次数，**按线程求差**：两次都在的线程取差，新出现的线程从 0 算起，窗口里退出的线程
# 不计。不能按进程合计相减——退出的线程此前的全部累计数会被一起减掉：长寿的线程（tokio 的阻塞线程、WebView2 浏览器进程
# 的工作线程）在窗口里退出时差值偏小甚至为负，真实的唤醒被掩盖。线程 id 被新线程复用（读数变小）时按新线程算
function Measure-SwitchDelta($Before, $After) {
  if ($null -eq $Before -or $null -eq $After) { return $null }
  $n = [long]0
  foreach ($tid in $After.Keys) {
    $a = $After[$tid]
    $b = $Before[$tid]
    if ($null -ne $b -and $a -ge $b) { $n += $a - $b } else { $n += $a }
  }
  return $n
}

# -Exercise 里用：经第二实例转发一个动作（`wisp --trigger <动作>`）。Start-Process -Wait 等转发完成并检查退出码——GUI 子系统
# 程序用 & 调用不会等待，失败了也不会报错，exercise 会悄悄退化成「从未显示」
function Invoke-WispTrigger([Parameter(Mandatory)][string]$Action) {
  if (-not $env:WISP_BIN) { throw 'Invoke-WispTrigger：WISP_BIN 未设置（只能在 -Exercise 里用）' }
  $p = Start-Process -FilePath $env:WISP_BIN -ArgumentList '--trigger', $Action -Wait -PassThru
  if ($p.ExitCode -ne 0) { throw "wisp --trigger $Action 失败（退出码 $($p.ExitCode)）" }
}

try {
  if ($DryRun) {
    # 临时「主进程」：一个 powershell，带一个 cmd.exe 子进程跑批处理（每轮一小段计算 + 1 秒 ping 等待）
    $batch = Join-Path $tmp 'fake-webview.cmd'
    $batchText = "@echo off`r`nfor /L %%j in (1,1,600) do (`r`n  for /L %%i in (1,1,20000) do set /a x=%%i >nul`r`n  ping -n 2 127.0.0.1 >nul`r`n)`r`n"
    [IO.File]::WriteAllText($batch, $batchText, (New-Object System.Text.ASCIIEncoding))
    $inner = "& `$env:ComSpec /c `"$batch`""
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($inner))
    $launched = Start-Process -FilePath 'powershell.exe' -ArgumentList '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded -PassThru -WindowStyle Hidden
    $ProcessId = $launched.Id
    $webViewName = 'cmd.exe'
    $Idle = 3; $Warmup = 2; $Interval = 1
    if (-not $Out) { $Out = Join-Path $tmp 'dry-run.json' }
    if (-not $Build) { $Build = 'release' }
    # 顺带走一遍 -Exercise 的流程（WISP_PID / WISP_BIN 已设置、进程仍在）
    if (-not $Exercise) {
      $Exercise = 'if (-not (Get-Process -Id $env:WISP_PID -ErrorAction SilentlyContinue) -or -not $env:WISP_BIN) { throw "WISP_PID / WISP_BIN 不对" }'
      $Settle = 1
    }
    # 繁忙的 runner 上 powershell.exe 冷启动可能超过预热的 2 秒：先等「WebView」子进程出现，否则整轮采样都找不到它
    $childDeadline = [DateTime]::UtcNow.AddSeconds(30)
    while ((Get-WebViewPids $ProcessId).Count -eq 0) {
      if ($launched.HasExited) { throw "dry-run 的临时进程 $ProcessId 提前退出了" }
      if ([DateTime]::UtcNow -gt $childDeadline) { throw "dry-run 的临时进程 $ProcessId 在 30 秒内没有启动 cmd.exe 子进程" }
      Start-Sleep -Milliseconds 250
    }
    Write-Host "dry-run：对临时进程 $ProcessId（带 cmd.exe 子进程，当作 WebView）采样 $Idle 秒"
  } elseif ($App) {
    if (-not (Test-Path $App)) { throw "$App 不存在" }
    if (-not $UseRealProfile) {
      throw ('-App 启动的 Wisp 会读写当前用户真实的配置、剪贴板历史、WebView2 配置与日志（Windows 上 Wisp 用已知文件夹，' +
        '不读 %APPDATA% / %LOCALAPPDATA%，脚本无法隔离）：在专用的测量账户、虚拟机或 CI runner 上加 -UseRealProfile 确认后再运行')
    }
    Write-Warning '-UseRealProfile：Wisp 使用当前用户真实的配置与数据目录（settings.json、clipboard.db、WebView2 配置、日志），测量结果受这些设置影响'
    $startArgs = @{ FilePath = $App; PassThru = $true }
    if ($AppArgs) { $startArgs.ArgumentList = $AppArgs }
    $launchMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $launched = Start-Process @startArgs
    # 应用日志（app::paths::log_dir：%LOCALAPPDATA%\wisp\logs，已知文件夹 FOLDERID_LocalAppData）
    if (-not $AppLog) { $AppLog = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'wisp\logs\wisp.log' }
    $ProcessId = $launched.Id
    if ($Warmup -lt 0) { $Warmup = 30 }
    Write-Host "已启动 $App（pid $ProcessId），等待 $Warmup 秒预热"
  } elseif ($ProcessId -eq 0) {
    $p = Get-Process -Name 'wisp-desktop' -ErrorAction SilentlyContinue | Sort-Object StartTime -Descending | Select-Object -First 1
    if (-not $p) { throw '没有找到运行中的 wisp-desktop；用 -ProcessId 或 -App 指定' }
    $ProcessId = $p.Id
  }
  if ($Warmup -lt 0) { $Warmup = 0 }
  if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { throw "进程 $ProcessId 不存在" }
  if (-not $Build) {
    $path = (Get-Process -Id $ProcessId).Path
    $Build = if ($path -match '[\\/]debug[\\/]') { 'debug' } else { 'release' }
  }

  Start-Sleep -Seconds $Warmup
  if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { throw "进程 $ProcessId 在预热期间退出了" }

  # 会话之后的空闲（与 measure-macos.sh 同一做法）：先执行一次 -Exercise，再等 -Settle 秒
  $exerciseStartMs = $null
  $exerciseEndMs = $null
  if ($Exercise) {
    Write-Host "执行 -Exercise：$Exercise"
    $env:WISP_PID = "$ProcessId"
    $env:WISP_BIN = (Get-Process -Id $ProcessId).Path
    $env:WISP_LOG = $AppLog
    $exerciseStartMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    # 脚本块里的错误（含 Invoke-WispTrigger 的失败）在 ErrorActionPreference=Stop 下抛出；原生命令的非零退出码不会抛，
    # 再看一次 $LASTEXITCODE
    $global:LASTEXITCODE = 0
    & ([scriptblock]::Create($Exercise))
    if ($LASTEXITCODE -ne 0) { throw "-Exercise 失败（最后一条原生命令的退出码 $LASTEXITCODE）" }
    $exerciseEndMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    Write-Host "等待 $Settle 秒（会话后的收尾）"
    Start-Sleep -Seconds $Settle
    if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { throw "进程 $ProcessId 在 -Exercise 之后退出了" }
  }

  $os = Get-CimInstance Win32_OperatingSystem -Property Version, Caption
  $cs = Get-CimInstance Win32_ComputerSystem -Property Model
  $raw = Join-Path $tmp 'samples.txt'
  $lines = New-Object System.Collections.Generic.List[string]
  $lines.Add("# measure-windows.ps1 $([DateTime]::UtcNow.ToString('o'))")
  $lines.Add('meta os windows')
  $lines.Add("meta os_version $($os.Caption) $($os.Version)")
  $lines.Add("meta arch $($env:PROCESSOR_ARCHITECTURE.ToLower())")
  $lines.Add("meta machine $($cs.Model)")
  $lines.Add("meta build $Build")
  $lines.Add('meta webview_attribution descendants')
  if ($null -ne $launchMs) { $lines.Add("meta launch_ms $launchMs") }
  if ($Exercise) {
    $lines.Add("meta exercise_start_ms $exerciseStartMs")
    $lines.Add("meta exercise_end_ms $exerciseEndMs")
    $lines.Add("meta settle_s $Settle")
    # 使用画像写进记录：exercise 的命令原样记下（换行换成空格）
    $lines.Add("meta note 会话后空闲：采样前执行了 -Exercise（$($Exercise -replace '\s*\r?\n\s*', ' ')）并静置 $Settle 秒")
  }
  $wv = Get-WebViewPids $ProcessId
  $csStart = Get-ThreadSwitches $ProcessId
  # WebView 进程各自的起点（采样期间才出现的进程没有起点，从 0 算起；开始时一个 WebView 进程都没有时不统计 WebView 唤醒）
  $wvStart = @{}
  foreach ($w in $wv) {
    $m = Get-ThreadSwitches $w
    if ($null -ne $m) { $wvStart[$w] = $m }
  }
  $csWatch = [Diagnostics.Stopwatch]::StartNew()
  if ($null -ne $csStart) {
    $lines.Add('meta wakeups_method context-switches')
    $lines.Add('meta note 唤醒次数用线程上下文切换近似（按线程求差，窗口里退出的线程不计）；精确的唤醒统计需要管理员权限的 ETW（Windows Performance Recorder）')
  } else {
    $lines.Add('meta note 读不到线程性能计数器（Win32_PerfRawData_PerfProc_Thread），唤醒次数记为 null')
  }

  $seenWebView = $false
  $samples = [int][Math]::Floor($Idle / $Interval) + 1
  Write-Host "采样 $Idle 秒（每 $Interval 秒一次，共 $samples 个样本）……"
  for ($i = 0; $i -lt $samples; $i++) {
    # 每 10 个样本重新枚举一次后代；还一个都没找到时（WebView 尚未创建）每个样本都重新找
    if ($i -gt 0 -and ($wv.Count -eq 0 -or $i % 10 -eq 0)) { $wv = Get-WebViewPids $ProcessId }
    $p = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if (-not $p) { throw "进程 $ProcessId 在采样期间退出了" }
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    # 读不到 CPU 时间（权限不足，例如进程以管理员身份运行）时 TotalProcessorTime 为 $null
    if ($null -eq $p.TotalProcessorTime) { throw "读不到进程 $ProcessId 的 CPU 时间（权限不足？请用同一用户、同一权限级别运行本脚本）" }
    $rssKib = [long]($p.WorkingSet64 / 1024)
    $cpuMs = [long]$p.TotalProcessorTime.TotalMilliseconds
    $parts = New-Object System.Collections.Generic.List[string]
    foreach ($w in $wv) {
      # 进程可能刚好退出：取不到就跳过这一个
      $wp = Get-Process -Id $w -ErrorAction SilentlyContinue
      if ($wp -and $null -ne $wp.TotalProcessorTime) { $parts.Add("${w}:$([long]($wp.WorkingSet64 / 1024)):$([long]$wp.TotalProcessorTime.TotalMilliseconds)") }
    }
    $wvField = '-'
    if ($parts.Count -gt 0) { $wvField = $parts -join ','; $seenWebView = $true }
    $lines.Add("sample $now $rssKib $cpuMs $wvField")
    if ($i + 1 -lt $samples) { Start-Sleep -Seconds $Interval }
  }
  if ($null -ne $csStart) {
    $csEnd = Get-ThreadSwitches $ProcessId
    $span = [Math]::Max(0.001, $csWatch.Elapsed.TotalSeconds)
    $d = Measure-SwitchDelta $csStart $csEnd
    if ($null -ne $d) { $lines.Add("meta wakeups_per_s $([Math]::Round($d / $span, 4))") }
    if ($wvStart.Count -gt 0) {
      $wvDelta = [long]0
      $wvRead = $false
      foreach ($w in $wv) {
        $after = Get-ThreadSwitches $w
        if ($null -eq $after) { continue }
        $before = if ($wvStart.ContainsKey($w)) { $wvStart[$w] } else { @{} }
        $wvDelta += Measure-SwitchDelta $before $after
        $wvRead = $true
      }
      if ($wvRead) { $lines.Add("meta wv_wakeups_per_s $([Math]::Round($wvDelta / $span, 4))") }
    }
  }
  if (-not $seenWebView) {
    $lines.Add("meta note 采样期间没有找到 pid $ProcessId 的 $webViewName 后代进程（WebView 尚未创建？）：WebView 指标记为 null")
  }
  # UTF-8 无 BOM（Windows PowerShell 5.1 的 Out-File 默认是 UTF-16）
  [IO.File]::WriteAllLines($raw, $lines, (New-Object System.Text.UTF8Encoding($false)))

  $nodeArgs = @((Join-Path $root 'scripts/perf/record.mjs'), '--raw', $raw)
  if ($Out) { $nodeArgs += @('--out', $Out) }
  if ($LatencyLog) { $nodeArgs += @('--latency-log', $LatencyLog) }
  if ($AppLog) { $nodeArgs += @('--app-log', $AppLog) }
  if ($LogOut) {
    if ($AppLog -and (Test-Path $AppLog)) {
      New-Item -ItemType Directory -Force -Path (Split-Path -Parent ([IO.Path]::GetFullPath($LogOut))) | Out-Null
      Copy-Item -Path $AppLog -Destination $LogOut -Force
    } else {
      Write-Warning "-LogOut：没有应用日志可复制（$AppLog）"
    }
  }
  foreach ($s in $Smoke) { $nodeArgs += @('--smoke', $s) }
  & node @nodeArgs
  if ($LASTEXITCODE -ne 0) { throw "record.mjs 失败（退出码 $LASTEXITCODE）" }

  if ($DryRun) {
    $r = Get-Content -Raw -Path $Out | ConvertFrom-Json
    $d = $r.idle
    $bad = New-Object System.Collections.Generic.List[string]
    if ($d.samples -lt 3) { $bad.Add('样本不足') }
    if ($d.rss_main -le 0) { $bad.Add('主进程工作集') }
    if ($null -eq $d.wakeups) { $bad.Add('主进程上下文切换') }
    if ($null -eq $d.session -or @($d.session.window_s).Count -ne 2) { $bad.Add('会话信息（-Exercise 的时刻与采样窗口）') }
    if ($null -eq $d.webview_processes -or $d.webview_processes -lt 1) { $bad.Add('没有识别出后代 WebView 进程') }
    if ($null -eq $d.rss_webview -or $d.rss_webview -le 0) { $bad.Add('WebView 工作集') }
    if ($null -eq $d.cpu_webview_avg -or $d.cpu_webview_avg -le 0) { $bad.Add('WebView CPU（子进程每轮做一段计算，应 > 0）') }
    if ($null -eq $d.wakeups_webview -or $null -eq $d.cpu_total_avg) { $bad.Add('WebView 上下文切换 / 合计 CPU') }
    if ($bad.Count -gt 0) { throw "dry-run 输出不完整：$($bad -join '；')：$($d | ConvertTo-Json -Compress)" }
    Write-Host '✓ measure-windows.ps1 dry-run 通过（进程采样、后代 WebView 进程的工作集 / CPU / 上下文切换、汇总与 schema 校验）'
  }
} finally {
  if ($launched -and -not $launched.HasExited) {
    # dry-run 的临时进程带子进程：连同子进程一起结束
    if ($DryRun) {
      Get-CimInstance Win32_Process -Filter "ParentProcessId = $($launched.Id)" -ErrorAction SilentlyContinue |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    }
    Stop-Process -Id $launched.Id -Force -ErrorAction SilentlyContinue
  }
  Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}
