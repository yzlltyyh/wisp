#!/usr/bin/env bash
# scripts/perf/measure-macos.sh
#
# macOS 空闲测量：主进程与 WebKit 进程各自的内存、累计 CPU、唤醒次数，按秒采样，汇总为
# wisp.perf/1 性能记录（格式见 scripts/perf/README.md）。统计与 schema 校验由 record.mjs 完成，本脚本只负责采样。
# 内存口径是 phys_footprint（proc_pid_rusage，与活动监视器的「内存」一致），记录里仍叫 rss_*，
# 由 idle.memory_metric 标明；没有系统 python3 时退回 ps 的 RSS（memory_metric = rss，与 phys_footprint 的基线不比较）。
#
# 用法：
#   scripts/perf/measure-macos.sh [选项]
#     --pid <pid>            测量已经在运行的进程（默认：找可执行文件名为 wisp-desktop 的进程）
#     --app <路径>           先启动再测量，结束后退出它：Wisp.app（或 .app 里的可执行文件）经 LaunchServices
#                            （open -n -g）启动；.app 之外的裸可执行文件只能从终端启动，WebView 归属是近似的（见下）
#     --home <目录>          （与 --app 一起用）以隔离的 HOME 启动（open --env HOME=…）：不读写用户真实的配置与剪贴板历史，
#                            全新配置不会弹引导窗口污染测量。默认在临时目录里新建一个，结束时删除；传 keep 保留
#                            用户真实 HOME（不推荐）
#     --settings <preset|none|文件>  （隔离 HOME 时）启动前写入的 settings.json。默认 preset：exercise.mjs --write-settings 的
#                            测量设置——关闭更新检查（启动 30 s 后与面板显示时的一次性联网不落进测量）、不写登录项、日志
#                            级别 debug（exercise.mjs 与 record.mjs 靠面板事件核对 exercise 的最后状态）、空闲卸载取本平台
#                            新配置的默认值；none：不写（全新配置）；文件：原样复制。--home keep 时一律不写
#     --app-args <参数>      （与 --app 一起用）传给 Wisp 的参数（open --args），默认 "--autostart"（静默启动、不弹窗）。
#                            注意：与正在运行的 Wisp 同 identifier 时会经单实例锁转发走——并行测量请用
#                            `pnpm tauri build --config '{"identifier":"io.github.yzlltyyh.wisp.perf"}'` 换一个 identifier
#     --idle <秒>            空闲采样时长，默认 600
#     --warmup <秒>          启动后等待多久再开始采样（--app 时默认 30，其余默认 0）
#     --exercise <命令>      预热之后、采样之前执行一次（bash -e -c：其中任何一条命令失败都算 exercise 失败；环境变量
#                            WISP_PID 为被测进程、WISP_BIN 为它的可执行文件、WISP_LOG 为它的应用日志），用来先用一次 Wisp，
#                            测「用过之后」的内存、CPU 与唤醒。推荐 "node scripts/perf/exercise.mjs"（灌剪贴板、依次唤出 /
#                            关闭输入与剪贴板面板，并由 debug 日志核对结束时面板确实已隐藏，见该脚本头注释）
#     --settle <秒>          --exercise 之后等待多久再采样，默认 30（让隐藏后的收尾、WebKit 的内存回收做完）。静置之后脚本还要
#                            约 3–4 s 读 WebKit 归属与唤醒计数的起始值，采样的实际窗口以记录里的 idle.session.window_s
#                            （exercise 结束后的秒数）为准。macOS 的面板在最后一次隐藏 45 s 后回收
#                            （windows::manager::panel_recycle_after；exercise.mjs 的最后一次隐藏在 exercise 结束前约 1.8 s）：
#                            「回收前」的窗口要在 exercise 后约 43 s 之前结束（用 --window-end 钉住终点），「回收后」要从
#                            45 s 之后开始；记录的 idle.session.state 由应用日志核对
#     --window-end <秒>      （与 --exercise 一起用）采样最晚在 exercise 结束后这么多秒结束：采样的起点随脚本开销浮动，
#                            终点钉在固定时刻，采样时长相应缩短（可用的窗口不足 10 s 时报错）
#     --app-log <文件>       被测应用的日志（默认：--app 启动时为 <HOME>/Library/Logs/wisp/wisp.log，HOME 为隔离目录或
#                            真实 HOME）。交给 exercise（WISP_LOG）与 record.mjs：得出 idle.session（exercise 唤出了哪些
#                            视图、采样窗口里面板是否已隐藏、是回收前还是回收后）与本次启动的 latency 段
#     --log-out <文件>       测量结束后把应用日志复制到这里（CI 上传产物用；隔离 HOME 会在结束时删除）
#     --interval <秒>        采样间隔，默认 1
#     --wakeups <方法>       auto（默认：免密 sudo 可用时用 powermetrics，否则 top）| top | powermetrics | none
#     --build <release|debug> 构建类型（默认按路径推断）
#     --latency-log <文件>   同时从开发者模式日志统计延迟（latency-report.mjs）
#     --smoke <文件>         合并 smoke-report.json（可重复：release、debug 各一份，见 README「基线」）
#     --out <文件>           输出路径（默认 target/perf/<date>-macos.json）
#     --dry-run              不需要 Wisp：编译一个最小的 WKWebView 探针 .app（macos-webview-probe.swift），经
#                            LaunchServices 启动后采样 3 秒，校验 WebView 归属与 WebView 的 RSS / CPU / 唤醒；再从终端
#                            直接启动一次探针，校验近似归属（CI 用；没有 swiftc 时退回只测一个临时 sleep 进程）
#
# WebView 归属：WebKit 的 XPC 进程（com.apple.WebKit.*）父进程是 launchd，按「负责进程」匹配
# （macos-webkit.py）。经 LaunchServices 启动的 Wisp 是它自己的负责进程，匹配精确（webview_attribution =
# responsible）；从终端 / IDE 启动时（./target/release/wisp-desktop、pnpm dev），Wisp 与它的 WebKit 进程的负责进程
# 都是终端，只能按「同一负责进程且启动不早于 Wisp」近似匹配（responsible-approx，门禁告警）。一个 WebKit 进程都
# 找不到时 WebView 指标记为 null（不是 0），门禁据此判定「未验证」。
# CPU：主进程与每个 WebKit 进程的累计 CPU 时间（ps 的 TIME）取相邻样本之差，比逐秒的 %cpu 更准确。
# 唤醒：top 的 IDLEW 是累计的「把 CPU 从空闲唤醒」次数，取首尾之差；powermetrics 需要 sudo，取 tasks 采样器的
# idle_wakeups——与 IDLEW、活动监视器的「空闲唤醒」同一口径（intr_wakeups 已经包含它，两者相加会重复计数）。
# 主进程与 WebKit 进程分别统计。
# 采样按绝对时刻对齐（第 i 个样本在开始后 i × interval 秒），循环本身的耗时不会累积成漂移：--idle 600 的采样窗口
# 就是 600 秒，与 powermetrics 的窗口一致。毫秒时间戳用系统自带的 perl 取（每次约 5 ms，node 约 45 ms），
# 空闲测量期间脚本自己引入的负载尽量小。
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

