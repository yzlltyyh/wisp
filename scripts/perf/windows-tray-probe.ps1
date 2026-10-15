# scripts/perf/windows-tray-probe.ps1：vendor/tray-icon 补丁（托盘图标离开检测定时器）的复测工具
#
# 复测 Windows 托盘图标的「离开检测」定时器在悬停之后会不会停下。tray-icon 在光标悬停于图标上时起一个 15 ms 的定时器
# （WM_TIMER，跑在托盘窗口所在的 UI 线程上），用来发现光标离开（托盘图标收不到 WM_MOUSELEAVE）。上游 0.25.1 在光标停住
# 一拍之后就不再检查、也不再关它：光标停在图标上、或停住之后第一下移动就离开图标时，UI 线程从此每秒约 64 次唤醒。
# 对一个已启动的 Wisp（-ProcessId，缺省取最新启动的 wisp-desktop）：
#
#   1. 找到它的托盘窗口（类名 tray_icon_app）与图标矩形（Shell_NotifyIconGetRect，uID 0–63 依次试）；托盘窗口所属的线程
#      （GetWindowThreadProcessId）就是收 WM_TIMER 的 UI 线程；
#   2. 两个场景，每一段前后各取一次线程表（NtQuerySystemInformation，与 windows-thread-wakeups.ps1 同一份 C#），只数这个
#      UI 线程的上下文切换：
#      - rest：进入图标、在图标里动两下（hover 段：最后一下之后 1 s），停在图标上 -Rest 秒（rest 段），再一下跳到图标外，
#        1 s 后数 -Seconds 秒（after 段）。上游（以及补丁的第一版）停住期间约 64 次/秒；现在停住约 0.5 s 后停表；
#      - leave：进入、动两下、停 100 ms（定时器已走过几拍，光标仍在图标上），一下跳到图标外——中间没有任何一次在图标里的
#        移动；1 s 后数 -Seconds 秒（after 段）。上游从此常驻约 64 次/秒；补丁后下一拍发现光标已离开、关掉定时器；
#   3. 输入方式（-Mode）：
#      - Input（缺省）：SendInput 注入绝对坐标的鼠标移动，资源管理器照常把托盘回调（WM_MOUSEMOVE）发给 Wisp——最接近真实
#        悬停（「停住之后第一下就在图标外」用一次跳跃代替）。hover 段 UI 线程几乎没醒时判为「资源管理器没有转发」（没有
#        交互桌面、图标在溢出区……），结论不成立。GitHub 的 windows-latest runner 上这种方式
#        UI 线程一次都没醒，结论不成立——要在真机上用；
#      - Post：SetCursorPos + 直接向托盘窗口投递回调消息（WM_USER_TRAYICON = 6002，lParam = WM_MOUSEMOVE），不经资源管理器——
#        合成的输入，只用来确定性地走一遍定时器的状态机（runner 上可以用它复现）。
#
# 需要交互桌面（真机，或 GitHub 的 windows-latest runner）；会移动鼠标光标，结束时放回原处。探测期间不要动鼠标。
#
# 用法：./scripts/perf/windows-tray-probe.ps1 [-ProcessId <pid>] [-Mode Input|Post] [-Rest 3] [-Seconds 10]
#                                             [-MaxPerSecond 2] [-Out <文件>]
# 输出 JSON（-Out 缺省时打印到标准输出）。退出码：0 = 通过；1 = 某个 rest / after 段里 UI 线程超过 -MaxPerSecond 次/秒
# （定时器没停）；2 = 结论不成立：「动两下」之后光标不在图标上（每个 hover 段都记下了光标位置），或 Input 方式下资源管理器
# 没有把悬停转发给 Wisp。
[CmdletBinding()]
param(
  [int]$ProcessId = 0,
  [ValidateSet('Input', 'Post')][string]$Mode = 'Input',
  [ValidateRange(2, 600)][double]$Rest = 3,
  [ValidateRange(1, 3600)][double]$Seconds = 10,
  [ValidateRange(0, 1000)][double]$MaxPerSecond = 2,
  [string]$Out = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ScriptVersion = 1

# 线程表：与 windows-thread-wakeups.ps1 同一份 C#（WispThreadProbe），从那个脚本里取出来编译
if (-not ('WispThreadProbe' -as [type])) {
  $tool = Join-Path $PSScriptRoot 'windows-thread-wakeups.ps1'
  $m = [regex]::Match((Get-Content -Raw $tool), "(?s)@'\r?\n(.*?)\r?\n'@")
  if (-not $m.Success) { throw "$tool 里找不到 WispThreadProbe 的 C# 源码" }
  Add-Type -TypeDefinition $m.Groups[1].Value
}

if (-not ('WispTrayProbe' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class WispTrayProbe {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string name);
  [DllImport("user32.dll")]
  static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
  [DllImport("user32.dll")]
  static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")]
  static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll", SetLastError = true)]
  static extern uint SendInput(uint count, INPUT[] inputs, int size);
  [DllImport("user32.dll")]
  static extern int GetSystemMetrics(int index);
  [DllImport("user32.dll")]
  static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
  [DllImport("shell32.dll")]
  static extern int Shell_NotifyIconGetRect(ref NOTIFYICONIDENTIFIER id, out RECT rect);

  [StructLayout(LayoutKind.Sequential)] struct POINT { public int X; public int Y; }
  [StructLayout(LayoutKind.Sequential)] struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)]
  struct NOTIFYICONIDENTIFIER { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }
  [StructLayout(LayoutKind.Sequential)]
  struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  // INPUT 的联合体里 MOUSEINPUT 最大（x64 上 32 字节），只用鼠标输入时按它布局即可（Marshal.SizeOf = 40）
  [StructLayout(LayoutKind.Sequential)]
  struct INPUT { public uint type; public MOUSEINPUT mi; }

  const uint WM_USER_TRAYICON = 6002;   // tray-icon 的回调消息
  const int WM_MOUSEMOVE = 0x0200;
  const uint INPUT_MOUSE = 0;
  const uint MOUSEEVENTF_MOVE = 0x0001, MOUSEEVENTF_VIRTUALDESK = 0x4000, MOUSEEVENTF_ABSOLUTE = 0x8000;
  const int SM_XVIRTUALSCREEN = 76, SM_YVIRTUALSCREEN = 77, SM_CXVIRTUALSCREEN = 78, SM_CYVIRTUALSCREEN = 79;

  // 坐标一律按物理像素（Per-Monitor V2）：缩放不是 100 % 时，DPI 无感知的线程读到的光标 / 图标矩形是虚拟化过的
  static void Aware() { SetThreadDpiAwarenessContext(new IntPtr(-4)); }

  // 属于 pid 的 tray_icon_app 顶层窗口；没有时为 0
  public static IntPtr TrayWindow(int pid) {
    IntPtr h = IntPtr.Zero;
    while ((h = FindWindowExW(IntPtr.Zero, h, "tray_icon_app", null)) != IntPtr.Zero) {
      uint p;
      GetWindowThreadProcessId(h, out p);
      if (p == (uint)pid) return h;
    }
    return IntPtr.Zero;
  }

  public static int WindowThread(IntPtr hwnd) {
    uint p;
    return (int)GetWindowThreadProcessId(hwnd, out p);
  }

  // { uID, left, top, right, bottom }；图标不在任务栏上（隐藏、溢出区收起）或读不到时为 null
  public static int[] IconRect(IntPtr hwnd) {
    Aware();
    for (uint id = 0; id < 64; id++) {
      var nid = new NOTIFYICONIDENTIFIER { cbSize = (uint)Marshal.SizeOf(typeof(NOTIFYICONIDENTIFIER)), hWnd = hwnd, uID = id };
      RECT r;
      if (Shell_NotifyIconGetRect(ref nid, out r) == 0) return new[] { (int)id, r.Left, r.Top, r.Right, r.Bottom };
    }
    return null;
  }

  public static int[] Cursor() {
    Aware();
    POINT p;
    return GetCursorPos(out p) ? new[] { p.X, p.Y } : null;
  }

  // 真实输入：注入一次绝对坐标（虚拟桌面）的鼠标移动
  public static bool InputMove(int x, int y) {
    Aware();
    int vx = GetSystemMetrics(SM_XVIRTUALSCREEN), vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    int vw = Math.Max(2, GetSystemMetrics(SM_CXVIRTUALSCREEN)), vh = Math.Max(2, GetSystemMetrics(SM_CYVIRTUALSCREEN));
    var mi = new MOUSEINPUT {
      dx = (int)Math.Round((x - vx) * 65535.0 / (vw - 1)),
      dy = (int)Math.Round((y - vy) * 65535.0 / (vh - 1)),
      dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    };
    var inputs = new[] { new INPUT { type = INPUT_MOUSE, mi = mi } };
    return SendInput(1, inputs, Marshal.SizeOf(typeof(INPUT))) == 1;
  }

  public static bool SetCursor(int x, int y) {
    Aware();
    return SetCursorPos(x, y);
  }

  // 合成：直接向托盘窗口投递「光标在图标上移动」的回调（不经资源管理器）
  public static bool PostTrayMove(IntPtr hwnd, int uid) {
    return PostMessageW(hwnd, WM_USER_TRAYICON, new IntPtr(uid), new IntPtr(WM_MOUSEMOVE));
  }
}
'@
}

