#!/usr/bin/env bash
# scripts/release/verify-macos-launch-env.sh
#
# 核对打出来的 Wisp.app 的启动环境：`Contents/Info.plist` 的 `LSEnvironment` 必须带 `MallocSpaceEfficient = 1`（经 LaunchServices
# 打开时生效：主进程空闲约 13 MiB；丢了这一段会回到约 21 MiB，而且应用照常能用、没有任何报错），并且与源文件
# `apps/desktop/src-tauri/Info.plist` 的 `LSEnvironment` 逐项一致（源文件以后增加的启动环境变量同样要进包）。
# 源文件本身由 autostart.rs 的单测与 `LAUNCH_ENV` 对照；这里核对的是 tauri 合并 Info.plist 之后的**打包结果**。
# 静态核对，不启动应用，几秒钟。调用方：release.yml 的 macOS 构建（上传到草稿之前）、ci.yml 的 macOS 冒烟构建之后；
# 发布前复核下载的 dmg 时也可以手动运行。运行时是否生效另由 smoke-gate 核对应用日志的 launch_env。
# 用法：scripts/release/verify-macos-launch-env.sh <Wisp.app>
# 注意：与中文相邻的变量一律写成 ${name}——macOS 自带的 bash 3.2 在 UTF-8 语言环境下会把全角标点的字节当成变量名的一部分。
set -euo pipefail

app="${1:?用法：$0 <Wisp.app>}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
src="$root/apps/desktop/src-tauri/Info.plist"
plist="$app/Contents/Info.plist"

for tool in plutil node; do
  command -v "$tool" > /dev/null || {
    echo "缺少 ${tool}（只能在 macOS 上运行）" >&2
    exit 2
  }
done
if [ ! -f "$plist" ]; then
  echo "::error::没有 ${plist}（参数应是 .app 目录）"
  exit 1
fi

value="$(plutil -extract LSEnvironment.MallocSpaceEfficient raw -o - "$plist" 2> /dev/null || true)"
if [ "$value" != 1 ]; then
  echo "::error::${plist} 的 LSEnvironment.MallocSpaceEfficient 是「${value:-（没有）}」，应为 1：打包时没有带上 src-tauri/Info.plist 的 LSEnvironment？经 LaunchServices 启动时主进程会从约 13 MiB 回到约 21 MiB"
  exit 1
fi

# 与源文件逐项一致（plutil 的 JSON 输出不保证键的顺序，用 node 按键比较）
want="$(plutil -extract LSEnvironment json -o - "$src")"
got="$(plutil -extract LSEnvironment json -o - "$plist")"
if ! node -e '
  const [a, b] = process.argv.slice(1).map((s) => JSON.parse(s))
  const norm = (o) => JSON.stringify(Object.keys(o).sort().map((k) => [k, o[k]]))
  process.exit(norm(a) === norm(b) ? 0 : 1)
' "$want" "$got"; then
  echo "::error::${plist} 的 LSEnvironment 与源文件 apps/desktop/src-tauri/Info.plist 不一致：包里 ${got}，源文件 ${want}"
  exit 1
fi

echo "启动环境：${plist} 的 LSEnvironment = ${got}（与源文件一致）"
