#!/usr/bin/env bash
# scripts/release/updater-rehearsal.sh
#
# 本机更新演练（macOS arm64）：真实地把 Wisp 从 <from>（默认 0.1.0）更新到 <to>（默认 0.1.1）——release 构建、更新器
# 签名、本机 HTTP 服务器提供 latest.json 与 .app.tar.gz(.sig)、应用自己检查 / 下载 / 验签 / 安装，重新启动后核对版本、
# 设置与数据、钥匙串条目。结果写进 <work>/results（各场景的检查项、日志与快照）。
#
# 签名：一律用**一次性测试密钥**（`tauri signer generate` 生成在 <work>/builds/signing，口令是随机值，只经 -p 与打包那一步
# 的环境传递、不落盘，它不保护任何正式资产），叠加配置把演练构建的 pubkey 换成这把测试公钥——本机不产生任何经正式私钥
# 签名的包（正式私钥与口令只在 CI 里使用）。正式密钥对是否配套由 CI 的 finalize 用正式公钥校验每个 .sig，本机演练不需要
# 正式私钥。
#
# 隔离：演练构建用单独的 identifier（io.github.yzlltyyh.wisp.rehearsal，不经单实例锁转发给正在运行的 Wisp）；应用以
# 隔离的 HOME 启动（open --env HOME=…）——配置、剪贴板历史、日志都在工作目录里，钥匙串是隔离 HOME 里一次性的
# keychain 文件（HOME 改了以后 Security 框架的默认钥匙串与搜索列表都解析到这里，用户的登录钥匙串不在列表里）。
# HOME 管不到的系统级资源由演练设置关掉：剪贴板监听（不把用户复制的内容记进工作目录）、五个全局快捷键（全部不绑定，
# 不和正在运行的 Wisp 抢组合键，也不会因为按了 ⌥S 以演练 identifier 触发屏幕录制的 TCC 请求）、扩展触发。
# 编译用单独的 CARGO_TARGET_DIR（<work>/cargo-target）：仓库的 target/release 不会混进演练构建。
#
# 演练构建与正式构建的差别只有 `tauri build --config` 叠加的一份配置（updater-rehearsal.mjs overlay）：端点换成
# http://127.0.0.1:<端口>/latest.json 并打开 dangerousInsecureTransportProtocol（release 构建只认 https，这只能在编译时
# 决定）、单独的 identifier、`plugins.wispUpdaterRehearsal.enabled`（打开 Rust 侧 updater::rehearsal 的日志与钥匙串探针），
# 以及 pubkey（只能是本次生成的测试公钥、key id 与正式公钥不同）。运行中的
# 应用把内嵌的公钥记进日志，脚本逐次核对它就是预期的那一把。
#
# 场景（每个场景启动一次应用，启动 30 s 后的自动检查是真实的代码路径）：
#   A  notify    默认策略：检查到新版本只提示、不下载；<from> 经应用自己的钥匙串句柄写入探针条目
#   B  tampered  篡改过的包 + 真签名（auto_install）：下载后验签失败，.app 原封不动
#   C  forged    别的密钥签的包（auto_install）：验签失败，.app 原封不动
#   D  genuine   真签名的包（auto_install）：下载、验签、替换 .app；<from> 读探针（同一个二进制，不弹框）
#   E  updated   重新启动：版本为 <to>、对同一份 latest.json 报告「已是最新」（不会循环更新）、设置与数据与更新前一致、
#                钥匙串条目还在。<to> 读取探针要经用户授权（ad-hoc 签名的 cdhash 变了，系统弹框要「登录」钥匙串的密码），
#                所以只在 --keychain-prompt / --interactive 时读——无人值守时不在屏幕上留一个没人回答的系统对话框
#                （实测：请求方退出后对话框还会停留约 1.5 分钟）
#   E2 again     只在 --keychain-prompt / --interactive 且 E 读到了同一个值时：再启动一次 <to> 读同一个条目，「始终允许」
#                之后应当不再弹框（15 s 内读到）
#   F  replay    重新装回 <from>，latest.json 把 <to> 的真包标成更高的版本（latest.json 本身不签名）：插件按签名
#                trusted comment 里记录的 version 拒绝（requireSignedVersion 关闭时也核对——它只额外拒绝没有记录版本的签名），
#                .app 原封不动
# --interactive 时 D 换成肉眼走一遍 GUI：托盘「有新版本」→ 设置·关于 →「下载并安装」→「重启」，脚本等日志后接着做 E。
#
# 用法：
#   scripts/release/updater-rehearsal.sh [选项]
#     --from <版本> / --to <版本>   默认 0.1.0 / 0.1.1（<to> 必须更新）
#     --work <目录>                 工作目录，默认 target/updater-rehearsal（结果在 <work>/results，构建在 <work>/builds，
#                                   编译缓存在 <work>/cargo-target，可随时删除）
#     --skip-build                  复用 <work>/builds 里上一次（带 --keep）的构建（端点端口也沿用）
#     --interactive                 D 由用户在 GUI 里完成（见上）；钥匙串读取默认等 120 s，请在授权框里点「始终允许」
#     --keychain-prompt             E 里让 <to> 读钥匙串探针（会弹系统授权框：输入「登录」钥匙串的密码——即本脚本生成、打印出来的
#                                   一次性钥匙串口令，不是你的登录密码——再点「始终允许」）；--interactive 时默认打开
#     --keychain-wait <秒>          E 里等钥匙串读取结果多久（默认 120）
#     --keep                        结束时保留隔离 HOME（含一次性钥匙串）、<work>/builds 与 <work>/www，方便事后查看或
#                                   --skip-build 重跑（builds/signing 里有测试私钥，它不保护任何正式资产）。不加 --keep 时
#                                   结束（含失败与中断）就删掉它们
#     --allow-dirty                 版本号文件有未提交的修改时也继续（结束时仍恢复为开始时的版本号）
#     --self-test                   不构建、不启动应用：bash 语法、辅助脚本自检、前置条件
#
# 私钥：构建分两步——先不带任何秘密跑 `tauri build --no-bundle`（前端、全部 crate、build script 与 proc-macro 都在这一步
# 跑完），再只把私钥交给打包与签名那一步 `tauri bundle`。TAURI_SIGNING_PRIVATE_KEY 的值是一次性测试私钥文件的**绝对路径**
# （tauri-cli 2 接受路径或内容，这里只给路径；tauri 在 apps/desktop 下运行，相对路径在那里找不到文件时会被当成私钥内容。
# TAURI_SIGNING_PRIVATE_KEY_PATH 只对 `tauri signer sign` 生效，`tauri build` / `tauri bundle` 不认——2.12.1 实测会报「A public
# key has been found, but no private key」）；测试私钥的口令只在本次运行的内存里，经那一条命令的前缀传给 tauri。
# 不要用 bash -x 运行。
#
# 版本号：构建前用 scripts/bump.mjs 改成 <from> / <to>，结束（含失败与中断）时恢复为开始时的版本号。
# 退出码：0 = 全部必过项通过；1 = 有必过项失败；2 = 用法 / 环境错误。
set -euo pipefail
set +x # 私钥口令只出现在一条命令的环境里：任何情况下都不要回显命令

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
helper="$here/updater-rehearsal.mjs"

