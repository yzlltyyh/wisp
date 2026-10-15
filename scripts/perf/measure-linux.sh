#!/usr/bin/env bash
# scripts/perf/measure-linux.sh
#
# Linux（X11）空闲测量：主进程与 WebKitGTK 进程各自的 RSS、累计 CPU、唤醒次数，按秒采样，汇总为
# wisp.perf/1 性能记录（格式见 scripts/perf/README.md）。统计与 schema 校验由 record.mjs 完成。
#
# 用法：
#   scripts/perf/measure-linux.sh [选项]
#     --pid <pid>            测量已经在运行的进程（默认：找名为 wisp-desktop 的进程）
#     --app <可执行文件>     先启动再测量（测量结束后退出它）
#     --app-args <参数>      （与 --app 一起用）传给 Wisp 的参数，默认 "--autostart"（静默启动、不弹引导 / 设置窗口）
#     --home <目录|keep>     （与 --app 一起用）以隔离的 HOME / XDG 目录（CONFIG / DATA / CACHE / STATE）启动：不读写真实的
#                            配置、剪贴板历史与日志。默认在临时目录里新建一个，结束时删除；keep 保留真实 HOME。隔离时保留
#                            X11 的授权（XAUTHORITY 未设置时指向真实的 ~/.Xauthority）；GTK / fontconfig / Mesa 的用户缓存
#                            是冷的，所以隔离时 --warmup 默认 60 秒（让首次构建缓存的 CPU 与内存在采样之前结束）
#     --settings <preset|none|文件>  （隔离 HOME 时）启动前写入的 <XDG_CONFIG_HOME>/wisp/settings.json。默认 preset：
#                            exercise.mjs --write-settings 的测量设置（关闭更新检查、不写自启项、日志级别 debug——exercise.mjs
#                            与 record.mjs 靠面板事件核对 exercise 的最后状态——、空闲卸载取本平台默认值）；none：不写；
#                            文件：原样复制。--home keep 时一律不写
#     --idle <秒>            空闲采样时长，默认 600
#     --warmup <秒>          启动后等待多久再开始采样（--app 时默认 30、隔离 HOME 时 60，其余默认 0）
#     --exercise <命令>      预热之后、采样之前执行一次（bash -e -c：任何一条命令失败都算 exercise 失败；与应用相同的环境，
#                            WISP_PID 为被测进程、WISP_BIN 为它的可执行文件、WISP_LOG 为它的应用日志），用来先用一次 Wisp，
#                            测「用过之后」能否回到零唤醒、WebView 内存回到多少（Windows / Linux 面板回收
#                            的取舍依据）。推荐 "node scripts/perf/exercise.mjs"（灌剪贴板需要 xclip；依次唤出 / 关闭输入与剪贴板
#                            面板，由 debug 日志核对结束时面板确实已隐藏）
#     --settle <秒>          --exercise 之后等待多久再采样，默认 30（采样的实际窗口以记录里的 idle.session.window_s 为准）
#     --app-log <文件>       被测应用的日志（默认：--app 启动时为 <XDG_STATE_HOME>/wisp/logs/wisp.log）。交给 record.mjs
#                            得出 idle.session（exercise 是否唤出了面板、采样窗口是回收前还是回收后）与本次启动的 latency 段
#     --log-out <文件>       测量结束后把应用日志复制到这里（隔离 HOME 会在结束时删除）
#     --interval <秒>        采样间隔，默认 1
#     --build <release|debug> 构建类型（默认按路径推断）
#     --latency-log <文件>   同时从开发者模式日志统计延迟
#     --smoke <文件>         合并 smoke-report.json（可重复：release、debug 各一份）
#     --out <文件>           输出路径（默认 target/perf/<date>-linux.json）
#     --dry-run              不需要 Wisp：启动一个临时进程，它带一个名为 WebKitWebProcess、周期性占用少量 CPU 的
#                            子进程，采样 3 秒，走完整流程并校验主进程与 WebView 的 RSS / CPU / 唤醒（CI 用）
#
# 数据来源：RSS 取 /proc/<pid>/status 的 VmRSS；CPU 取 /proc/<pid>/stat 的 utime + stime（累计，取相邻样本之差）；
# WebView 为主进程的后代中名字以 WebKit 开头的进程（WebKitWebProcess、WebKitNetworkProcess），
# 一个都找不到时 WebView 指标记为 null；唤醒次数优先用各线程 /proc/<pid>/task/*/sched 的 nr_wakeups
# （需要 kernel.sched_schedstats=1），否则用各线程 voluntary_ctxt_switches 之和（线程每次主动睡眠都对应一次随后的
# 唤醒，是同一量的近似）。主进程与 WebView 进程分别统计。
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

