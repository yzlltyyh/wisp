#!/usr/bin/env bash
# scripts/ci/panel-recycle-linux.sh：Linux 上「面板回收要不要打开」的对照数据（ci.yml 的 panel-recycle 作业）。
#
# 在 dbus-run-session + xvfb-run 里运行（会话总线给单实例转发 --trigger 用；Xvfb 给 X11）：
#   dbus-run-session -- xvfb-run -a -s '-screen 0 1920x1080x24' bash scripts/ci/panel-recycle-linux.sh
# 先在会话总线上起一个最小的 StatusNotifierWatcher（scripts/ci/sni-watcher.py），否则 Wisp 会走「没有托盘 → 打开设置窗口」
# 的退化路径，设置窗口的 WebView 会混进测量。然后：
#   0. 热身：启动一次 target/release/wisp-desktop、20 秒后退出（不采样）——让二进制、共享库与 WebKitGTK 的磁盘缓存先热起来，
#      三段测量的起点一致（否则只有第一段是冷启动）；
#   依次测三段空闲（fake-llm 构建：WISP_PANEL_RECYCLE_SECS 可以覆盖回收时长），每段都是新进程、隔离的 HOME / XDG 目录、
#   measure-linux.sh 的测量设置（--settings preset：更新检查关闭、日志级别 debug）、显式预热 60 秒：
#   1. never     从未显示（预热之后直接采样）
#   2. used      用过之后（exercise.mjs：灌剪贴板、输入面板与剪贴板面板各唤出再关闭；当前默认：Linux 不回收）
#   3. recycled  用过并回收（WISP_PANEL_RECYCLE_SECS=45，与 macOS 的默认值相同；静置 60 s 后采样）
# 每段都过 smoke-gate --require-webview（WebView 必须测到），后两段再核对 exercise 用到了两个视图、采样窗口里面板已隐藏、
# 窗口的状态（--expect-views / --expect-state）；日志里出现「没有托盘宿主」的退化时那一段作废。**一段无效不中断**：
# 三段都测完、各自的判定写进摘要之后，有任何一段无效才以非零退出（每个平台只跑一次，不能因为第一段就丢掉后两段）。
# 记录与日志写到 target/panel-recycle/。判定方法见 scripts/perf/README.md「面板回收对照」。
set -uo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
bin="$root/target/release/wisp-desktop"
out="$root/target/panel-recycle"
idle="${PANEL_RECYCLE_IDLE:-120}"
warmup="${PANEL_RECYCLE_WARMUP:-60}"

die() {
  echo "::error::panel-recycle-linux: $*" >&2
  exit 1
}

[ -x "$bin" ] || die "$bin 不存在：先 pnpm tauri build --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json"
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || die "没有 D-Bus 会话总线：在 dbus-run-session 里运行"
[ -n "${DISPLAY:-}" ] || die "没有 DISPLAY：在 xvfb-run 里运行"
has_xclip=有
command -v xclip > /dev/null || {
  has_xclip="没有 xclip，未灌入"
  echo "::warning::panel-recycle-linux：没有 xclip，exercise 不灌剪贴板（--seed-optional 只告警）"
}
mkdir -p "$out"

python3 "$root/scripts/ci/sni-watcher.py" > "$out/sni-watcher.log" 2>&1 &
watcher=$!
trap 'kill "$watcher" 2> /dev/null || true' EXIT
for _ in $(seq 100); do
  if grep -q 'sni-watcher: ready' "$out/sni-watcher.log"; then break; fi
  kill -0 "$watcher" 2> /dev/null || die "sni-watcher 退出了：$(cat "$out/sni-watcher.log")"
  sleep 0.1
done
grep -q 'sni-watcher: ready' "$out/sni-watcher.log" || die "sni-watcher 10 秒内没有取得 org.kde.StatusNotifierWatcher"

# 0. 热身（不采样）：隔离的临时 HOME，20 秒后退出
echo "::group::panel-recycle 热身（启动一次、20 秒后退出，不采样）"
warm_home="$(mktemp -d)"
HOME="$warm_home" XDG_CONFIG_HOME="$warm_home/.config" XDG_DATA_HOME="$warm_home/.local/share" \
  XDG_CACHE_HOME="$warm_home/.cache" XDG_STATE_HOME="$warm_home/.local/state" \
  "$bin" --autostart > "$out/warmup.out.log" 2>&1 &
warm_pid=$!
sleep 20
kill "$warm_pid" 2> /dev/null || true
wait "$warm_pid" 2> /dev/null || true
rm -rf "$warm_home"
echo "::endgroup::"

exercise='node scripts/perf/exercise.mjs --views input,clipboard --texts 30 --images 3 --seed-optional'
results=()
failed=0

# segment <名字> <smoke-gate 的额外参数（空格分隔）> [measure-linux.sh 的参数…]
segment() {
  local name="$1" gate="$2" verdict=有效
  shift 2
  echo "::group::panel-recycle ${name}"
  if ! (cd "$root" && bash scripts/perf/measure-linux.sh --app "$bin" --warmup "$warmup" --idle "$idle" \
    --out "$out/$name.json" --log-out "$out/$name.log" "$@"); then
    verdict="测量失败（measure-linux.sh 非零退出）"
  elif grep -q 'no StatusNotifierWatcher' "$out/$name.log" 2> /dev/null; then
    verdict="无效：Wisp 没有找到托盘宿主、打开了设置窗口（tray::verify_host），设置窗口的 WebView 混进了测量"
  else
    # shellcheck disable=SC2086 # gate 按空格拆成多个参数
    node "$root/scripts/perf/smoke-gate.mjs" "$out/$name.json" --ci --require-webview $gate ||
      verdict="无效：smoke-gate 判定测量无效"
  fi
  echo "::endgroup::"
  results+=("| ${name} | ${verdict} |")
  if [ "$verdict" != 有效 ]; then
    echo "::error::panel-recycle-linux：${name}：${verdict}"
    failed=1
  fi
}

segment never ''
segment used '--expect-state hot --expect-views input,clipboard' --exercise "$exercise" --settle 30
export WISP_PANEL_RECYCLE_SECS=45
segment recycled '--expect-state recycled --expect-views input,clipboard' --exercise "$exercise" --settle 60
unset WISP_PANEL_RECYCLE_SECS

summary="$(
  printf '### 面板回收对照：linux 各段的判定\n\n'
  printf '测量顺序：热身启动（20 s，不采样）→ never → used → recycled；每段新进程、隔离 HOME、预热 %s s、采样 %s s；' "$warmup" "$idle"
  printf '更新检查关闭（measure-linux.sh --settings preset：general.update_policy = off）；剪贴板由 xclip 灌入（%s）。\n\n' "$has_xclip"
  printf '| 段 | 判定 |\n| --- | --- |\n'
  printf '%s\n' "${results[@]}"
)"
echo "$summary"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then printf '%s\n\n' "$summary" >> "$GITHUB_STEP_SUMMARY"; fi
exit "$failed"