pid=""
app=""
idle=600
warmup=""
interval=1
wakeups_mode=auto
build=""
latency_log=""
smokes=()
out=""
dry_run=0
exercise=""
settle=30
window_end=""
home=""
settings_opt=preset
app_args="--autostart"
app_log=""
log_out=""
launch_ms=""

die() {
  echo "measure-macos: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --pid) pid="${2:?}"; shift 2 ;;
    --app) app="${2:?}"; shift 2 ;;
    --idle) idle="${2:?}"; shift 2 ;;
    --warmup) warmup="${2:?}"; shift 2 ;;
    --interval) interval="${2:?}"; shift 2 ;;
    --wakeups) wakeups_mode="${2:?}"; shift 2 ;;
    --build) build="${2:?}"; shift 2 ;;
    --latency-log) latency_log="${2:?}"; shift 2 ;;
    --smoke) smokes+=("${2:?}"); shift 2 ;;
    --out) out="${2:?}"; shift 2 ;;
    --exercise) exercise="${2:?}"; shift 2 ;;
    --settle) settle="${2:?}"; shift 2 ;;
    --window-end) window_end="${2:?}"; shift 2 ;;
    --home) home="${2:?}"; shift 2 ;;
    --settings) settings_opt="${2:?}"; shift 2 ;;
    --app-log) app_log="${2:?}"; shift 2 ;;
    --log-out) log_out="${2:?}"; shift 2 ;;
    --app-args) app_args="${2-}"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help)
      awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
      exit 0
      ;;
    *) die "未知参数 $1（--help 查看用法）" ;;
  esac
done

