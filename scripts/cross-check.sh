#!/usr/bin/env bash
# scripts/cross-check.sh
#
# 在非目标平台（例如 macOS 开发机）上对 Windows / Linux 目标做 `cargo check` / `cargo clippy`，
# 改动平台层（wisp-platform 的 Windows / Linux 实现）或窗口层的 native 模块（windows/native/*.rs）时，在本机就能发现
# 跨平台编译错误，不必等到 CI。只做 Rust 的类型检查与 lint，**不链接、不产出可执行文件**。
#
# 原理：check / clippy 仍会运行依赖的构建脚本，其中 ring、libsqlite3-sys 等要用目标平台的 C 工具链编译 C 代码，
# gtk / webkit2gtk 等 *-sys 要用 pkg-config 查询目标平台的系统库——本机都没有。由于 check 不链接，这些产物不会被
# 使用，所以用下面四个假工具替代：假编译器只生成空的目标文件，假归档器只生成空的静态库，假资源编译器只生成空的
# 资源文件，假 pkg-config 对任何包都报告「存在、版本足够新、没有额外标志」。Rust 代码（包括全部 `cfg(target_os = …)` 分支）照常完整地类型检查。
#
# 局限：不能发现链接错误、C 头文件 / 系统库版本问题、运行期错误——这些仍以 CI（windows-latest / ubuntu-22.04）为准。
#
# 用法：
#   scripts/cross-check.sh [windows|linux|all] [check|clippy] [额外的 cargo 参数…]
#   默认 all + clippy（-D warnings）。例：
#     scripts/cross-check.sh windows check -p wisp-desktop
#     scripts/cross-check.sh linux clippy --features wayland
# 产物放在 target/xcheck（与常规构建隔离，不会让本机构建缓存失效）。
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
targets_arg="${1:-all}"
mode="${2:-clippy}"
shift $(($# > 2 ? 2 : $#))

case "$targets_arg" in
  windows) targets=(x86_64-pc-windows-msvc) ;;
  linux) targets=(x86_64-unknown-linux-gnu) ;;
  all) targets=(x86_64-pc-windows-msvc x86_64-unknown-linux-gnu) ;;
  *)
    echo "用法：$0 [windows|linux|all] [check|clippy] [cargo 参数…]" >&2
    exit 2
    ;;
esac
case "$mode" in
  check | clippy) ;;
  *)
    echo "第二个参数只能是 check 或 clippy，实际为 $mode" >&2
    exit 2
    ;;
esac

shims="$root/target/xcheck/shims"
mkdir -p "$shims"

# 假 C/C++ 编译器：-E（cc crate 探测编译器种类）输出空；其余调用只创建 -o / -Fo / /Fo 指定的输出文件
cat >"$shims/fake-cc" <<'EOF'
#!/usr/bin/env bash
out=""
prev=""
for a in "$@"; do
  case "$a" in
    -E) exit 0 ;;
    --version | -v | -V) echo "fake-cc 0.0.0 (wisp cross-check)"; exit 0 ;;
    -Fo* | /Fo*) out="${a:3}" ;;
    -o*) [ "$a" != "-o" ] && out="${a:2}" ;;
  esac
  if [ "$prev" = "-o" ] || [ "$prev" = "-Fo" ] || [ "$prev" = "/Fo" ]; then out="$a"; fi
  prev="$a"
done
if [ -n "$out" ]; then mkdir -p "$(dirname "$out")" && : >"$out"; fi
exit 0
EOF

# 假归档器：GNU ar（`ar <ops> <archive> <objs…>`）与 lib.exe / llvm-lib（`-out:<archive>`）两种写法
cat >"$shims/fake-ar" <<'EOF'
#!/usr/bin/env bash
out=""
for a in "$@"; do
  case "$a" in
    -out:* | /out:* | -OUT:* | /OUT:*) out="${a#*:}" ;;
    *.a | *.lib) [ -z "$out" ] && out="$a" ;;
  esac
done
if [ -n "$out" ]; then mkdir -p "$(dirname "$out")" && : >"$out"; fi
exit 0
EOF

# 假 pkg-config：任何包都存在；--modversion 报告足够新的版本；其余查询（--libs / --cflags / --variable）输出空
cat >"$shims/fake-pkg-config" <<'EOF'
#!/usr/bin/env bash
for a in "$@"; do
  case "$a" in
    --version) echo "0.29.2"; exit 0 ;;
  esac
done
for a in "$@"; do
  if [ "$a" = "--modversion" ]; then
    n=0
    for b in "$@"; do
      case "$b" in
        -* | ">="* | "<"* | "="* | [0-9]*) ;;
        *) n=$((n + 1)) ;;
      esac
    done
    [ "$n" -eq 0 ] && n=1
    for _ in $(seq "$n"); do echo "999.0.0"; done
    exit 0
  fi
done
exit 0
EOF
# 假资源编译器（tauri-build → tauri-winres → embed-resource）：探测时自称 LLVM-RC 且支持 /no-preprocess
# （预处理交给上面的假编译器），编译时只创建 /fo 指定的输出文件
cat >"$shims/fake-rc" <<'EOF'
#!/usr/bin/env bash
out=""
prev=""
for a in "$@"; do
  case "$a" in
    "/?" | -V)
      echo "OVERVIEW: LLVM Resource Converter (wisp cross-check fake)"
      echo "  /no-preprocess"
      exit 0
      ;;
  esac
  if [ "$prev" = "/fo" ] || [ "$prev" = "-fo" ]; then out="$a"; fi
  prev="$a"
done
if [ -n "$out" ]; then mkdir -p "$(dirname "$out")" && : >"$out"; fi
exit 0
EOF
chmod +x "$shims/fake-cc" "$shims/fake-ar" "$shims/fake-pkg-config" "$shims/fake-rc"

export CARGO_TARGET_DIR="$root/target/xcheck"
export PKG_CONFIG="$shims/fake-pkg-config"
export PKG_CONFIG_ALLOW_CROSS=1
for t in "${targets[@]}"; do
  u="${t//-/_}"
  export "CC_$u=$shims/fake-cc" "CXX_$u=$shims/fake-cc" "AR_$u=$shims/fake-ar"
done
# Windows 资源编译（图标 / manifest）在非 Windows 主机上找不到 rc.exe / llvm-rc
export RC="$shims/fake-rc"

status=0
for t in "${targets[@]}"; do
  echo "==> cargo $mode --workspace --all-targets --target $t $*" >&2
  if [ "$mode" = clippy ]; then
    cargo clippy --manifest-path "$root/Cargo.toml" --workspace --all-targets --target "$t" "$@" -- -D warnings || status=1
  else
    cargo check --manifest-path "$root/Cargo.toml" --workspace --all-targets --target "$t" "$@" || status=1
  fi
done
exit $status