from=0.1.0
to=0.1.1
work="$root/target/updater-rehearsal"
skip_build=0
interactive=0
keychain_prompt=0
keychain_wait=120
keychain_read_ok=0
keep=0
allow_dirty=0
self_test=0
identifier=io.github.yzlltyyh.wisp.rehearsal

die() {
  echo "updater-rehearsal: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --from) from="${2:?}"; shift 2 ;;
    --to) to="${2:?}"; shift 2 ;;
    --work) work="${2:?}"; shift 2 ;;
    --skip-build) skip_build=1; shift ;;
    --interactive) interactive=1; keychain_prompt=1; shift ;;
    --keychain-prompt) keychain_prompt=1; shift ;;
    --keychain-wait) keychain_wait="${2:?}"; shift 2 ;;
    --keep) keep=1; shift ;;
    --official-key) die "不支持 --official-key：正式私钥只在 CI 里使用，本机演练一律用一次性测试密钥" ;;
    --allow-dirty) allow_dirty=1; shift ;;
    --self-test) self_test=1; shift ;;
    -h | --help)
      awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
      exit 0
      ;;
    *) die "未知参数 $1（--help 查看用法）" ;;
  esac
done

case "$keychain_wait" in '' | *[!0-9]*) die "--keychain-wait 必须是非负整数秒" ;; esac

version_files=(package.json apps/desktop/package.json Cargo.toml Cargo.lock apps/desktop/src-tauri/tauri.conf.json)

preflight() {
  [ "$(uname -s)" = Darwin ] || die "只能在 macOS 上运行"
  [ "$(uname -m)" = arm64 ] || die "只支持 Apple Silicon（macOS 只出 aarch64）"
  local tool
  for tool in node pnpm open osascript codesign ditto security xattr /usr/libexec/PlistBuddy; do
    command -v "$tool" > /dev/null || die "缺少 $tool"
  done
  command -v sqlite3 > /dev/null || echo "updater-rehearsal: 没有 sqlite3，数据库只比较文件大小" >&2
  node "$helper" --self-test > /dev/null || die "updater-rehearsal.mjs 自检失败"
  node -e '
    const re = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
    const [a, b] = [process.argv[1], process.argv[2]].map((v) => (re.exec(v) || die(v)).slice(1).map(Number));
    function die(v) { console.error(`版本号必须是 x.y.z：${v}`); process.exit(1) }
    const cmp = a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
    if (cmp >= 0) { console.error(`--to 必须比 --from 新：${process.argv[1]} → ${process.argv[2]}`); process.exit(1) }
  ' "$from" "$to" || die "版本号不对"
}

if [ "$self_test" = 1 ]; then
  bash -n "$0"
  preflight
  echo "✓ updater-rehearsal.sh 自检通过（语法、辅助脚本自检、前置条件）"
  exit 0
fi

preflight

mkdir -p "$work"
work="$(cd "$work" && pwd -P)" # tauri 在 apps/desktop 下运行：交给它的路径都要是绝对路径
builds="$work/builds"
cargo_target="$work/cargo-target"
results="$work/results"
home="$work/home"
www="$work/www"
app="$home/Applications/Wisp.app"
wisp_log="$home/Library/Logs/wisp/wisp.log"
keychain="$home/Library/Keychains/login.keychain-db"
checks="$results/checks.tsv"
server_log="$results/server.jsonl"

original_version="$(node "$root/scripts/bump.mjs" --print)"
real_home="$HOME"
server_pid=""
app_pid=""
server_from=0
version_touched=0
failed=0
finished=0

# ---------------------------------------------------------------------------
# 记录

say() { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
record() { # record <PASS|FAIL|INFO> <场景> <说明>
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$checks"
  case "$1" in
    PASS) printf '  ✓ %s\n' "$3" ;;
    FAIL)
      printf '  ✗ %s\n' "$3"
      failed=1
      ;;
    *) printf '  · %s\n' "$3" ;;
  esac
}
check() { # check <场景> <说明> <命令…>：命令成功记 PASS，否则 FAIL
  local scenario="$1" what="$2"
  shift 2
  if "$@" > /dev/null; then record PASS "$scenario" "$what"; else record FAIL "$scenario" "$what"; fi
}

# ---------------------------------------------------------------------------
# 进程

running_pids() { # 可执行文件完整路径等于 $1 的进程（大小写不敏感，见 measure-macos.sh）
  ps -axo pid=,comm= | awk -v exe="$1" '{ p = $1; sub(/^[ \t]*[0-9]+[ \t]+/, ""); if (tolower($0) == tolower(exe)) print p }'
}

app_exe() {
  local name
  name="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Contents/Info.plist")"
  printf '%s/%s\n' "$(cd "$app/Contents/MacOS" && pwd -P)" "$name"
}

log_size() { if [ -f "$wisp_log" ]; then wc -c < "$wisp_log" | tr -d ' '; else echo 0; fi; }