[ "$(uname -s)" = Darwin ] || die "只能在 macOS 上运行"
case "$idle" in '' | *[!0-9]*) die "--idle 必须是正整数秒" ;; esac
case "$interval" in '' | *[!0-9]*) die "--interval 必须是正整数秒" ;; esac
case "$settle" in '' | *[!0-9]*) die "--settle 必须是非负整数秒" ;; esac
case "$window_end" in '' ) ;; *[!0-9]*) die "--window-end 必须是正整数秒" ;; esac
[ "$idle" -ge 1 ] && [ "$interval" -ge 1 ] || die "--idle / --interval 必须 ≥ 1"
case "$wakeups_mode" in auto | top | powermetrics | none) ;; *) die "--wakeups 只能是 auto / top / powermetrics / none" ;; esac
for tool in ps top node open; do command -v "$tool" > /dev/null || die "缺少 $tool"; done

# 当前时刻（Unix 毫秒）。系统自带 perl 时用它（进程很轻）；没有时退回 node
if [ -x /usr/bin/perl ] && /usr/bin/perl -MTime::HiRes=time -e 1 2> /dev/null; then
  now_ms() { /usr/bin/perl -MTime::HiRes=time -e 'printf "%d", time * 1000'; }
else
  now_ms() { node -e 'process.stdout.write(String(Date.now()))'; }
fi

has_python=1
/usr/bin/python3 -I -c 'import ctypes' 2> /dev/null || has_python=0

tmp="$(mktemp -d "${TMPDIR:-/tmp}/wisp-perf.XXXXXX")"
launched=""     # 本脚本启动的进程，结束时退出它
probe_direct="" # dry-run 第二步从终端直接启动的探针
pm_pid=""

# 退出一个进程（先 SIGTERM，5 秒内没退出再 SIGKILL）；经 open 启动的进程不是本 shell 的子进程，只能轮询
stop_process() {
  local p="$1" k
  kill "$p" 2> /dev/null || return 0
  for ((k = 0; k < 50; k++)); do
    kill -0 "$p" 2> /dev/null || return 0
    sleep 0.1
  done
  kill -9 "$p" 2> /dev/null || true
}

cleanup() {
  if [ -n "$pm_pid" ]; then kill "$pm_pid" 2> /dev/null || true; fi
  if [ -n "$probe_direct" ]; then stop_process "$probe_direct"; fi
  if [ -n "$launched" ]; then
    stop_process "$launched"
  elif [ -f "$tmp/launch.exe" ]; then
    # launch_bundle 在命令替换的子 shell 里失败（die）时 launched 没有被赋值，但 open 可能已经把应用拉起来了：按 open 之前的
    # 进程快照，停掉之后新出现的同一可执行文件的进程——否则应用留在后台，还会把下面删掉的临时 HOME 重新建出来
    local exe before p
    exe="$(cat "$tmp/launch.exe")"
    before="$(cat "$tmp/launch.before" 2> /dev/null || true)"
    for p in $(running_pids "$exe"); do
      case "$before" in
        *" $p "*) ;;
        *) stop_process "$p" ;;
      esac
    done
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# 可执行文件完整路径（已解析符号链接，与 ps 的 comm 一致）等于 $1 的进程。macOS 的默认文件系统大小写不敏感，而 pwd -P
# 保留调用方输入时的大小写（从 ~/projects/… 运行、ps 报的是 ~/Projects/…）：两边都转成小写再比较
running_pids() {
  ps -axo pid=,comm= | awk -v exe="$1" '{ p = $1; sub(/^[ \t]*[0-9]+[ \t]+/, ""); if (tolower($0) == tolower(exe)) print p }'
}

# 经 LaunchServices 启动 .app（open -n -g：新实例、不抢前台），输出新进程的 pid。这样启动的应用是它自己的
# 「负责进程」，WebKit 进程能精确归属；直接执行 Contents/MacOS 里的可执行文件则不是（负责进程会是终端）
launch_bundle() {
  local bundle="$1" name exe before p k
  name="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$bundle/Contents/Info.plist" 2> /dev/null || true)"
  [ -n "$name" ] || die "$bundle 不是有效的 .app（读不到 CFBundleExecutable）"
  exe="$(cd "$bundle/Contents/MacOS" 2> /dev/null && pwd -P)/$name"
  [ -x "$exe" ] || die "$exe 不可执行"
  before=" $(running_pids "$exe" | tr '\n' ' ') "
  # 给 cleanup 留底（本函数在命令替换的子 shell 里运行，变量传不回去）：失败退出时据此停掉 open 拉起的进程
  printf '%s\n' "$exe" > "$tmp/launch.exe"
  printf '%s\n' "$before" > "$tmp/launch.before"
  local open_args=(-n -g --stdout "$tmp/app.out.log" --stderr "$tmp/app.err.log")
  if [ -n "$launch_home" ]; then open_args+=(--env "HOME=$launch_home"); fi
  open_args+=("$bundle")
  if [ -n "$app_args" ]; then
    # shellcheck disable=SC2206 # 有意按空白拆分成多个参数
    local extra=($app_args)
    open_args+=(--args "${extra[@]}")
  fi
  open "${open_args[@]}" || die "open $bundle 失败"
  for ((k = 0; k < 150; k++)); do
    for p in $(running_pids "$exe"); do
      case "$before" in
        *" $p "*) ;;
        *)
          echo "$p"
          return 0
          ;;
      esac
    done
    sleep 0.1
  done
  die "15 秒内没有找到 $exe 的新进程（open 启动失败？）"
}