if ($ProcessId -le 0) {
  $p = Get-Process -Name 'wisp-desktop' -ErrorAction SilentlyContinue | Sort-Object StartTime -Descending | Select-Object -First 1
  if (-not $p) { throw '没有找到运行中的 wisp-desktop；用 -ProcessId 指定' }
  $ProcessId = $p.Id
}
if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { throw "进程 $ProcessId 不存在" }

$hwnd = [WispTrayProbe]::TrayWindow($ProcessId)
if ($hwnd -eq [IntPtr]::Zero) { throw "进程 $ProcessId 没有 tray_icon_app 窗口（托盘还没建好？）" }
$tid = [WispTrayProbe]::WindowThread($hwnd)
$rect = [WispTrayProbe]::IconRect($hwnd)
if (-not $rect) { throw "读不到进程 $ProcessId 的托盘图标矩形（图标被隐藏或收在溢出区？把它拖到任务栏上再试）" }
$uid, $left, $top, $right, $bottom = $rect
$cx = [int](($left + $right) / 2)
$cy = [int](($top + $bottom) / 2)
# 图标外的落点：图标正上方 200 px（屏幕顶端不够时取正下方）——一下跳过去，中间不经过图标
$awayY = if ($top -ge 220) { $top - 200 } else { $bottom + 200 }
$away = @($cx, $awayY)
$origin = [WispTrayProbe]::Cursor()