# launch <keychain 探针：write|read|-> ：经 LaunchServices 启动（新实例、不抢前台、静默启动），设置 app_pid、log_from
# 与 server_from（启动之前服务器请求日志的行数：本次启动的请求都在它之后，钥匙串等待再久也不会漏掉）
launch() {
  local probe="$1" exe before p k
  exe="$(app_exe)"
  before=" $(running_pids "$exe" | tr '\n' ' ') "
  log_from="$(log_size)"
  server_from="$(server_mark)"
  local args=(-n -g --env "HOME=$home" --stdout "$results/app.out.log" --stderr "$results/app.err.log")
  if [ "$probe" != - ]; then args+=(--env "WISP_REHEARSAL_KEYCHAIN=$probe"); fi
  open "${args[@]}" "$app" --args --autostart || die "open $app 失败"
  for ((k = 0; k < 150; k++)); do
    for p in $(running_pids "$exe"); do
      case "$before" in *" $p "*) ;; *)
        app_pid="$p"
        return 0
        ;;
      esac
    done
    sleep 0.1
  done
  die "15 秒内没有找到新启动的 $exe"
}

# 本次启动以来的日志
log_since() { tail -c "+$((log_from + 1))" "$wisp_log" 2> /dev/null || true; }

# wait_log <ERE> <超时秒>：等本次启动以来的日志出现匹配行（输出该行）；应用中途退出或超时返回 1
wait_log() {
  local pattern="$1" timeout="$2" deadline line
  deadline=$((SECONDS + timeout))
  while [ "$SECONDS" -lt "$deadline" ]; do
    line="$(log_since | grep -E -m1 -- "$pattern" || true)"
    if [ -n "$line" ]; then
      printf '%s\n' "$line"
      return 0
    fi
    if [ -n "$app_pid" ] && ! kill -0 "$app_pid" 2> /dev/null; then
      line="$(log_since | grep -E -m1 -- "$pattern" || true)"
      [ -n "$line" ] && printf '%s\n' "$line" && return 0
      return 1
    fi
    sleep 0.5
  done
  return 1
}

# 退出应用：先发「退出」Apple 事件（走 RunEvent::Exit，写掉待写的设置），10 s 内没退再 SIGTERM / SIGKILL
quit_app() {
  local scenario="$1" k
  [ -n "$app_pid" ] || return 0
  if kill -0 "$app_pid" 2> /dev/null; then
    osascript -e "tell application id \"$identifier\" to quit" > /dev/null 2>&1 &
    local osa=$!
    for ((k = 0; k < 100; k++)); do
      kill -0 "$app_pid" 2> /dev/null || break
      sleep 0.1
    done
    kill "$osa" 2> /dev/null || true
    wait "$osa" 2> /dev/null || true
    if kill -0 "$app_pid" 2> /dev/null; then
      record INFO "$scenario" "「退出」事件 10 s 内没有生效，改用 SIGTERM"
      kill "$app_pid" 2> /dev/null || true
      for ((k = 0; k < 50; k++)); do
        kill -0 "$app_pid" 2> /dev/null || break
        sleep 0.1
      done
      kill -9 "$app_pid" 2> /dev/null || true
    elif log_since | grep -qE ': exit$'; then
      record INFO "$scenario" "应用正常退出（日志有 exit）"
    fi
  fi
  app_pid=""
}

# ---------------------------------------------------------------------------
# 清理