# 构建 dry-run 用的 WKWebView 探针 .app，输出 .app 路径
build_probe() {
  local bundle="$tmp/WispPerfProbe.app"
  mkdir -p "$bundle/Contents/MacOS"
  cat > "$bundle/Contents/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>wisp-webview-probe</string>
<key>CFBundleIdentifier</key><string>io.github.yzlltyyh.wisp.perf-probe</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
  swiftc -O -o "$bundle/Contents/MacOS/wisp-webview-probe" "$here/macos-webview-probe.swift" > "$tmp/swiftc.log" 2>&1 ||
    die "编译 WKWebView 探针失败：$(tail -5 "$tmp/swiftc.log")"
  echo "$bundle"
}

probe=0
if [ "$dry_run" = 1 ]; then
  app_args=""
  idle=3
  interval=1
  [ "$wakeups_mode" = auto ] && wakeups_mode=top
  [ -z "$out" ] && out="$tmp/dry-run.json"
  [ -z "$build" ] && build=release
  # 顺带走一遍 --exercise 的流程（WISP_PID / WISP_BIN 已设置、命令成功、进程仍在、记录里有会话信息）
  if [ -z "$exercise" ]; then
    # shellcheck disable=SC2016 # 由 exercise 的 bash 展开
    exercise='kill -0 "$WISP_PID" && [ -n "$WISP_BIN" ]'
    settle=0
  fi
  if command -v swiftc > /dev/null && [ "$has_python" = 1 ]; then
    probe=1
    app="$(build_probe)"
    warmup=2
    echo "dry-run：已编译 WKWebView 探针，经 LaunchServices 启动后采样 ${idle} 秒"
  else
    sleep 120 &
    launched=$!
    disown "$launched"
    pid=$launched
    app=""
    warmup=0
    echo "! dry-run：没有 swiftc 或 python3，退回对临时 sleep 进程采样 ${idle} 秒（不验证 WebView 归属）" >&2
  fi
fi

if [ -n "$window_end" ] && [ -z "$exercise" ]; then die "--window-end 要与 --exercise 一起用（终点按 exercise 结束的时刻算）"; fi

# --app 启动时用的 HOME：默认临时目录里新建一个（隔离用户的真实配置与剪贴板历史），--home keep 用真实 HOME
launch_home=""
if [ -n "$app" ] && [ "$probe" = 0 ]; then
  case "$home" in
    keep) launch_home="" ;;
    "")
      launch_home="$tmp/home"
      mkdir -p "$launch_home"
      ;;
    *)
      mkdir -p "$home" || die "无法创建 --home $home"
      launch_home="$home"
      ;;
  esac
  [ -n "$launch_home" ] && echo "以隔离的 HOME 启动：${launch_home}（--home keep 改用真实 HOME）"
  # 启动前写入设置（只在隔离 HOME 里；不碰真实配置）：app::paths 的 config_dir 是 ~/Library/Application Support/wisp
  if [ -n "$launch_home" ]; then
    settings_file="$launch_home/Library/Application Support/wisp/settings.json"
    case "$settings_opt" in
      none) ;;
      preset) node "$here/exercise.mjs" --write-settings "$settings_file" > /dev/null || die "写入测量设置失败" ;;
      *)
        [ -f "$settings_opt" ] || die "--settings $settings_opt 不存在"
        mkdir -p "$(dirname "$settings_file")"
        cp "$settings_opt" "$settings_file"
        ;;
    esac
    [ "$settings_opt" = none ] || echo "已写入设置：${settings_opt}（${settings_file}）"
  fi
fi

