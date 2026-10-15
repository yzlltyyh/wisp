#!/usr/bin/env bash
# scripts/release/verify-linux-deps.sh
#
# 核对 Linux 安装包声明的运行时依赖：
#   ① deb：主程序的 DT_NEEDED 库全部能由 Depends 声明的包（及其传递依赖）提供——在只装了声明依赖的系统上能启动；
#   ② rpm：Requires 包含 tauri.conf.json 的 bundle.linux.rpm.depends；
#   ③ CI 的 apt 列表（scripts/ci/linux-deps.sh）与 deb 声明一致：每个声明的运行时包都在列表中 -dev 包的依赖闭包里；
#   ④ glibc 版本需求不高于 ubuntu-22.04（2.35）。
# 用法：scripts/release/verify-linux-deps.sh <bundle 目录>（例如 target/x86_64-unknown-linux-gnu/release/bundle）
# 只能在 Debian / Ubuntu（有 dpkg、apt-cache）上运行；release.yml 的 Linux 作业在打包后调用。
set -euo pipefail

bundle="${1:?用法：$0 <bundle 目录>}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
max_glibc="2.35"

fail=0
err() {
  echo "::error::$*"
  fail=1
}
for tool in dpkg-deb dpkg apt-cache objdump node; do
  command -v "$tool" > /dev/null || {
    echo "缺少 $tool" >&2
    exit 2
  }
done

deb="$(find "$bundle" -name '*.deb' -type f -print -quit)"
rpm_file="$(find "$bundle" -name '*.rpm' -type f -print -quit)"
[ -n "$deb" ] || {
  echo "::error::$bundle 下没有 .deb"
  exit 1
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
dpkg-deb -x "$deb" "$tmp/root"
depends="$(dpkg-deb -f "$deb" Depends)"
echo "deb：$(basename "$deb")"
echo "  Depends: $depends"

# 声明的包名（去掉版本约束与可选项）
declared="$(echo "$depends" | tr ',' '\n' | sed -E 's/\|.*//; s/\(.*\)//; s/^ +//; s/ +$//' | grep -v '^$' | sort -u)"

# 声明包的传递依赖闭包（只看 Depends / PreDepends）+ 永远存在的基础包
closure="$tmp/closure.txt"
# shellcheck disable=SC2086
apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks --no-replaces --no-enhances \
  $declared 2> /dev/null | grep -vE '^\s' | sed 's/:.*//' | sort -u > "$closure"
printf '%s\n' libc6 libgcc-s1 libstdc++6 >> "$closure"

# ① 主程序（以及包内其他 ELF）的 DT_NEEDED
mapfile -t elfs < <(find "$tmp/root/usr/bin" "$tmp/root/usr/lib" -type f -perm -u+x 2> /dev/null | while read -r f; do
  if [ "$(head -c 4 "$f" | tail -c 3)" = ELF ]; then echo "$f"; fi
done)
[ ${#elfs[@]} -gt 0 ] || err "deb 中没有可执行的 ELF 文件"
needed="$(for f in "${elfs[@]}"; do objdump -p "$f" | awk '/NEEDED/ {print $2}'; done | sort -u)"
echo "  DT_NEEDED: $(echo "$needed" | tr '\n' ' ')"
ldcache="$(/sbin/ldconfig -p 2> /dev/null || ldconfig -p)"
# 提供某个文件的包名：依次尝试 ldconfig 给出的路径、真实路径与合并 /usr 前后的两种写法（dpkg 记录的是安装时的路径）
owner_of() {
  local p cand
  for p in "$@"; do
    for cand in "$p" "${p#/usr}" "/usr${p}"; do
      if dpkg -S "$cand" > "$tmp/owner" 2> /dev/null; then
        sed -n '1s/:.*//p' "$tmp/owner"
        return 0
      fi
    done
  done
  return 1
}

for lib in $needed; do
  # 注意 set -o pipefail：awk 提前退出会让 ldconfig 收到 SIGPIPE，这里先取完整输出再筛选
  path="$(awk -v l="$lib" '$1 == l && /x86-64/ {print $NF; exit}' <<< "$ldcache")"
  if [ -z "$path" ]; then
    err "找不到 $lib（runner 上没有安装提供它的包）"
    continue
  fi
  if ! pkg="$(owner_of "$path" "$(readlink -f "$path")")"; then
    err "$lib（$path）不属于任何已安装的包"
  elif ! grep -qx "$pkg" "$closure"; then
    err "$lib 由 $pkg 提供，但 deb 的 Depends 及其传递依赖里没有 $pkg（在 tauri.conf.json 的 bundle.linux.deb.depends 中声明）"
  else
    echo "    $lib → $pkg"
  fi
done

# ④ glibc 版本
glibc_need="$(for f in "${elfs[@]}"; do objdump -T "$f" 2> /dev/null | grep -oE 'GLIBC_[0-9]+\.[0-9]+' || true; done | sed 's/GLIBC_//' | sort -uV | tail -n 1)"
echo "  需要的 glibc：${glibc_need:-无}（上限 $max_glibc）"
if [ -n "$glibc_need" ] && [ "$(printf '%s\n%s\n' "$glibc_need" "$max_glibc" | sort -V | tail -n 1)" != "$max_glibc" ]; then
  err "需要 glibc $glibc_need，高于 ubuntu-22.04 的 $max_glibc"
fi

# ② rpm
conf="$root/apps/desktop/src-tauri/tauri.conf.json"
if [ -n "$rpm_file" ]; then
  echo "rpm：$(basename "$rpm_file")"
  if command -v rpm > /dev/null; then
    requires="$(rpm -qpR "$rpm_file" 2> /dev/null | sed -E 's/ .*//' | sort -u)"
    echo "  Requires: $(echo "$requires" | tr '\n' ' ')"
    for p in $(node -e 'const c=require(process.argv[1]); console.log((c.bundle?.linux?.rpm?.depends ?? []).join(" "))' "$conf"); do
      grep -qx "$p" <<< "$requires" || err "rpm 的 Requires 中没有 tauri.conf.json 声明的 $p"
    done
  else
    echo "  （没有 rpm 命令，跳过 Requires 核对）"
  fi
else
  err "$bundle 下没有 .rpm"
fi

# ③ CI 的 apt 列表与 deb 声明一致：每个声明的运行时包都在 CI 列表（-dev 包）的依赖闭包里，
#    即 CI 编译、测试时用到的正是安装包声明的这些库
apt_list="$(bash "$root/scripts/ci/linux-deps.sh" build --print)"
ci_closure="$tmp/ci-closure.txt"
# shellcheck disable=SC2086
apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks --no-replaces --no-enhances \
  $apt_list 2> /dev/null | grep -vE '^\s' | sed 's/:.*//' | sort -u > "$ci_closure"
for p in $declared; do
  grep -qx "$p" "$ci_closure" ||
    err "deb 声明的 $p 不在 CI 的 apt 列表（scripts/ci/linux-deps.sh）的依赖闭包里——CI 测试的环境与安装包声明不一致"
done

if [ "$fail" = 0 ]; then
  echo "✓ Linux 运行时依赖声明与产物一致"
else
  exit 1
fi
