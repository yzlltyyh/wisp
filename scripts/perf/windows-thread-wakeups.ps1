# scripts/perf/windows-thread-wakeups.ps1：空闲唤醒与 CPU 的按线程归因工具
#
# 按线程定位一个 Windows 进程的空闲唤醒与 CPU（与 macos-thread-wakeups.py / linux-thread-wakeups.py 同一口径），不需要管理员权限：
#
# - 计数窗口首尾各取一次 NtQuerySystemInformation(SystemProcessInformation) 的线程表：每个线程的上下文切换累计数
#   （SYSTEM_THREAD_INFORMATION.ContextSwitches，线程每被切换上 CPU 一次加 1——空闲线程每被唤醒一次加 1，与
#   measure-windows.ps1 用的性能计数器 Thread\Context Switches/sec 是同一个内核计数）、内核 + 用户 CPU 时间、窗口结束时
#   的状态与等待原因（KWAIT_REASON：WrUserRequest = 消息循环、UserRequest = WaitForSingleObject(Ex) 一类、WrQueue =
#   IOCP / 线程池、WrAlertByThreadId = Rust 的 park / 条件变量……）；窗口里新出现的线程从 0 算起，窗口里退出的线程只记名字
#   与它在窗口开始时的累计数（`exited_in_window[].switches_at_start`）；
# - 同一窗口的旧口径对照（`legacy_process_sum`）：两次快照各自的「全部线程累计数之和」相减——measure-windows.ps1
#   早期的做法。窗口里退出的线程此前的全部累计数会被一起减掉，所以它偏小、甚至为负；与按线程求差的
#   `switches_per_s` 并列输出，用来在同一进程、同一窗口上直接对照两种口径；
# - 线程名取 GetThreadDescription（Rust 的命名线程、Chromium / WebView2 的线程都有），起始地址取
#   NtQueryInformationThread(ThreadQuerySetWin32StartAddress) 并换算成「模块+偏移」（没有名字的线程靠它归属：
#   ntdll 的线程池、combase 的 COM、EmbeddedBrowserWebView.dll 的 WebView2 宿主线程……）；
# - 可选（-Stacks <文件>）：计数窗口之后用 cdb（Debugging Tools for Windows，Windows SDK 自带）以非侵入方式附加，转储每个
#   线程此刻的等待栈（系统 DLL 走微软符号服务器；release 构建 strip 了符号，Wisp 自己的帧只有「模块+偏移」）。附加期间
#   目标的线程会被挂起片刻，所以放在计数窗口之后。
#
# 用法（PowerShell 7+ 或 Windows PowerShell 5.1；64 位）：
#   ./scripts/perf/windows-thread-wakeups.ps1 -ProcessId <pid> [-Seconds 120] [-Warmup 0] [-Label <文本>] [-Out <文件>]
#                                             [-Stacks <文件>] [-Cdb <cdb.exe>]
#   ./scripts/perf/windows-thread-wakeups.ps1 -SelfTest   起一个每 100 ms 醒一次的临时进程，核对它的唤醒能被测出、能归到线程
# 输出 JSON（-Out 缺省时打印到标准输出），字段见文件末尾的 $result。
[CmdletBinding()]
param(
  [int]$ProcessId = 0,
  [ValidateRange(1, 86400)][double]$Seconds = 120,
  [ValidateRange(0, 86400)][double]$Warmup = 0,
  [string]$Label = '',
  [string]$Out = '',
  [string]$Stacks = '',
  [string]$Cdb = '',
  [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ScriptVersion = 2

if (-not ('WispThreadProbe' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public sealed class WispThreadSample {
  public int Tid;
  public long ContextSwitches;
  public long KernelTime;   // 100 ns
  public long UserTime;     // 100 ns
  public int State;
  public int WaitReason;
  public int Priority;
}

public static class WispThreadProbe {
  [DllImport("ntdll.dll")]
  static extern int NtQuerySystemInformation(int infoClass, IntPtr info, int length, out int returned);
  [DllImport("ntdll.dll")]
  static extern int NtQueryInformationThread(IntPtr thread, int infoClass, out IntPtr info, int length, IntPtr returned);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern IntPtr OpenThread(int access, bool inherit, int tid);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")]
  static extern int GetThreadDescription(IntPtr thread, out IntPtr description);
  [DllImport("kernel32.dll")]
  static extern IntPtr LocalFree(IntPtr mem);

  const int SystemProcessInformation = 5;
  const int ThreadQuerySetWin32StartAddress = 9;
  const int STATUS_INFO_LENGTH_MISMATCH = unchecked((int)0xC0000004);
  const int THREAD_QUERY_INFORMATION = 0x0040;
  const int THREAD_QUERY_LIMITED_INFORMATION = 0x0800;
  // x64 布局：SYSTEM_PROCESS_INFORMATION 256 字节，其后是 NumberOfThreads 个 80 字节的 SYSTEM_THREAD_INFORMATION
  const int ProcThreadsOffset = 256;
  const int ProcPidOffset = 80;
  const int ThreadSize = 80;

  // 进程 pid 的全部线程（找不到进程时返回空表）
  public static List<WispThreadSample> Snapshot(int pid) {
    if (IntPtr.Size != 8) throw new InvalidOperationException("只支持 64 位 PowerShell");
    int size = 1 << 20;
    while (true) {
      IntPtr buf = Marshal.AllocHGlobal(size);
      try {
        int needed;
        int status = NtQuerySystemInformation(SystemProcessInformation, buf, size, out needed);
        if (status == STATUS_INFO_LENGTH_MISMATCH) { size = Math.Max(size * 2, needed + 65536); continue; }
        if (status != 0) throw new InvalidOperationException("NtQuerySystemInformation 0x" + status.ToString("X8"));
        var list = new List<WispThreadSample>();
        long offset = 0;
        while (true) {
          IntPtr p = new IntPtr(buf.ToInt64() + offset);
          int next = Marshal.ReadInt32(p, 0);
          int threads = Marshal.ReadInt32(p, 4);
          long id = Marshal.ReadIntPtr(p, ProcPidOffset).ToInt64();
          if (id == pid) {
            for (int i = 0; i < threads; i++) {
              IntPtr t = new IntPtr(p.ToInt64() + ProcThreadsOffset + (long)i * ThreadSize);
              list.Add(new WispThreadSample {
                KernelTime = Marshal.ReadInt64(t, 0),
                UserTime = Marshal.ReadInt64(t, 8),
                Tid = (int)Marshal.ReadIntPtr(t, 48).ToInt64(),
                Priority = Marshal.ReadInt32(t, 56),
                ContextSwitches = (uint)Marshal.ReadInt32(t, 64),
                State = Marshal.ReadInt32(t, 68),
                WaitReason = Marshal.ReadInt32(t, 72),
              });
            }
            return list;
          }
          if (next == 0) return list;
          offset += next;
        }
      } finally {
        Marshal.FreeHGlobal(buf);
      }
    }
  }

  // GetThreadDescription；没有名字或读不到时为空串
  public static string Name(int tid) {
    IntPtr h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, tid);
    if (h == IntPtr.Zero) return "";
    try {
      IntPtr s;
      if (GetThreadDescription(h, out s) < 0 || s == IntPtr.Zero) return "";
      try { return Marshal.PtrToStringUni(s) ?? ""; } finally { LocalFree(s); }
    } finally {
      CloseHandle(h);
    }
  }

  // 线程的 Win32 起始地址（CreateThread 传入的函数）；读不到时为 0
  public static long StartAddress(int tid) {
    IntPtr h = OpenThread(THREAD_QUERY_INFORMATION, false, tid);
    if (h == IntPtr.Zero) return 0;
    try {
      IntPtr addr;
      return NtQueryInformationThread(h, ThreadQuerySetWin32StartAddress, out addr, IntPtr.Size, IntPtr.Zero) == 0
        ? addr.ToInt64() : 0;
    } finally {
      CloseHandle(h);
    }
  }
}
'@
}

# KWAIT_REASON（wdm.h）
$WaitReasons = @(
  'Executive', 'FreePage', 'PageIn', 'PoolAllocation', 'DelayExecution', 'Suspended', 'UserRequest', 'WrExecutive',
  'WrFreePage', 'WrPageIn', 'WrPoolAllocation', 'WrDelayExecution', 'WrSuspended', 'WrUserRequest', 'WrSpare0',
  'WrQueue', 'WrLpcReceive', 'WrLpcReply', 'WrVirtualMemory', 'WrPageOut', 'WrRendezvous', 'WrKeyedEvent',
  'WrTerminated', 'WrProcessInSwap', 'WrCpuRateControl', 'WrCalloutStack', 'WrKernel', 'WrResource', 'WrPushLock',
  'WrMutex', 'WrQuantumEnd', 'WrDispatchInt', 'WrPreempted', 'WrYieldExecution', 'WrFastMutex', 'WrGuardedMutex',
  'WrRundown', 'WrAlertByThreadId', 'WrDeferredPreempt', 'WrPhysicalFault', 'WrIoRing', 'WrMdlCache'
)
# KTHREAD_STATE
$ThreadStates = @('Initialized', 'Ready', 'Running', 'Standby', 'Terminated', 'Waiting', 'Transition', 'DeferredReady',
  'GateWaitObsolete', 'WaitingForProcessInSwap')

function Get-Label([string[]]$Table, [int]$Value) {
  if ($Value -ge 0 -and $Value -lt $Table.Count) { return $Table[$Value] }
  return "#$Value"
}

# 模块表（起始地址 → 「模块+0x偏移」）
function Get-ModuleTable([int]$Id) {
  $mods = New-Object System.Collections.Generic.List[object]
  try {
    foreach ($m in (Get-Process -Id $Id).Modules) {
      $mods.Add([pscustomobject]@{ Name = $m.ModuleName; Base = $m.BaseAddress.ToInt64(); Size = [long]$m.ModuleMemorySize })
    }
  } catch {
    Write-Warning "读不到进程 $Id 的模块表：$($_.Exception.Message)"
  }
  return , $mods
}

function Format-Address($Modules, [long]$Address) {
  if ($Address -eq 0) { return $null }
  foreach ($m in $Modules) {
    if ($Address -ge $m.Base -and $Address -lt $m.Base + $m.Size) {
      return ('{0}+0x{1:x}' -f $m.Name, ($Address - $m.Base))
    }
  }
  return ('0x{0:x}' -f $Address)
}

# 线程 id → 名字 / 起始地址（同一线程只查一次；线程退出后查不到，所以首尾两次快照都查）
$script:ThreadInfo = @{}
function Get-ThreadInfo([int]$Tid) {
  if (-not $script:ThreadInfo.ContainsKey($Tid)) {
    $script:ThreadInfo[$Tid] = [pscustomobject]@{
      Name = [WispThreadProbe]::Name($Tid)
      Start = [WispThreadProbe]::StartAddress($Tid)
    }
  }
  return $script:ThreadInfo[$Tid]
}

function Find-Cdb {
  if ($Cdb) { return $Cdb }
  $roots = @(${env:ProgramFiles(x86)}, $env:ProgramFiles) | Where-Object { $_ }
  foreach ($r in $roots) {
    $p = Join-Path $r 'Windows Kits\10\Debuggers\x64\cdb.exe'
    if (Test-Path $p) { return $p }
  }
  return $null
}

# cdb 非侵入附加，转储全部线程的等待栈（超时 180 s：符号下载慢时宁可没有栈，也不卡住测量）
function Save-Stacks([int]$Id, [string]$Path) {
  $exe = Find-Cdb
  if (-not $exe) { return 'cdb 不可用（没有安装 Debugging Tools for Windows）' }
  $cache = Join-Path ([IO.Path]::GetTempPath()) 'wisp-symcache'
  New-Item -ItemType Directory -Force -Path $cache | Out-Null
  $log = [IO.Path]::GetFullPath($Path)
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
  $cdbArgs = @('-pv', '-p', "$Id", '-y', "`"srv*$cache*https://msdl.microsoft.com/download/symbols`"", '-logo', "`"$log`"",
    '-c', '"~*k 40; qd"')
  $p = Start-Process -FilePath $exe -ArgumentList $cdbArgs -PassThru -WindowStyle Hidden
  if (-not $p.WaitForExit(180000)) {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    return "cdb 180 s 内没有结束（已结束它；$log 里可能只有部分栈）"
  }
  return $null
}

function Measure-Threads([int]$Id, [double]$Secs, [double]$Wait, [string]$Name, [string]$StackFile) {
  if ($Wait -gt 0) { Start-Sleep -Milliseconds ([int]($Wait * 1000)) }
  if (-not (Get-Process -Id $Id -ErrorAction SilentlyContinue)) { throw "进程 $Id 不存在" }
  $proc = Get-Process -Id $Id
  $modules = Get-ModuleTable $Id
  $started = [DateTimeOffset]::Now
  $t0 = @{}
  foreach ($s in [WispThreadProbe]::Snapshot($Id)) { $t0[$s.Tid] = $s; $null = Get-ThreadInfo $s.Tid }
  $cpu0 = $proc.TotalProcessorTime.TotalMilliseconds
  $watch = [Diagnostics.Stopwatch]::StartNew()
  Start-Sleep -Milliseconds ([int]($Secs * 1000))
  $list1 = [WispThreadProbe]::Snapshot($Id)
  $elapsed = $watch.Elapsed.TotalSeconds
  if ($list1.Count -eq 0) { throw "进程 $Id 在计数窗口里退出了" }
  $proc.Refresh()
  $cpu1 = $proc.TotalProcessorTime.TotalMilliseconds
  $t1 = @{}
  foreach ($s in $list1) { $t1[$s.Tid] = $s; $null = Get-ThreadInfo $s.Tid }
  # 起始地址可能落在计数窗口里才加载的模块（例如 WebView2 的 DLL）：结束时再取一次模块表
  $modules = Get-ModuleTable $Id

  $rows = New-Object System.Collections.Generic.List[object]
  foreach ($tid in $t1.Keys) {
    $b = $t1[$tid]
    $a = $t0[$tid]
    $info = Get-ThreadInfo $tid
    $switches = if ($a) { $b.ContextSwitches - $a.ContextSwitches } else { $b.ContextSwitches }
    $cpu100ns = if ($a) { ($b.KernelTime + $b.UserTime) - ($a.KernelTime + $a.UserTime) } else { $b.KernelTime + $b.UserTime }
    $rows.Add([ordered]@{
        tid = $tid
        thread = $info.Name
        start = Format-Address $modules $info.Start
        switches = [long]$switches
        per_s = [Math]::Round($switches / $elapsed, 3)
        cpu_ms = [Math]::Round($cpu100ns / 10000.0, 1)
        new_in_window = -not $a
        state = Get-Label $ThreadStates $b.State
        wait_reason = Get-Label $WaitReasons $b.WaitReason
        priority = $b.Priority
      })
  }
  $sorted = @($rows | Sort-Object -Property @{ Expression = { $_.switches }; Descending = $true }, @{ Expression = { $_.tid } })
  $exited = @($t0.Keys | Where-Object { -not $t1.ContainsKey($_) } | ForEach-Object {
      $i = Get-ThreadInfo $_
      [ordered]@{ tid = $_; thread = $i.Name; start = Format-Address $modules $i.Start; switches_at_start = [long]$t0[$_].ContextSwitches }
    })
  # 旧口径（进程合计相减）：同一对快照上算，供对照
  $sum0 = [long]0
  foreach ($s in $t0.Values) { $sum0 += $s.ContextSwitches }
  $sum1 = [long]0
  foreach ($s in $t1.Values) { $sum1 += $s.ContextSwitches }
  # 同名（没有名字时按起始模块）的线程合并：线程池一类的线程会换 id
  $groups = [ordered]@{}
  foreach ($r in $sorted) {
    $key = if ($r.thread) { $r.thread } elseif ($r.start) { ($r.start -split '\+')[0] } else { '?' }
    if (-not $groups.Contains($key)) { $groups[$key] = [ordered]@{ group = $key; threads = 0; switches = 0; cpu_ms = 0.0 } }
    $g = $groups[$key]
    $g.threads++
    $g.switches += $r.switches
    $g.cpu_ms = [Math]::Round($g.cpu_ms + $r.cpu_ms, 1)
  }
  $byGroup = @($groups.Values | ForEach-Object { $_.per_s = [Math]::Round($_.switches / $elapsed, 3); $_ } |
      Sort-Object -Property @{ Expression = { $_.switches }; Descending = $true })
  $total = [long]0
  foreach ($r in $sorted) { $total += $r.switches }
  $os = Get-CimInstance Win32_OperatingSystem -Property Caption, Version
  $result = [ordered]@{
    tool = 'scripts/perf/windows-thread-wakeups.ps1'
    tool_version = $ScriptVersion
    label = $Name
    started_at = $started.ToString('yyyy-MM-ddTHH:mm:sszzz')
    host = [ordered]@{ os = "$($os.Caption) $($os.Version)"; cpus = [Environment]::ProcessorCount }
    pid = $Id
    executable = $proc.Path
    warmup_s = $Wait
    idle_s = [Math]::Round($elapsed, 1)
    process_cpu_ms = [Math]::Round($cpu1 - $cpu0, 1)
    process_cpu_percent = [Math]::Round(($cpu1 - $cpu0) / 1000.0 / $elapsed * 100, 4)
    threads = $t1.Count
    switches_total = $total
    switches_per_s = [Math]::Round($total / $elapsed, 3)
    legacy_process_sum = [ordered]@{
      note = '旧口径：两次快照的全部线程累计数之和相减（窗口里退出的线程此前的累计数被一起减掉，偏小、可为负）'
      switches = $sum1 - $sum0
      per_s = [Math]::Round(($sum1 - $sum0) / $elapsed, 3)
    }
    by_group = $byGroup
    per_thread = @($sorted | Where-Object { $_.switches -gt 0 -or $_.cpu_ms -gt 0 })
    quiet_threads = @($sorted | Where-Object { $_.switches -eq 0 -and $_.cpu_ms -eq 0 } |
        ForEach-Object { if ($_.thread) { $_.thread } elseif ($_.start) { $_.start } else { "tid $($_.tid)" } } | Sort-Object -Unique)
    exited_in_window = $exited
    stacks = $null
  }
  if ($StackFile) {
    $err = Save-Stacks $Id $StackFile
    $result.stacks = if ($err) { $err } else { $StackFile }
  }
  return $result
}

if ($SelfTest) {
  # 临时进程：主线程每 100 ms 醒一次（≈ 10 次/s），其余线程空闲
  $child = Start-Process -FilePath 'powershell.exe' -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile', '-NonInteractive',
  '-Command', 'while ($true) { [Threading.Thread]::Sleep(100) }'
  try {
    # Windows PowerShell 冷启动在繁忙的 runner 上要几秒：等它进入循环，计数窗口里只剩 100 ms 的节拍
    Start-Sleep -Seconds 6
    $r = Measure-Threads $child.Id 4 0 'self-test' ''
    $bad = New-Object System.Collections.Generic.List[string]
    if ($r.threads -lt 1) { $bad.Add('没有线程') }
    if ($r.switches_per_s -lt 5 -or $r.switches_per_s -gt 2000) { $bad.Add("合计 $($r.switches_per_s) 次/s，预期约 10") }
    $top = @($r.per_thread) | Select-Object -First 1
    if (-not $top -or $top.per_s -lt 5) { $bad.Add('最忙的线程不到 5 次/s') }
    if (-not $top -or -not $top.start) { $bad.Add('最忙的线程没有起始地址') }
    if (@($r.by_group).Count -lt 1) { $bad.Add('没有分组') }
    if ($null -eq $r.legacy_process_sum -or $null -eq $r.legacy_process_sum.per_s) { $bad.Add('没有旧口径的对照') }
    if ($bad.Count -gt 0) { throw "self-test 失败：$($bad -join '；')：$($r | ConvertTo-Json -Depth 6 -Compress)" }
    Write-Host "✓ windows-thread-wakeups.ps1 self-test 通过（$($r.threads) 个线程，合计 $($r.switches_per_s) 次/s，最忙 $($top.per_s) 次/s @ $($top.start)）"
  } finally {
    Stop-Process -Id $child.Id -Force -ErrorAction SilentlyContinue
  }
  exit 0
}

if ($ProcessId -le 0) { throw '用 -ProcessId 指定进程（或 -SelfTest）' }
$res = Measure-Threads $ProcessId $Seconds $Warmup $Label $Stacks
$json = $res | ConvertTo-Json -Depth 6
if ($Out) {
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent ([IO.Path]::GetFullPath($Out))) | Out-Null
  [IO.File]::WriteAllText([IO.Path]::GetFullPath($Out), $json + "`n", (New-Object System.Text.UTF8Encoding($false)))
} else {
  $json
}