if [ -n "$app" ]; then
  bundle=""
  if [ -d "$app" ] && [ -f "$app/Contents/Info.plist" ]; then
    bundle="$app"
  else
    case "$app" in
      */Contents/MacOS/*)
        candidate="${app%/Contents/MacOS/*}"
        if [ -f "$candidate/Contents/Info.plist" ]; then bundle="$candidate"; fi
        ;;
    esac
  fi
  launch_ms="$(now_ms)"
  if [ -n "$bundle" ]; then
    pid="$(launch_bundle "$bundle")"
    launched=$pid
    rm -f "$tmp/launch.exe" "$tmp/launch.before"
    echo "已由 LaunchServices 启动 ${bundle}（pid ${pid}）"
  else
    [ -x "$app" ] || die "$app 不可执行"
    # shellcheck disable=SC2086 # 有意按空白拆分成多个参数
    HOME="${launch_home:-$HOME}" "$app" $app_args > "$tmp/app.out.log" 2>&1 &
    launched=$!
    disown "$launched"
    pid=$launched
    echo "! 从终端直接启动 ${app}（pid ${pid}）：WebKit 进程的负责进程是终端，WebView 归属只能近似（打成 .app 再用 --app 可精确归属）" >&2
  fi
  [ -z "$warmup" ] && warmup=30
  echo "等待 ${warmup} 秒预热"
elif [ -z "$pid" ]; then
  pid="$(pgrep -nx wisp-desktop || true)"
  [ -n "$pid" ] || die "没有找到运行中的 wisp-desktop；用 --pid 或 --app 指定"
fi
[ -z "$warmup" ] && warmup=0
kill -0 "$pid" 2> /dev/null || die "进程 $pid 不存在"
# 应用日志（app::paths::log_dir：macOS 为 ~/Library/Logs/wisp/wisp.log，~ 跟随启动时的 HOME）
if [ -z "$app_log" ] && [ -n "$app" ] && [ "$probe" = 0 ]; then
  app_log="${launch_home:-$HOME}/Library/Logs/wisp/wisp.log"
fi

if [ -z "$build" ]; then
  case "$(ps -o comm= -p "$pid" || true)" in
    */debug/*) build=debug ;;
    *) build=release ;;
  esac
fi

sleep "$warmup"
kill -0 "$pid" 2> /dev/null || die "进程 $pid 在预热期间退出了"

# 会话之后的空闲（用过面板之后 WebKit 进程必须回到零唤醒）：先执行一次 --exercise，再等 --settle 秒
exercise_start_ms=""
exercise_end_ms=""
if [ -n "$exercise" ]; then
  echo "执行 --exercise：$exercise"
  exercise_start_ms="$(now_ms)"
  # -e：exercise 里任何一条命令失败都算失败（否则 "a; sleep 1" 只看最后一条，--trigger 转发失败也会「成功」）
  WISP_PID="$pid" WISP_BIN="$(ps -o comm= -p "$pid")" WISP_LOG="$app_log" bash -e -c "$exercise" ||
    die "--exercise 失败（退出码 $?）"
  exercise_end_ms="$(now_ms)"
  echo "等待 ${settle} 秒（会话后的收尾）"
  sleep "$settle"
  kill -0 "$pid" 2> /dev/null || die "进程 $pid 在 --exercise 之后退出了"
fi

# WebKit 进程：设置 wv_mode（exact / approx；没有 python3 时为空）与 wv_pids（逗号分隔，没有时为空），
# wv_all 累计采样期间出现过的全部 WebKit 进程
wv_mode=""
wv_pids=""
wv_all=""
# 诊断：采样期间出现过的 WebKit 进程的「pid=类型」（GPU / Networking / WebContent），进程第一次出现时记一次，写进记录的 notes——
# runner 上进程数不到下限时据此判断缺的是哪一个（Apple 虚拟机上没有 GPU 进程）
wv_names=""
scan_webkit() {
  local mode="" owner="" pids="" p n
  wv_mode=""
  wv_pids=""
  [ "$has_python" = 1 ] || return 0
  read -r mode owner pids < <(/usr/bin/python3 -I "$here/macos-webkit.py" "$1" 2> /dev/null || true) || true
  wv_mode="$mode"
  : "$owner"
  if [ -n "$pids" ] && [ "$pids" != - ]; then
    wv_pids="$pids"
    for p in ${pids//,/ }; do
      case ",$wv_all," in *",$p,"*) continue ;; esac
      n="$(ps -o comm= -p "$p" 2> /dev/null || true)"
      n="${n##*/}"
      wv_names="${wv_names:+$wv_names, }$p=${n#com.apple.WebKit.}"
    done
    wv_all="$(printf '%s\n%s\n' "${wv_all//,/$'\n'}" "${pids//,/$'\n'}" | awk 'NF && !seen[$0]++' | paste -sd, -)"
  fi
}