function Get-Switches {
  $t = [WispThreadProbe]::Snapshot($ProcessId) | Where-Object { $_.Tid -eq $tid } | Select-Object -First 1
  if (-not $t) { throw "进程 $ProcessId 的 UI 线程 $tid 不见了（进程退出了？）" }
  return [long]$t.ContextSwitches
}

function Move-Cursor([int]$X, [int]$Y, [switch]$OverIcon) {
  if ($Mode -eq 'Input') {
    if (-not [WispTrayProbe]::InputMove($X, $Y)) { throw "SendInput 失败（没有交互桌面？）" }
  } else {
    $null = [WispTrayProbe]::SetCursor($X, $Y)
    if ($OverIcon) { $null = [WispTrayProbe]::PostTrayMove($hwnd, $uid) }
  }
}

# 光标此刻是否在图标矩形里（记进结果：分清「光标没移过去」与「移过去了、资源管理器没有转发」）
function Test-CursorOnIcon {
  $c = [WispTrayProbe]::Cursor()
  return [ordered]@{
    x = if ($c) { $c[0] } else { $null }
    y = if ($c) { $c[1] } else { $null }
    on_icon = [bool]($c -and $c[0] -ge $left -and $c[0] -lt $right -and $c[1] -ge $top -and $c[1] -lt $bottom)
  }
}

# 进入图标并在图标里动两下（tray-icon：第一下 Enter，之后的 Move 起 15 ms 定时器）；返回最后一下之后的光标位置
function Enter-Icon {
  Move-Cursor $cx $cy -OverIcon
  Start-Sleep -Milliseconds 40
  Move-Cursor ($cx + 2) $cy -OverIcon
  Start-Sleep -Milliseconds 40
  Move-Cursor ($cx + 4) $cy -OverIcon
  return Test-CursorOnIcon
}

$segments = New-Object System.Collections.Generic.List[object]
function Add-Segment([string]$Scenario, [string]$Name, [long]$Switches, [double]$Secs, $Cursor = $null) {
  $row = [ordered]@{
    scenario = $Scenario
    segment = $Name
    seconds = [Math]::Round($Secs, 2)
    switches = $Switches
    per_s = [Math]::Round($Switches / [Math]::Max(0.001, $Secs), 3)
    cursor = $Cursor
  }
  $segments.Add($row)
  Write-Host ('{0,-6} {1,-6} {2,6:0.0} s  {3,5} 次  {4,8:0.000} 次/s' -f $Scenario, $Name, $row.seconds, $Switches, $row.per_s)
}