cleanup() {
  local code=$?
  set +e
  # set -u / set -e 打断时 EXIT trap 里的 $? 不一定非零：没有跑到结尾就按失败退出
  if [ "$code" = 0 ] && [ "$finished" != 1 ]; then code=1; fi
  quit_app cleanup > /dev/null 2>&1
  if [ -n "$server_pid" ]; then kill "$server_pid" 2> /dev/null; wait "$server_pid" 2> /dev/null; fi
  if [ "$version_touched" = 1 ]; then
    (cd "$root" && node scripts/bump.mjs "$original_version" > /dev/null) || echo "updater-rehearsal: 恢复版本号失败，请手动运行 node scripts/bump.mjs $original_version" >&2
  fi
  # WebKit 与系统框架不看 HOME，按 identifier 把缓存建在真实的 ~/Library 与每用户的 cache / temp 目录（getconf
  # DARWIN_USER_CACHE_DIR / DARWIN_USER_TEMP_DIR）下：只删演练 identifier 的这几个目录
  local d
  for d in "Caches/$identifier" "WebKit/$identifier" "HTTPStorages/$identifier" "Saved Application State/$identifier.savedState"; do
    rm -rf "${real_home:?}/Library/$d"
  done
  for d in "$(getconf DARWIN_USER_CACHE_DIR 2> /dev/null)" "$(getconf DARWIN_USER_TEMP_DIR 2> /dev/null)"; do
    [ -n "$d" ] && [ -d "${d%/}/$identifier" ] && rm -rf "${d%/}/${identifier:?}"
  done
  # cargo-target 里只留编译缓存：打包产物（含签名的 .app.tar.gz(.sig)）复制到 builds/ 之后就不要了
  rm -rf "${cargo_target:?}/release/bundle"
  if [ "$keep" = 0 ]; then
    if [ -d "$home" ]; then
      [ -f "$keychain" ] && HOME="$home" security delete-keychain "$keychain" 2> /dev/null
      rm -rf "$home"
    fi
    # 演练包与测试密钥不留在磁盘上
    rm -rf "${builds:?}" "${www:?}"
  fi
  # 删完之后再注销所有演练 .app 的 LaunchServices 记录（先删，免得注销与删除之间又被登记）：不只是启动过的那个，打包、
  # ditto、codesign 之后 Spotlight 也会异步登记它们（编译缓存里的那份复制后就删了，记录却可能晚到）。lsregister -u 对已经
  # 删掉的路径同样有效，所以不看文件在不在
  local lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
  local a
  for a in "$app" "$builds/$from/Wisp.app" "$builds/$to/Wisp.app" "$cargo_target/release/bundle/macos/Wisp.app"; do
    [ -x "$lsregister" ] && "$lsregister" -u "$a" > /dev/null 2>&1
  done
  exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---------------------------------------------------------------------------
# 构建

plist() { /usr/libexec/PlistBuddy -c "Print :$2" "$1/Contents/Info.plist"; }
cdhash() { codesign -dvvv "$1" 2>&1 | awk -F= '/^CDHash=/{ print $2; exit }'; }

# build <版本>：改版本号、release 构建（只打 .app 与更新器产物），复制到 <work>/builds/<版本>/。编译用单独的
# CARGO_TARGET_DIR（仓库的 target/release 不混进演练构建——scripts/perf 的测量命令用的就是那里的产物），分两步：
#   1. tauri build --no-bundle：前端 + cargo build --release，**环境里没有任何秘密**（数百个 build script、proc-macro 与
#      vite 都在这一步跑）；
#   2. tauri bundle --bundles app：只打包与签名（不跑 beforeBuildCommand、不编译），私钥路径与口令只在这一条命令的环境里
build_version() {
  local v="$1" out="$builds/$1" bundle="$cargo_target/release/bundle/macos" log="$results/build-$1.log"
  step "构建 ${v}（release，.app + 更新器产物，${signer_label}签名）"
  (cd "$root" && node scripts/bump.mjs "$v" > /dev/null)
  version_touched=1
  rm -rf "$bundle"
  local t0=$SECONDS
  (
    cd "$root"
    unset TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD TAURI_SIGNING_PRIVATE_KEY_PATH TAURI_PRIVATE_KEY TAURI_PRIVATE_KEY_PASSWORD
    CARGO_TARGET_DIR="$cargo_target" pnpm -C apps/desktop tauri build --no-bundle --config "$work/overlay.json"
  ) > "$log" 2>&1 || {
    tail -n 30 "$log" >&2
    die "编译 $v 失败（完整日志 ${log}）"
  }
  local t1=$SECONDS
  (
    cd "$root"
    unset TAURI_SIGNING_PRIVATE_KEY_PATH TAURI_PRIVATE_KEY TAURI_PRIVATE_KEY_PASSWORD
    # 测试私钥（绝对路径，见文件头）与口令只在这一条命令的环境里（不出现在任何命令行上、不回显）
    CARGO_TARGET_DIR="$cargo_target" TAURI_SIGNING_PRIVATE_KEY="$test_key" \
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$test_key_password" \
      pnpm -C apps/desktop tauri bundle --bundles app --config "$work/overlay.json"
  ) >> "$log" 2>&1 || {
    tail -n 30 "$log" >&2
    die "打包 / 签名 $v 失败（完整日志 ${log}）"
  }
  [ -f "$bundle/Wisp.app.tar.gz.sig" ] || die "打包 $v 没有生成更新器签名（${bundle}）"
  rm -rf "$out"
  mkdir -p "$out"
  ditto "$bundle/Wisp.app" "$out/Wisp.app"
  cp "$bundle/Wisp.app.tar.gz" "$out/Wisp_${v}_aarch64.app.tar.gz"
  cp "$bundle/Wisp.app.tar.gz.sig" "$out/Wisp_${v}_aarch64.app.tar.gz.sig"
  rm -rf "$bundle" # 只留 builds/ 里的一份（按 --keep 处理）
  say "  → ${out}（$(du -sh "$out/Wisp.app" | cut -f1) .app，$(du -h "$out/Wisp_${v}_aarch64.app.tar.gz" | cut -f1) tar.gz；编译 $((t1 - t0)) s，打包签名 $((SECONDS - t1)) s）"
}

mkdir -p "$results"
: > "$checks"
: > "$server_log"
# 上一次运行留下的快照与种子摘要不能混进这一次的比较
rm -f "$results/data-before.json" "$results/data-after.json" "$results/seed-history.json"

signing_dir="$builds/signing"
test_key="$signing_dir/wisp-test.key"
test_key_password=""
if [ "$skip_build" = 1 ]; then
  [ -f "$builds/port" ] && [ -f "$signing_dir/mode" ] && [ -d "$builds/$from/Wisp.app" ] && [ -d "$builds/$to/Wisp.app" ] ||
    die "--skip-build：$builds 里没有上一次的构建（不带 --keep 的运行结束时会删掉构建）"
  port="$(cat "$builds/port")"
  [ "$(cat "$signing_dir/mode")" = test ] ||
    die "--skip-build：上一次的构建不是用一次性测试密钥签的，请重新构建"
else
  if [ "$allow_dirty" = 0 ] && ! (cd "$root" && git diff --quiet -- "${version_files[@]}"); then
    die "版本号相关文件有未提交的修改（${version_files[*]}）；确认无误可加 --allow-dirty"
  fi
  rm -rf "$builds"
  mkdir -p "$builds" "$signing_dir"
  port="$(node "$helper" free-port)"
  echo "$port" > "$builds/port"
  echo test > "$signing_dir/mode"
  # 一次性测试密钥：口令只在本次运行的内存里（它不保护任何正式资产；--skip-build 不再需要它）
  test_key_password="$(uuidgen)"
  (cd "$root" && pnpm -C apps/desktop exec tauri signer generate --ci -p "$test_key_password" -w "$test_key") > "$results/signer.log" 2>&1 ||
    die "生成测试密钥失败（$results/signer.log）"
fi
signer_label="一次性测试密钥"
pubkey_label="一次性测试公钥"
expected_pubkey="$(tr -d '[:space:]' < "$test_key.pub")"
pubkey_args=(--pubkey-file "$test_key.pub")
if ! out="$(node "$helper" overlay --port "$port" --identifier "$identifier" --out "$work/overlay.json" ${pubkey_args[@]+"${pubkey_args[@]}"} 2>&1)"; then
  die "叠加配置不安全：$out"
fi
say "${out#* }"
if [ "$skip_build" = 0 ]; then
  build_version "$from"
  build_version "$to"
  (cd "$root" && node scripts/bump.mjs "$original_version" > /dev/null)
  version_touched=0
fi

step "产物核对"
for v in "$from" "$to"; do
  check build "${v}：Info.plist 版本为 $v" test "$(plist "$builds/$v/Wisp.app" CFBundleShortVersionString)" = "$v"
  check build "${v}：identifier 为演练用的 $identifier" test "$(plist "$builds/$v/Wisp.app" CFBundleIdentifier)" = "$identifier"
  check build "${v}：codesign --verify --deep --strict 通过" codesign --verify --deep --strict "$builds/$v/Wisp.app"
  check build "${v}：ad-hoc 签名（与正式构建相同，tauri.conf.json signingIdentity \"-\"）" \
    sh -c "codesign -dv '$builds/$v/Wisp.app' 2>&1 | grep -q '^Signature=adhoc'"
  if out="$(node "$helper" verify --file "$builds/$v/Wisp_${v}_aarch64.app.tar.gz" --sig "$builds/$v/Wisp_${v}_aarch64.app.tar.gz.sig" --version "$v" ${pubkey_args[@]+"${pubkey_args[@]}"} 2>&1)"; then
    record PASS build "${v}：更新器签名可被${pubkey_label}校验，trusted comment 记录了 version:$v"
    printf '%s\n' "$out" | sed -n 's/^trusted comment: /    trusted comment: /p'
  else
    record FAIL build "${v}：更新器签名校验失败：$out"
  fi
done
from_cdhash="$(cdhash "$builds/$from/Wisp.app")"
to_cdhash="$(cdhash "$builds/$to/Wisp.app")"
record INFO build "cdhash：$from = ${from_cdhash}，$to = $to_cdhash"
check build "两个版本的 cdhash 不同（ad-hoc 签名的 designated requirement 就是 cdhash）" test "$from_cdhash" != "$to_cdhash"
check build "测试公钥的 key id 与正式公钥不同（演练构建不认正式签名，本机没有任何正式签名的包）" \
  sh -c '! node "$1" verify --file "$2" --sig "$3" > /dev/null 2>&1' _ "$helper" "$builds/$to/Wisp_${to}_aarch64.app.tar.gz" "$builds/$to/Wisp_${to}_aarch64.app.tar.gz.sig"

# ---------------------------------------------------------------------------
# 服务器与 latest.json

step "本机更新服务器（http://127.0.0.1:$port/）"
rm -rf "$www"
mkdir -p "$www/genuine" "$www/tampered" "$www/forged"
tarball="Wisp_${to}_aarch64.app.tar.gz"
cp "$builds/$to/$tarball" "$www/genuine/$tarball"
cp "$builds/$to/$tarball.sig" "$www/genuine/$tarball.sig"
node "$helper" tamper --in "$builds/$to/$tarball" --out "$www/tampered/$tarball" | sed 's/^/  /'
cp "$builds/$to/$tarball.sig" "$www/tampered/$tarball.sig"
cp "$builds/$to/$tarball" "$www/forged/$tarball"
node "$helper" forge --in "$builds/$to/$tarball" --out "$www/forged/$tarball.sig" --version "$to" | sed 's/^/  /'
tarball_bytes="$(wc -c < "$builds/$to/$tarball" | tr -d ' ')"

# serve_manifest <genuine|tampered|forged>：latest.json 指向对应目录里的包与签名
serve_manifest() {
  node "$helper" manifest --tarball "$1/$tarball" --sig "$www/$1/$tarball.sig" --version "$to" \
    --notes "Wisp ${to}（本机更新演练：$1）" --base-url "http://127.0.0.1:$port" --out "$www/latest.json" > /dev/null
}
serve_manifest genuine
node "$helper" serve --dir "$www" --port "$port" --log "$server_log" > "$results/server.out.log" 2>&1 &
server_pid=$!
for ((k = 0; k < 50; k++)); do
  grep -q '^serving ' "$results/server.out.log" 2> /dev/null && break
  kill -0 "$server_pid" 2> /dev/null || die "本机服务器没有启动：$(cat "$results/server.out.log")"
  sleep 0.1
done
say "  serving ${www}（请求日志 ${server_log}）"
server_mark() { wc -l < "$server_log" | tr -d ' '; }
# served <起始行号> <路径 ERE>：起始行之后有没有 200 的 GET 请求
served() { tail -n "+$(($1 + 1))" "$server_log" | grep -E "\"method\":\"GET\",\"path\":\"$2\",\"status\":200" | tail -n 1; }

# ---------------------------------------------------------------------------
# 隔离 HOME：一次性钥匙串、演练设置、安装 <from>

step "隔离 HOME（${home}）"
if [ -f "$keychain" ]; then HOME="$home" security delete-keychain "$keychain" 2> /dev/null || true; fi
rm -rf "$home"
mkdir -p "$home/Applications" "$home/Library/Keychains"
keychain_password="$(uuidgen)" # 一次性钥匙串的口令：只保护这个临时文件
HOME="$home" security create-keychain -p "$keychain_password" "$keychain"
HOME="$home" security set-keychain-settings "$keychain" # 不自动锁定
HOME="$home" security unlock-keychain -p "$keychain_password" "$keychain"
if [ "$keychain_prompt" = 1 ]; then
  say "  一次性钥匙串的口令（系统授权框要「登录」钥匙串的密码时输入它，不是你的登录密码）：$keychain_password"
fi
check setup "隔离 HOME 的默认钥匙串是一次性的 keychain 文件（用户的登录钥匙串不在搜索列表里）" \
  sh -c "HOME='$home' security default-keychain | grep -qF '$keychain' && ! HOME='$home' security list-keychains | grep -q \"\$HOME/Library/Keychains/login\""
node "$helper" seed --home "$home" | sed 's/^/  /'
ditto "$builds/$from/Wisp.app" "$app"
check setup "已安装 $from 到 ${app}（所在目录可写 → auto_install 走「立即安装」）" test "$(plist "$app" CFBundleShortVersionString)" = "$from"

probe_present() { HOME="$home" security find-generic-password -s wisp -a wisp-updater-rehearsal-probe "$keychain" > /dev/null 2>&1; }

# 运行中的应用记下的演练构建配置：版本、本机端点、预期的公钥
check_build_line() {
  local scenario="$1" v="$2" line
  if line="$(wait_log 'updater rehearsal build: ' 20)"; then
    check "$scenario" "运行的是演练构建 $v" grep -q "version=$v " <<< "$line"
    check "$scenario" "端点是本机服务器 http://127.0.0.1:$port/latest.json" grep -qF "endpoints=http://127.0.0.1:$port/latest.json " <<< "$line"
    check "$scenario" "验签用的是${pubkey_label}（运行中的应用记下的内嵌公钥）" grep -qF "pubkey=$expected_pubkey " <<< "$line"
  else
    record FAIL "$scenario" "日志里没有 updater rehearsal build 行（不是演练构建？）"
  fi
}

# ---------------------------------------------------------------------------
# A：notify（默认策略）——只提示、不下载；写入钥匙串探针

step "A：默认策略 notify——检查到 $to 只提示，不下载"
node "$helper" set-policy --home "$home" --policy notify --reset-check > /dev/null
launch write
check A "$from 启动（日志 wisp $from starting）" wait_log "wisp $from starting" 20
check_build_line A "$from"
if line="$(wait_log 'hotkeys: translate_selection=' 20)"; then
  check A "五个全局快捷键都没有注册（演练设置全部不绑定，不和正在运行的 Wisp 抢组合键）" \
    sh -c '! printf "%s\n" "$1" | grep -qE "=(Registered|Fallback)"' _ "$line"
else
  check A "没有注册任何全局快捷键（日志没有 Registered / Fallback）" sh -c "! tail -c +$((log_from + 1)) '$wisp_log' | grep -qE 'hotkeys: .*=(Registered|Fallback)'"
fi
if line="$(wait_log 'updater rehearsal: keychain probe (written|write failed)' 20)"; then
  probe_fp="$(sed -n 's/.*written fingerprint=\([0-9a-f]*\).*/\1/p' <<< "$line")"
  check A "$from 经应用自己的钥匙串句柄写入探针条目（指纹 ${probe_fp:-?}）" test -n "$probe_fp"
else
  probe_fp=""
  record FAIL A "没有钥匙串探针写入的日志"
fi
check A "钥匙串里有探针条目（service wisp，只读属性、不读内容）" probe_present
check A "启动 30 s 后自动检查发现 ${to}（update available version=${to}）" wait_log "update available version=$to" 75
sleep 5
check A "notify 策略不下载（日志没有 update download started）" sh -c "! tail -c +$((log_from + 1)) '$wisp_log' | grep -q 'update download started'"
check A "服务器只被请求了 latest.json（没有请求安装包）" sh -c "[ -n \"\$(tail -n +$((server_from + 1)) '$server_log' | grep '\"path\":\"/latest.json\"')\" ] && ! tail -n +$((server_from + 1)) '$server_log' | grep -q 'tar.gz'"
quit_app A
# 给 0.1.0 建好的 clipboard.db 写 1 条剪贴板记录与 1 条识别历史（演练关闭了剪贴板监听，不碰用户的剪贴板）；写入后的行数与
# 摘要记进 seed-history.json，E 里核对更新前后的快照都有这两条
if out="$(node "$helper" seed-history --home "$home" --out "$results/seed-history.json" 2>&1)"; then
  record INFO A "${out#* }"
else
  record FAIL A "写入种子历史失败：$out"
fi

# ---------------------------------------------------------------------------
# B / C：验签失败的两种情形——.app 必须原封不动

negative() { # negative <场景> <manifest> <说明>
  local scenario="$1" kind="$2" what="$3" line
  step "${scenario}：${what}（auto_install）——验签失败，.app 原封不动"
  serve_manifest "$kind"
  node "$helper" set-policy --home "$home" --policy auto_install --reset-check > /dev/null
  launch -
  check "$scenario" "$from 启动" wait_log "wisp $from starting" 20
  check "$scenario" "自动检查发现 $to" wait_log "update available version=$to" 75
  if line="$(wait_log 'automatic update install failed|update installed version=' 60)"; then
    say "    $(sed 's/^[^ ]* //' <<< "$line")"
    check "$scenario" "安装被拒绝：签名校验失败" grep -qi 'automatic update install failed: .*signature' <<< "$line"
  else
    record FAIL "$scenario" "60 s 内没有安装结果"
  fi
  check "$scenario" "安装包确实下载了（GET /$kind/$tarball 200）" test -n "$(served "$server_from" "/$kind/$tarball")"
  check "$scenario" "日志没有 update installed" sh -c "! tail -c +$((log_from + 1)) '$wisp_log' | grep -q 'update installed version='"
  check "$scenario" ".app 仍是 ${from}（Info.plist）" test "$(plist "$app" CFBundleShortVersionString)" = "$from"
  check "$scenario" ".app 的 cdhash 没变" test "$(cdhash "$app")" = "$from_cdhash"
  quit_app "$scenario"
}
negative B tampered "篡改过的包 + 真签名（${signer_label}）"
negative C forged "别的密钥签的包"

# ---------------------------------------------------------------------------
# D：真签名的包——下载、验签、替换 .app

serve_manifest genuine
if [ "$interactive" = 0 ]; then
  step "D：真签名的包（${signer_label}，auto_install）——下载、验签、安装"
  node "$helper" set-policy --home "$home" --policy auto_install --reset-check > /dev/null
  # 「更新前」的快照在安装之前拍（种子历史已写入）：E 结束后与它比较，并核对种子记录与演练设置都在
  node "$helper" snapshot --home "$home" --out "$results/data-before.json"
  launch read
  check D "$from 启动" wait_log "wisp $from starting" 20
  check_build_line D "$from"
  if line="$(wait_log 'updater rehearsal: keychain probe (readable|missing|read failed)' 20)"; then
    check D "同一个二进制读探针不弹框、内容一致（指纹 ${probe_fp}）" grep -q "readable fingerprint=$probe_fp" <<< "$line"
  else
    record FAIL D "$from 读钥匙串探针没有结果"
  fi
  check D "自动检查发现 $to" wait_log "update available version=$to" 75
  if line="$(wait_log "update downloaded version=$to bytes=" 60)"; then
    check D "下载完整（$tarball_bytes 字节）" grep -q "bytes=$tarball_bytes\$" <<< "$line"
  else
    record FAIL D "没有下载完成的日志"
  fi
  if line="$(wait_log "update installed version=$to|automatic update install failed" 60)"; then
    check D "验签通过并安装（update installed version=${to}）" grep -q "update installed version=$to" <<< "$line"
  else
    record FAIL D "60 s 内没有安装结果"
  fi
  check D "服务器：GET /genuine/$tarball 200（$tarball_bytes 字节）" sh -c "tail -n +$((server_from + 1)) '$server_log' | grep -q '\"path\":\"/genuine/$tarball\",\"status\":200,\"bytes\":$tarball_bytes'"
  check D "原位置的 .app 已是 ${to}（旧进程仍在运行）" test "$(plist "$app" CFBundleShortVersionString)" = "$to"
  check D "旧进程在 .app 被替换之后仍然存活" kill -0 "$app_pid"
  check D ".app 的 cdhash 是 $to 的" test "$(cdhash "$app")" = "$to_cdhash"
  sleep 2
  quit_app D
else
  step "D（交互）：请在 GUI 里完成更新"
  node "$helper" set-policy --home "$home" --policy notify --reset-check > /dev/null
  # 「更新前」的快照在安装之前拍（同无人值守）
  node "$helper" snapshot --home "$home" --out "$results/data-before.json"
  launch read
  check D "$from 启动" wait_log "wisp $from starting" 20
  cat << EOF

  演练应用已在后台运行（菜单栏多了一个 Wisp 图标，identifier ${identifier}；演练设置的界面语言是英文）。约 30 s 后它会发现 ${to}：
    1. 菜单栏的 Wisp 图标带上角标；点开，状态行是「Wisp ${to} is available · Click to view」，点它打开 设置 → 关于；
    2. 关于页显示「Version ${to} is available」与更新内容「Wisp ${to}（本机更新演练：genuine）」；点「Download and install」，
       进度走完后变为「${to} is installed and takes effect after a relaunch」（托盘状态行同步变为已就绪）；
    3. 点「Relaunch」。${to} 启动后读取钥匙串探针：系统弹框要「登录」钥匙串的密码——输入上面打印的一次性口令，点「始终允许」；
       脚本随后再启动一次 ${to} 读同一个条目，这一次不应再弹框（E2）。
  脚本等待日志里出现 ${to} 的启动（最多 15 分钟）……
EOF
  check D "自动检查发现 $to" wait_log "update available version=$to" 75
  app_pid="" # 用户点「重启」后由应用自己重新拉起，不再跟踪旧进程
  check D "用户点了「下载并安装」：验签通过并安装" wait_log "update installed version=$to" 900
  check D "用户点了「重启」：$to 启动" wait_log "wisp $to starting" 900
fi

# ---------------------------------------------------------------------------
# E：更新之后

step "E：重新启动后的 $to"
if [ "$interactive" = 0 ]; then
  # 清掉检查时间：让 $to 启动 30 s 后对同一份 latest.json 再检查一次（auto_install 策略下若版本比较有误会循环更新）。
  # 服务器请求的起点在 launch 里、启动之前记下（server_from）：钥匙串等待再久，之后的「没有再次下载」也覆盖整个运行期
  node "$helper" set-policy --home "$home" --policy auto_install --reset-check > /dev/null
  launch "$([ "$keychain_prompt" = 1 ] && echo read || echo -)"
  check E "$to 启动（日志 wisp $to starting）" wait_log "wisp $to starting" 20
else
  # 应用自己重新拉起的 ${to}（环境沿用旧进程的：隔离 HOME 与 WISP_REHEARSAL_KEYCHAIN=read）
  app_pid="$(running_pids "$(app_exe)" | head -n 1)"
  log_from="$(grep -b -m1 "wisp $to starting" "$wisp_log" | cut -d: -f1)"
fi
check_build_line E "$to"
check E "钥匙串条目在更新后还在" probe_present
# 条目的访问控制（dump-keychain -a 只列属性与 ACL，不读内容）：写入它的应用的 designated requirement 与 partition_id。
# 新版本满足这条 requirement 时读取不弹框（固定证书签名）；不满足时第一次读取弹框要钥匙串密码（ad-hoc）
acl="$(HOME="$home" security dump-keychain -a "$keychain" 2> /dev/null)"
probe_req="$(awk '/"acct"<blob>="wisp-updater-rehearsal-probe"/{ f = 1 } f && /requirement:/{ sub(/^ *requirement: /, ""); print; exit }' <<< "$acl")"
probe_part="$(awk '/"acct"<blob>="wisp-updater-rehearsal-probe"/{ f = 1 } f && /partition_id/{ p = 1 } p && /description:/{ sub(/^ *description: /, ""); print; exit }' <<< "$acl")"
if [ -z "$probe_req" ]; then
  record FAIL E "读不到探针条目的 ACL（dump-keychain -a）"
elif codesign --verify -R="$probe_req" "$app" 2> /dev/null; then
  record INFO E "$to 满足条目 ACL 的 requirement（${probe_req}，partition ${probe_part}）：读取不弹框"
else
  record INFO E "$to 不满足 $from 写入条目时的 ACL（requirement ${probe_req}，partition ${probe_part}）：第一次读取弹框要钥匙串密码，「始终允许」后不再弹（ad-hoc 签名预期如此）"
fi
if [ "$keychain_prompt" = 0 ]; then
  record INFO E "没有让 $to 读钥匙串探针（会弹系统授权框；--keychain-prompt / --interactive 时读并核对指纹）"
elif line="$(wait_log 'updater rehearsal: keychain probe (readable|missing|read failed)' "$keychain_wait")"; then
  if [ -n "$probe_fp" ] && grep -q "readable fingerprint=$probe_fp" <<< "$line"; then
    record PASS E "$to 读到了 $from 写入的钥匙串条目（指纹 $probe_fp 一致）"
    keychain_read_ok=1
  else
    record FAIL E "$to 读钥匙串探针：$(sed 's/.*keychain probe //' <<< "$line")"
  fi
elif log_since | grep -q 'updater rehearsal: keychain probe read started'; then
  record FAIL E "$to 读钥匙串探针 ${keychain_wait} s 内没有返回（系统授权框没有回答？）"
else
  record FAIL E "$to 没有开始读钥匙串探针"
fi
if [ "$interactive" = 0 ]; then
  check E "对同一份 latest.json 报告已是最新（update check: up to date，不会循环更新）" wait_log 'update check: up to date' 75
  check E "启动以来没有再次下载（服务器没有安装包请求，日志没有 update download started）" \
    sh -c "! tail -n +$((server_from + 1)) '$server_log' | grep -q 'tar.gz' && ! tail -c +$((log_from + 1)) '$wisp_log' | grep -q 'update download started'"
fi
check E "Info.plist 版本为 $to" test "$(plist "$app" CFBundleShortVersionString)" = "$to"
check E "更新后的 .app 签名完整（codesign --verify --deep --strict）" codesign --verify --deep --strict "$app"
check E "更新后的 .app 没有 com.apple.quarantine（更新器写入的文件不带隔离属性，不经 Gatekeeper）" \
  sh -c "! xattr -r '$app' 2> /dev/null | grep -q com.apple.quarantine"
check E ".app 目录权限 755（其他用户也能启动）" test "$(stat -f %Lp "$app")" = 755
quit_app E
node "$helper" snapshot --home "$home" --out "$results/data-after.json"
if out="$(node "$helper" compare --before "$results/data-before.json" --after "$results/data-after.json" --seed "$results/seed-history.json" 2>&1)"; then
  record PASS E "设置与数据在更新前后一致（安装前拍的快照 vs 更新后：settings.json、state.json 除允许的字段外、剪贴板与识别历史逐行一致、其余文件），种子记录与演练设置都在"
  printf '%s\n' "$out" | grep '允许的变化\|新增文件\|种子记录\|演练设置' | sed 's/^ */    /' || true
else
  record FAIL E "设置与数据在更新前后不一致（data-before.json / data-after.json）"
  printf '%s\n' "$out" | sed 's/^ */    /'
fi
leftovers="$(find "$(getconf DARWIN_USER_TEMP_DIR)" "$home/Applications" -maxdepth 1 -name 'tauri_*_app*' -newer "$work/overlay.json" 2> /dev/null | wc -l | tr -d ' ')"
record INFO E "更新器留下的临时目录（tauri_*_app*）：$leftovers 个"

# ---------------------------------------------------------------------------
# E2：「始终允许」之后再启动一次 <to>——读取钥匙串不再弹框

if [ "$keychain_read_ok" = 1 ]; then
  step "E2：再启动一次 ${to}——「始终允许」之后读取钥匙串不再弹框"
  quit_app E2
  launch read
  check E2 "$to 启动" wait_log "wisp $to starting" 20
  if line="$(wait_log 'updater rehearsal: keychain probe (readable|missing|read failed)' 15)"; then
    check E2 "读取不再弹框：15 s 内读到同一个值（指纹 ${probe_fp}）" grep -q "readable fingerprint=$probe_fp" <<< "$line"
  else
    record FAIL E2 "15 s 内没有读到（系统又弹了授权框？上一次没有点「始终允许」）"
  fi
  quit_app E2
elif [ "$keychain_prompt" = 1 ]; then
  record INFO E2 "E 没有读到同一个值，跳过「再启动一次不弹框」"
fi

# ---------------------------------------------------------------------------
# F：latest.json 本身不签名——把 <to> 的真包标成更高的版本（重放旧包、伪造版本号），插件按签名里记录的 version 拒绝

replay="$(node -e 'const [a, b, c] = process.argv[1].split(".").map(Number); console.log(`${a}.${b}.${c + 1}`)' "$to")"
step "F：latest.json 把 $to 的真包标成 ${replay}（auto_install）——按签名记录的版本拒绝，.app 原封不动"
quit_app F
rm -rf "$app"
ditto "$builds/$from/Wisp.app" "$app"
node "$helper" manifest --tarball "genuine/$tarball" --sig "$www/genuine/$tarball.sig" --version "$replay" \
  --notes "Wisp ${replay}（本机更新演练：replay）" --base-url "http://127.0.0.1:$port" --out "$www/latest.json" > /dev/null
node "$helper" set-policy --home "$home" --policy auto_install --reset-check > /dev/null
launch -
check F "重新装回的 $from 启动" wait_log "wisp $from starting" 20
if line="$(wait_log "update installed version=$replay|automatic update install failed" 90)"; then
  say "    $(sed 's/^[^ ]* //' <<< "$line")"
  check F "安装被拒绝：签名记录的版本（${to}）与 latest.json 宣称的（${replay}）不一致" \
    grep -q "automatic update install failed: .*signed for version $to .*announced version $replay" <<< "$line"
else
  record FAIL F "90 s 内没有安装结果"
fi
check F ".app 仍是 ${from}（Info.plist）" test "$(plist "$app" CFBundleShortVersionString)" = "$from"
quit_app F

# ---------------------------------------------------------------------------
# 结果

check isolation "整个演练期间没有注册任何全局快捷键（日志没有 Registered / Fallback）" sh -c "! grep -qE 'hotkeys: .*=(Registered|Fallback)' '$wisp_log'"
cp "$wisp_log" "$results/wisp.log" 2> /dev/null || true
step "结果（${checks}）"
pass=$(grep -c '^PASS' "$checks" || true)
fail=$(grep -c '^FAIL' "$checks" || true)
say "  必过项：$pass 通过，$fail 失败"
if [ "$keychain_prompt" = 0 ]; then
  say "  钥匙串：只核对了条目还在与 ACL；$to 读取要经系统授权框（ad-hoc 签名）。发版前用 --interactive 肉眼补一次："
  say "          在授权框里输入打印的一次性口令并点「始终允许」，脚本核对读到同一个值、再启动一次不再弹框"
fi
say "  日志与快照：$results"
if [ "$keep" = 1 ]; then
  say "  --keep：隔离 HOME、builds/（含一次性测试密钥）、www/ 保留，可以 --skip-build 重跑；用完删掉"
else
  say "  隔离 HOME、builds/、www/ 在退出时删除（编译缓存 $cargo_target 保留，可随时删除）"
fi
finished=1
[ "$failed" = 0 ] || exit 1
