#!/usr/bin/env bash
# scripts/ci/linux-deps.sh
#
# CI 与发布在 Linux runner 上安装的系统依赖——全仓库只在这里维护一份（「CI 的 apt 列表与 deb / rpm 的运行时依赖声明
# 一致」由 scripts/release/verify-linux-deps.sh 在发布时核对）。
#
# 用法：scripts/ci/linux-deps.sh [build|release|smoke] [--print]
#   build    编译 / 测试（默认）：WebKitGTK 4.1、ayatana-appindicator、librsvg、libxdo（Tauri 菜单）、patchelf
#   release  另加 rpm（rpmbuild）与 xdg-utils（AppImage 打包）
#   smoke    另加 xvfb 与 dbus（无显示器的 runner 上跑 WISP_SMOKE）
#   --print  只打印包名（每行一个），不安装
set -euo pipefail

profile="build"
print_only=0
for arg in "$@"; do
  case "$arg" in
    build | release | smoke) profile="$arg" ;;
    --print) print_only=1 ;;
    *)
      echo "用法：$0 [build|release|smoke] [--print]" >&2
      exit 2
      ;;
  esac
done

# 不引入 enigo / libxdo 之外的输入库：Linux 的输入模拟走 x11rb 的 XTest（纯 Rust）
packages=(
  libwebkit2gtk-4.1-dev
  libayatana-appindicator3-dev
  librsvg2-dev
  libxdo-dev
  patchelf
)
case "$profile" in
  release) packages+=(rpm xdg-utils) ;;
  smoke) packages+=(xvfb xauth dbus dbus-x11) ;;
esac

if [ "$print_only" = 1 ]; then
  printf '%s\n' "${packages[@]}"
  exit 0
fi

if [ "$(uname -s)" != Linux ]; then
  echo "linux-deps.sh 只能在 Linux 上运行" >&2
  exit 2
fi

sudo_cmd=()
if [ "$(id -u)" != 0 ]; then sudo_cmd=(sudo); fi

export DEBIAN_FRONTEND=noninteractive
# 推荐包（recommends）对编译没有用，跳过可以省下约一半的下载与安装时间
"${sudo_cmd[@]}" apt-get update -qq
"${sudo_cmd[@]}" apt-get install -y -qq --no-install-recommends "${packages[@]}"