function Measure-Quiet([string]$Scenario, [string]$Name, [double]$Secs) {
  $a = Get-Switches
  $w = [Diagnostics.Stopwatch]::StartNew()
  Start-Sleep -Milliseconds ([int]($Secs * 1000))
  $b = Get-Switches
  Add-Segment $Scenario $Name ($b - $a) $w.Elapsed.TotalSeconds
}

Write-Host ("托盘窗口 0x{0:x}（UI 线程 {1}），图标 uID {2}：({3}, {4})–({5}, {6})；方式 {7}" -f $hwnd.ToInt64(), $tid, $uid, $left, $top, $right, $bottom, $Mode)
try {
  # 起点：光标先放到图标外，UI 线程安静下来
  Move-Cursor $away[0] $away[1]
  Start-Sleep -Seconds 1
  Measure-Quiet 'base' 'idle' 3

  # rest：进入、动两下、停在图标上
  $a = Get-Switches
  $w = [Diagnostics.Stopwatch]::StartNew()
  $restCursor = Enter-Icon
  Start-Sleep -Seconds 1
  $b = Get-Switches
  Add-Segment 'rest' 'hover' ($b - $a) $w.Elapsed.TotalSeconds $restCursor
  $hoverSwitches = $b - $a
  Measure-Quiet 'rest' 'rest' $Rest
  Move-Cursor $away[0] $away[1]
  Start-Sleep -Seconds 1
  Measure-Quiet 'rest' 'after' $Seconds

  # leave：进入、动两下、停 100 ms，一下跳到图标外
  $a = Get-Switches
  $w = [Diagnostics.Stopwatch]::StartNew()
  $leaveCursor = Enter-Icon
  Start-Sleep -Milliseconds 100
  Move-Cursor $away[0] $away[1]
  Start-Sleep -Seconds 1
  $b = Get-Switches
  Add-Segment 'leave' 'hover' ($b - $a) $w.Elapsed.TotalSeconds $leaveCursor
  Measure-Quiet 'leave' 'after' $Seconds
} finally {
  if ($origin) { $null = [WispTrayProbe]::SetCursor($origin[0], $origin[1]) }
}

$bad = @($segments | Where-Object { $_.segment -in @('rest', 'after') -and $_.per_s -gt $MaxPerSecond })
# Input：hover 段（移动 + 1 s）里 UI 线程至少要醒几次（回调本身 + 定时器的几拍），否则资源管理器没有把悬停转发给 Wisp
# 两种方式都要求「动两下」之后光标确实停在图标上——否则 rest 段的 0 可能只是定时器第一拍就发现光标不在图标里
$delivered = $restCursor.on_icon -and $leaveCursor.on_icon -and ($Mode -eq 'Post' -or $hoverSwitches -ge 5)
$verdict = if (-not $delivered) { 'inconclusive' } elseif ($bad.Count -gt 0) { 'timer-kept-running' } else { 'ok' }
$result = [ordered]@{
  tool = 'scripts/perf/windows-tray-probe.ps1'
  tool_version = $ScriptVersion
  mode = $Mode
  pid = $ProcessId
  executable = (Get-Process -Id $ProcessId).Path
  ui_thread = $tid
  icon = [ordered]@{ uid = $uid; left = $left; top = $top; right = $right; bottom = $bottom }
  rest_s = $Rest
  after_s = $Seconds
  max_per_s = $MaxPerSecond
  hover_delivered = $delivered
  verdict = $verdict
  segments = $segments
}
$json = $result | ConvertTo-Json -Depth 5
if ($Out) {
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent ([IO.Path]::GetFullPath($Out))) | Out-Null
  [IO.File]::WriteAllText([IO.Path]::GetFullPath($Out), $json + "`n", (New-Object System.Text.UTF8Encoding($false)))
} else {
  $json
}
switch ($verdict) {
  'ok' { Write-Host "✓ 托盘悬停之后 UI 线程安静（$Mode）" ; exit 0 }
  'inconclusive' {
    $why = if (-not $restCursor.on_icon) { "光标没有移到图标上（停在 $($restCursor.x), $($restCursor.y)：注入的输入没有生效？）" }
    else { '光标在图标上，但资源管理器没有把悬停转发给 Wisp' }
    Write-Warning "hover 段 UI 线程只醒了 $hoverSwitches 次：$why，结论不成立"
    exit 2
  }
  default { Write-Warning "离开检测定时器没有停：$(($bad | ForEach-Object { '{0}/{1} {2} 次/s' -f $_.scenario, $_.segment, $_.per_s }) -join '；')"; exit 1 }
}