pid=""
app=""
app_args="--autostart"
home=""
settings_opt=preset
exercise=""
settle=30
idle=600
warmup=""
interval=1
build=""
latency_log=""
smokes=()
out=""
dry_run=0
app_log=""
log_out=""
launch_ms=""

die() {
  echo "measure-linux: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --pid) pid="${2:?}"; shift 2 ;;
    --app) app="${2:?}"; shift 2 ;;
    --app-args) app_args="${2-}"; shift 2 ;;
    --home) home="${2:?}"; shift 2 ;;
    --settings) settings_opt="${2:?}"; shift 2 ;;
    --exercise) exercise="${2:?}"; shift 2 ;;
    --settle) settle="${2:?}"; shift 2 ;;
    --idle) idle="${2:?}"; shift 2 ;;
    --warmup) warmup="${2:?}"; shift 2 ;;
    --interval) interval="${2:?}"; shift 2 ;;
    --build) build="${2:?}"; shift 2 ;;
    --latency-log) latency_log="${2:?}"; shift 2 ;;
    --app-log) app_log="${2:?}"; shift 2 ;;
    --log-out) log_out="${2:?}"; shift 2 ;;
    --smoke) smokes+=("${2:?}"); shift 2 ;;
    --out) out="${2:?}"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help)
      awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
      exit 0
      ;;
    *) die "未知参数 $1（--help 查看用法）" ;;
  esac
done

[ "$(uname -s)" = Linux ] || die "只能在 Linux 上运行"
case "$idle" in '' | *[!0-9]*) die "--idle 必须是正整数秒" ;; esac
case "$interval" in '' | *[!0-9]*) die "--interval 必须是正整数秒" ;; esac
case "$settle" in '' | *[!0-9]*) die "--settle 必须是非负整数秒" ;; esac
[ "$idle" -ge 1 ] && [ "$interval" -ge 1 ] || die "--idle / --interval 必须 ≥ 1"
for tool in ps awk node getconf; do command -v "$tool" > /dev/null || die "缺少 $tool"; done