# top 的 IDLEW：对给定 pid（逗号分隔）输出 "pid:累计次数,…"；一个都取不到时输出 -
top_idlew() {
  local args=() p
  for p in ${1//,/ }; do args+=(-pid "$p"); done
  if [ ${#args[@]} -eq 0 ]; then
    echo -
    return 0
  fi
  top -l 1 "${args[@]}" -stats pid,idlew 2> /dev/null | awk '
    $1 == "PID" { body = 1; next }
    body && $1 ~ /^[0-9]+$/ {
      v = $2; m = 1
      if (v ~ /[Kk]\+?$/) m = 1000; else if (v ~ /[Mm]\+?$/) m = 1000000
      gsub(/[^0-9.]/, "", v)
      out = out (out == "" ? "" : ",") $1 ":" int(v * m)
    }
    END { print (out == "" ? "-" : out) }'
}

# "pid:n,…" 中某个 pid 的值（没有时为 -）
value_of() {
  tr ',' '\n' <<< "$2" | awk -F: -v p="$1" '$1 == p { v = $2 } END { print (v == "" ? "-" : v) }'
}

raw="$tmp/samples.txt"
{
  echo "# measure-macos.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "meta os macos"
  echo "meta os_version $(sw_vers -productVersion)"
  echo "meta arch $(uname -m)"
  echo "meta machine $(sysctl -n hw.model 2> /dev/null || echo unknown)"
  echo "meta build $build"
  if [ "$has_python" = 1 ]; then echo "meta memory_metric phys_footprint"; else echo "meta memory_metric rss"; fi
  if [ -n "$launch_ms" ]; then echo "meta launch_ms $launch_ms"; fi
  if [ -n "$exercise" ]; then
    echo "meta exercise_start_ms $exercise_start_ms"
    echo "meta exercise_end_ms $exercise_end_ms"
    echo "meta settle_s $settle"
    # 使用画像写进记录：exercise 的命令原样记下（换行换成空格）
    echo "meta note 会话后空闲：采样前执行了 --exercise（${exercise//$'\n'/ }）并静置 ${settle} 秒"
  fi
} > "$raw"

scan_webkit "$pid"
case "$wv_mode" in
  exact) echo "meta webview_attribution responsible" >> "$raw" ;;
  approx)
    echo "meta webview_attribution responsible-approx" >> "$raw"
    echo "meta note Wisp（pid ${pid}）不是自己的负责进程（从终端 / IDE 启动）：WebKit 进程按「同一负责进程且启动不早于 Wisp」近似匹配，可能混入同一终端随后启动的其他 WebKit 应用；用 --app <Wisp.app> 经 LaunchServices 启动可精确归属" >> "$raw"
    ;;
  *) echo "meta note 没有可用的系统 python3（ctypes）：无法确定 WebKit 进程归属，WebView 的 RSS / CPU / 唤醒记为 null" >> "$raw" ;;
esac

if [ "$wakeups_mode" = auto ]; then
  if [ "$(id -u)" = 0 ] || sudo -n true 2> /dev/null; then wakeups_mode=powermetrics; else wakeups_mode=top; fi
fi
# --window-end：终点钉在 exercise 结束后的固定时刻（上面的 WebKit 归属与起始读数花掉的几秒从采样时长里扣，
# powermetrics 的窗口随之缩短）；采样循环到点即停
deadline_ms=""
if [ -n "$window_end" ]; then
  deadline_ms=$((exercise_end_ms + window_end * 1000))
  avail=$(((deadline_ms - $(now_ms)) / 1000 - 1))
  [ "$avail" -ge 10 ] || die "--window-end ${window_end}：静置之后只剩 ${avail} 秒可采（不足 10 秒），减小 --settle 或加大 --window-end"
  if [ "$avail" -lt "$idle" ]; then
    echo "--window-end ${window_end}：采样 ${avail} 秒（exercise 结束后 ${window_end} 秒停止）"
    idle=$avail
  fi
  echo "meta note 采样终点钉在 exercise 结束后 ${window_end} 秒（--window-end）" >> "$raw"
fi
pm_out="$tmp/powermetrics.plist"
case "$wakeups_mode" in
  top)
    echo "meta wakeups_method top-idlew" >> "$raw"
    echo "meta wakeups_start $(value_of "$pid" "$(top_idlew "$pid")")" >> "$raw"
    if [ -n "$wv_pids" ]; then echo "meta wv_wakeups_start $(top_idlew "$wv_pids")" >> "$raw"; fi
    ;;
  powermetrics)
    # 以 root 运行时直接调用；否则经 sudo -n（auto 模式已确认免密 sudo 可用）
    pm_cmd=(powermetrics --samplers tasks -f plist -i "$((idle * 1000))" -n 1)
    [ "$(id -u)" = 0 ] || pm_cmd=(sudo -n "${pm_cmd[@]}")
    "${pm_cmd[@]}" > "$pm_out" 2> "$tmp/powermetrics.err" &
    pm_pid=$!
    ;;
  none) echo "meta note 未测量唤醒次数（--wakeups none）" >> "$raw" ;;