tmp="$(mktemp -d "${TMPDIR:-/tmp}/wisp-perf.XXXXXX")"
launched=""
app_env=() # --app 启动时给应用（与 --exercise）的环境：隔离的 HOME / XDG 目录
cleanup() {
  if [ -n "$launched" ]; then
    # dry-run 的临时进程带子进程：先结束子进程
    if [ "$dry_run" = 1 ]; then pkill -P "$launched" 2> /dev/null || true; fi
    kill "$launched" 2> /dev/null || true
    wait "$launched" 2> /dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

if [ "$dry_run" = 1 ]; then
  # 临时「主进程」：一个 bash，带一个名为 WebKitWebProcess 的子进程（bash 的副本），子进程每 200 ms 做一小段计算，
  # 用来验证 WebView 的后代进程识别与 RSS / CPU / 唤醒采样
  cp "$(command -v bash)" "$tmp/WebKitWebProcess"
  # 单引号里的 $ 由子 bash 展开
  # shellcheck disable=SC2016
  bash -c '"$1" -c "for ((j = 0; j < 600; j++)); do for ((k = 0; k < 20000; k++)); do :; done; sleep 0.2; done" & wait' \
    _ "$tmp/WebKitWebProcess" > /dev/null 2>&1 &
  launched=$!
  pid=$launched
  idle=3
  warmup=1
  interval=1
  [ -z "$out" ] && out="$tmp/dry-run.json"
  [ -z "$build" ] && build=release
  # 顺带走一遍 --exercise 的流程（WISP_PID / WISP_BIN 已设置、命令成功、进程仍在）
  if [ -z "$exercise" ]; then
    exercise='kill -0 "$WISP_PID" && [ -x "$WISP_BIN" ]'
    settle=1
  fi
  echo "dry-run：对临时进程 ${pid}（带 WebKitWebProcess 子进程）采样 ${idle} 秒"
elif [ -n "$app" ]; then
  [ -x "$app" ] || die "$app 不可执行"
  if [ "$home" != keep ]; then
    if [ -z "$home" ]; then home="$tmp/home"; fi
    mkdir -p "$home"
    # X11 授权：cookie 在真实的 ~/.Xauthority 里、XAUTHORITY 又没设置时（startx、部分显示管理器），换了 HOME 就连不上显示器
    if [ -z "${XAUTHORITY:-}" ] && [ -f "$HOME/.Xauthority" ]; then app_env+=(XAUTHORITY="$HOME/.Xauthority"); fi
    # XDG 目录一并指过去（设置了 XDG_* 的桌面上只改 HOME 不够）；XDG_RUNTIME_DIR 不动（D-Bus / 单实例要用）
    app_env+=(HOME="$home" XDG_CONFIG_HOME="$home/.config" XDG_DATA_HOME="$home/.local/share"
      XDG_CACHE_HOME="$home/.cache" XDG_STATE_HOME="$home/.local/state")
    echo "以隔离的 HOME 启动：$home（--home keep 改用真实 HOME）；用户缓存是冷的，预热默认 60 秒"
    [ -z "$warmup" ] && warmup=60
    # 启动前写入设置（只在隔离 HOME 里）：app::paths 的 config_dir 是 $XDG_CONFIG_HOME/wisp
    settings_file="$home/.config/wisp/settings.json"
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
  launch_ms="$(date +%s%3N)"
  # shellcheck disable=SC2086 # app_args 按空格拆成多个参数
  env "${app_env[@]+"${app_env[@]}"}" "$app" $app_args > "$tmp/app.log" 2>&1 &
  launched=$!
  pid=$launched
  [ -z "$warmup" ] && warmup=30
  # 应用日志（app::paths::log_dir：$XDG_STATE_HOME/wisp/logs，未设置时 ~/.local/state/wisp/logs）
  if [ -z "$app_log" ]; then
    if [ "$home" != keep ]; then
      app_log="$home/.local/state/wisp/logs/wisp.log"
    else
      app_log="${XDG_STATE_HOME:-$HOME/.local/state}/wisp/logs/wisp.log"
    fi
  fi
  echo "已启动 ${app}（pid ${pid}），等待 ${warmup} 秒预热"
elif [ -z "$pid" ]; then
  pid="$(pgrep -nx wisp-desktop || true)"
  [ -n "$pid" ] || die "没有找到运行中的 wisp-desktop；用 --pid 或 --app 指定"
fi
[ -z "$warmup" ] && warmup=0
[ -d "/proc/$pid" ] || die "进程 $pid 不存在"

if [ -z "$build" ]; then
  case "$(readlink "/proc/$pid/exe" 2> /dev/null || true)" in
    */debug/*) build=debug ;;
    *) build=release ;;
  esac
fi

sleep "$warmup"
[ -d "/proc/$pid" ] || die "进程 $pid 在预热期间退出了"

# 会话之后的空闲（与 measure-macos.sh 同一做法）：先执行一次 --exercise，再等 --settle 秒
exercise_start_ms=""
exercise_end_ms=""
if [ -n "$exercise" ]; then
  echo "执行 --exercise：$exercise"
  exercise_start_ms="$(date +%s%3N)"
  # 与应用同一套环境（隔离的 HOME / XDG）：--trigger 的第二实例与被测实例看到的配置一致；-e：任何一条命令失败都算失败
  env "${app_env[@]+"${app_env[@]}"}" WISP_PID="$pid" WISP_BIN="$(readlink "/proc/$pid/exe" 2> /dev/null || true)" \
    WISP_LOG="$app_log" bash -e -c "$exercise" || die "--exercise 失败（退出码 $?）"
  exercise_end_ms="$(date +%s%3N)"
  echo "等待 ${settle} 秒（会话后的收尾）"
  sleep "$settle"
  [ -d "/proc/$pid" ] || die "进程 $pid 在 --exercise 之后退出了"
fi

hz="$(getconf CLK_TCK)"

# 累计 CPU（毫秒）：/proc/<pid>/stat 第 2 个字段（comm）可能含空格，从最后一个 ') ' 之后数字段；进程不存在时失败
cpu_ms() {
  local stat
  stat="$(cat "/proc/$1/stat" 2> /dev/null)" || return 1
  stat="${stat##*) }"
  # 去掉 comm 后：第 12、13 个字段是 utime、stime
  awk -v hz="$hz" '{printf "%d\n", ($12 + $13) * 1000 / hz}' <<< "$stat"
}

# VmRSS（KiB）；进程不存在时失败
rss_kib() {
  awk '/^VmRSS:/ {print $2; found = 1} END {if (!found) exit 1}' "/proc/$1/status" 2> /dev/null
}

# 后代进程中名字以 WebKit 开头的那些（空格分隔）
webview_pids() {
  ps -e -o pid=,ppid=,comm= | awk -v root="$1" '
    { parent[$1] = $2; name[$1] = $3 }
    END {
      for (p in parent) {
        q = p; depth = 0
        while (q != "" && q != root && q != 1 && q != 0 && depth < 32) { q = parent[q]; depth++ }
        if (q == root && p != root && name[p] ~ /^WebKit/) printf "%s ", p
      }
    }'
}

schedstats=0
if [ "$(cat /proc/sys/kernel/sched_schedstats 2> /dev/null || echo 0)" = 1 ]; then schedstats=1; fi

# 进程全部线程的唤醒计数之和；进程不存在时输出空串
wakeups_total() {
  [ -d "/proc/$1" ] || return 0
  if [ "$schedstats" = 1 ]; then
    cat /proc/"$1"/task/*/sched 2> /dev/null | awk '$1 == "nr_wakeups" || $1 == "se.statistics.nr_wakeups" {s += $NF} END {printf "%d\n", s}'
  else
    cat /proc/"$1"/task/*/status 2> /dev/null | awk '/^voluntary_ctxt_switches:/ {s += $2} END {printf "%d\n", s}'
  fi
}

# 一组进程的唤醒计数："pid:n,…"（没有时为 -）
wakeups_list() {
  local w n list=""
  for w in "$@"; do
    n="$(wakeups_total "$w")"
    if [ -n "$n" ]; then list="${list:+$list,}$w:$n"; fi
  done
  echo "${list:--}"
}

wv="$(webview_pids "$pid")"
raw="$tmp/samples.txt"
{
  echo "# measure-linux.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "meta os linux"
  # shellcheck disable=SC1091
  echo "meta os_version $(. /etc/os-release 2> /dev/null && echo "${PRETTY_NAME:-unknown}") / $(uname -r)"
  echo "meta arch $(uname -m)"
  echo "meta machine $(cat /sys/devices/virtual/dmi/id/product_name 2> /dev/null || echo unknown)"
  echo "meta build $build"
  echo "meta webview_attribution descendants"
  if [ -n "$launch_ms" ]; then echo "meta launch_ms $launch_ms"; fi
  if [ -n "$exercise" ]; then
    echo "meta exercise_start_ms $exercise_start_ms"
    echo "meta exercise_end_ms $exercise_end_ms"
    echo "meta settle_s $settle"
    # 使用画像写进记录：exercise 的命令原样记下（换行换成空格）
    echo "meta note 会话后空闲：采样前执行了 --exercise（${exercise//$'\n'/ }）并静置 ${settle} 秒"
  fi
  if [ "$schedstats" = 1 ]; then
    echo "meta wakeups_method nr_wakeups"
  else
    echo "meta wakeups_method voluntary_ctxt_switches"
    echo "meta note kernel.sched_schedstats 未开启：唤醒次数用 voluntary_ctxt_switches 近似（sudo sysctl kernel.sched_schedstats=1 可得到 nr_wakeups）"
  fi
  echo "meta wakeups_start $(wakeups_total "$pid")"
  # shellcheck disable=SC2086
  if [ -n "$wv" ]; then echo "meta wv_wakeups_start $(wakeups_list $wv)"; fi
} > "$raw"

seen_wv=0
samples=$((idle / interval + 1))
echo "采样 ${idle} 秒（每 ${interval} 秒一次，共 ${samples} 个样本）……"
for ((i = 0; i < samples; i++)); do
  if [ "$i" -gt 0 ] && [ $((i % 10)) -eq 0 ]; then wv="$(webview_pids "$pid")"; fi
  cpu="$(cpu_ms "$pid")" || die "进程 $pid 在采样期间退出了"
  rss="$(rss_kib "$pid")" || die "进程 $pid 在采样期间退出了"
  now_ms="$(date +%s%3N)"
  list=""
  for w in $wv; do
    # 进程可能刚好退出：取不到就跳过这一个
    if r="$(rss_kib "$w")" && c="$(cpu_ms "$w")"; then list="${list:+$list,}$w:$r:$c"; fi
  done
  if [ -n "$list" ]; then seen_wv=1; fi
  echo "sample $now_ms $rss $cpu ${list:--}" >> "$raw"
  if [ $((i + 1)) -lt "$samples" ]; then sleep "$interval"; fi
done
echo "meta wakeups_end $(wakeups_total "$pid")" >> "$raw"
# shellcheck disable=SC2086
if [ -n "$wv" ]; then echo "meta wv_wakeups_end $(wakeups_list $wv)" >> "$raw"; fi
if [ "$seen_wv" = 0 ]; then
  echo "meta note 采样期间没有找到 pid $pid 的 WebKit 后代进程（WebView 尚未创建？）：WebView 指标记为 null" >> "$raw"
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
  node -e '
    const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))
    const i = r.idle
    const bad = []
    if (i.samples < 3) bad.push("样本不足")
    if (!(i.rss_main > 0)) bad.push("主进程 RSS")
    if (i.wakeups === null) bad.push("主进程唤醒")
    if (!i.session || i.session.window_s.length !== 2) bad.push("会话信息（--exercise 的时刻与采样窗口）")
    if (i.webview_attribution !== "descendants" || !(i.webview_processes >= 1)) bad.push("没有识别出 WebKitWebProcess 子进程")
    if (!(i.rss_webview > 0)) bad.push("WebView RSS")
    if (!(i.cpu_webview_avg > 0)) bad.push("WebView CPU（子进程每 200 ms 做一段计算，应 > 0）")
    if (i.wakeups_webview === null || i.cpu_total_avg === null) bad.push("WebView 唤醒 / 合计 CPU")
    if (bad.length) {
      console.error("dry-run 输出不完整：" + bad.join("；"), i)
      process.exit(1)
    }' "$out"
  echo "✓ measure-linux.sh dry-run 通过（/proc 采样、WebKit 后代进程的 RSS / CPU / 唤醒、汇总与 schema 校验）"
fi