esac

samples=$((idle / interval + 1))
echo "采样 ${idle} 秒（每 ${interval} 秒一次，共 ${samples} 个样本）……"
start_ms="$(now_ms)"
for ((i = 0; i < samples; i++)); do
  if [ "$i" -gt 0 ] && [ $((i % 10)) -eq 0 ]; then scan_webkit "$pid"; fi
  t_ms="$(now_ms)"
  if [ -n "$deadline_ms" ] && [ "$i" -gt 0 ] && [ "$t_ms" -gt "$deadline_ms" ]; then break; fi
  ps_out="$(ps -o pid=,rss=,time= -p "$pid${wv_pids:+,$wv_pids}" 2> /dev/null || true)"
  # phys_footprint（"pid:KiB,…"）：有就用它代替 ps 的 RSS
  fp=-
  if [ "$has_python" = 1 ]; then
    fp="$(/usr/bin/python3 -I "$here/macos-webkit.py" --footprint "$pid${wv_pids:+,$wv_pids}" 2> /dev/null || echo -)"
  fi
  # 主进程 → "<内存 KiB> <cpu ms>"；WebKit 进程 → "pid:内存:cpu,…"（没有时为 -）。ps 的 TIME 为 [[dd-]hh:]mm:ss.ss
  line="$(awk -v main="$pid" -v fp="$fp" '
    BEGIN { n = split(fp, a, ","); for (k = 1; k <= n; k++) { split(a[k], kv, ":"); if (kv[2] != "") F[kv[1]] = kv[2] } }
    function mem(p, rss) { return (p in F) ? F[p] : rss }
    function ms(t,   d, a, n, p, s, k) {
      d = 0
      if (index(t, "-")) { split(t, a, "-"); d = a[1]; t = a[2] }
      n = split(t, p, ":"); s = 0
      for (k = 1; k <= n; k++) s = s * 60 + p[k]
      return int((d * 86400 + s) * 1000 + 0.5)
    }
    $1 == main { m = mem($1, $2) " " ms($3); next }
    $1 ~ /^[0-9]+$/ { w = w (w == "" ? "" : ",") $1 ":" mem($1, $2) ":" ms($3) }
    END { if (m == "") exit 1; print m, (w == "" ? "-" : w) }' <<< "$ps_out")" || die "进程 $pid 在采样期间退出了"
  echo "sample $t_ms $line" >> "$raw"
  # 睡到下一个样本的绝对时刻（落后时不睡，直接采下一个）
  if [ $((i + 1)) -lt "$samples" ]; then
    wait_ms=$((start_ms + (i + 1) * interval * 1000 - t_ms))
    if [ "$wait_ms" -gt 0 ]; then sleep "$(printf '%d.%03d' $((wait_ms / 1000)) $((wait_ms % 1000)))"; fi
  fi
done

case "$wakeups_mode" in
  top)
    echo "meta wakeups_end $(value_of "$pid" "$(top_idlew "$pid")")" >> "$raw"
    if [ -n "$wv_pids" ]; then echo "meta wv_wakeups_end $(top_idlew "$wv_pids")" >> "$raw"; fi
    ;;
  powermetrics)
    if wait "$pm_pid" && [ -s "$pm_out" ]; then
      # 输出两行：主进程的每秒唤醒、WebKit 进程合计的每秒唤醒（取不到时为 -）
      rates="$(plutil -convert json -o - "$pm_out" 2> /dev/null | node -e '
        let s = ""
        process.stdin.on("data", (d) => (s += d)).on("end", () => {
          const doc = JSON.parse(s)
          const secs = (doc.elapsed_ns ?? 0) / 1e9 || 1
          const tasks = new Map((doc.tasks ?? []).map((t) => [t.pid, t]))
          // idle_wakeups 是 intr_wakeups 的子集（CPU 从空闲被唤醒的那部分），只取它，与 top 的 IDLEW 同口径
          const rate = (t) => t.idle_wakeups_per_s ?? (t.idle_wakeups ?? 0) / secs
          const main = tasks.get(Number(process.argv[1]))
          const wv = (process.argv[2] || "").split(",").filter(Boolean).map((p) => tasks.get(Number(p))).filter(Boolean)
          console.log(main ? String(rate(main)) : "-")
          console.log(wv.length ? String(wv.reduce((a, t) => a + rate(t), 0)) : "-")
        })' "$pid" "$wv_all" || printf -- '-\n-\n')"
      main_rate="$(sed -n 1p <<< "$rates")"
      wv_rate="$(sed -n 2p <<< "$rates")"
      {
        echo "meta wakeups_method powermetrics-idlew"
        echo "meta wakeups_per_s ${main_rate:--}"
        echo "meta wv_wakeups_per_s ${wv_rate:--}"
        if [ "${main_rate:--}" = - ]; then echo "meta note powermetrics 的输出中没有 pid $pid"; fi
      } >> "$raw"
    else
      echo "meta note powermetrics 失败：$(head -c 200 "$tmp/powermetrics.err" | tr '\n' ' ')" >> "$raw"
    fi
    pm_pid=""
    ;;
esac
if [ -n "$wv_mode" ] && [ -z "$wv_all" ]; then
  echo "meta note 采样期间没有找到属于 pid $pid 的 WebKit 进程（WebView 尚未创建，或归属失败）：WebView 指标记为 null" >> "$raw"
fi
if [ -n "$wv_names" ]; then
  echo "meta note 采样期间出现过的 WebKit 进程：$wv_names" >> "$raw"
  echo "  WebKit 进程：$wv_names" >&2
fi

args=(--raw "$raw")
[ -n "$out" ] && args+=(--out "$out")
[ -n "$latency_log" ] && args+=(--latency-log "$latency_log")
[ -n "$app_log" ] && args+=(--app-log "$app_log")
if [ -n "$log_out" ]; then
  if [ -n "$app_log" ] && [ -f "$app_log" ]; then
    mkdir -p "$(dirname "$log_out")"
    cp "$app_log" "$log_out"
  else
    echo "! --log-out：没有应用日志可复制（${app_log:-未知路径}）" >&2
  fi
fi
for s in "${smokes[@]+"${smokes[@]}"}"; do args+=(--smoke "$s"); done
node "$root/scripts/perf/record.mjs" "${args[@]}"

if [ "$dry_run" = 1 ]; then
  # shellcheck disable=SC2016 # 单引号里是 JS 源码（模板字符串的 ${…} 由 node 展开）
  node -e '
    const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))
    const i = r.idle
    const bad = []
    if (i.samples < 3) bad.push("样本不足")
    if (!(i.rss_main > 0)) bad.push("主进程 RSS")
    if (i.wakeups === null) bad.push("主进程唤醒")
    if (!i.session || i.session.window_s.length !== 2) bad.push("会话信息（--exercise 的时刻与采样窗口）")
    if (process.argv[2] === "1") {
      if (i.webview_attribution !== "responsible") bad.push(`WebView 归属应为 responsible，实际为 ${i.webview_attribution}`)
      if (!(i.webview_processes >= 1)) bad.push("没有找到探针的 WebKit 进程")
      if (!(i.rss_webview > 0)) bad.push("WebView RSS")
      if (!(i.cpu_webview_avg > 0)) bad.push("WebView CPU（探针页面每 200 ms 忙算 40 ms，应 > 0）")
      if (i.wakeups_webview === null) bad.push("WebView 唤醒")
      if (i.cpu_total_avg === null) bad.push("主进程 + WebView 的合计 CPU")
    }
    if (bad.length) {
      console.error("dry-run 输出不完整：" + bad.join("；"), i)
      process.exit(1)
    }' "$out" "$probe"
  if [ "$probe" = 1 ]; then
    # 第二步：从终端直接启动探针（负责进程是终端），WebKit 进程仍要能按「负责进程 + 启动时间」找到
    stop_process "$launched"
    launched=""
    "$app/Contents/MacOS/wisp-webview-probe" > /dev/null 2>&1 &
    probe_direct=$!
    disown "$probe_direct"
    for ((i = 0; i < 50; i++)); do
      sleep 0.2
      scan_webkit "$probe_direct"
      if [ -n "$wv_pids" ]; then break; fi
    done
    [ -n "$wv_pids" ] || die "dry-run：从终端启动的探针找不到它的 WebKit 进程（近似归属失败）"
    echo "✓ 从终端启动时按负责进程找到探针的 WebKit 进程 ${wv_pids}（${wv_mode}）"
  fi
  echo "✓ measure-macos.sh dry-run 通过（采样、WebView 归属与 RSS / CPU / 唤醒、汇总与 schema 校验）"
fi
