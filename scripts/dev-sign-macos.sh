#!/usr/bin/env bash
# scripts/dev-sign-macos.sh
#
# 用固定的自签证书「Wisp Dev」重签开发构建，让 TCC 授权（辅助功能、屏幕录制、输入监控）与钥匙串条目的 ACL
# 在重新编译后仍然有效。
#
# 为什么需要：TCC 与钥匙串 ACL 都绑定代码签名的 designated requirement。Tauri 默认的 ad-hoc 签名没有证书，
# designated requirement 就是 cdhash，每次编译都会变：TCC 认为是新应用、要求重新授权；钥匙串（keyring v1 的
# 传统 keychain）每次读取 Key 都弹「允许访问」对话框。用固定证书签名后，designated requirement 变成
# 「identifier io.github.yzlltyyh.wisp 且证书为 Wisp Dev」，与 cdhash 无关，重编译后授权依然有效。
#
# 用法：
#   scripts/dev-sign-macos.sh --init            在登录钥匙串中创建「Wisp Dev」代码签名证书并设为信任（每台机器一次，会弹窗要密码）
#   scripts/dev-sign-macos.sh [路径…]           重签 .app 或可执行文件；不给路径时签
#                                               target/debug/bundle/macos/Wisp.app 与 target/debug/wisp-desktop（存在的那些）
#   scripts/dev-sign-macos.sh --dev [参数…]     运行 `pnpm dev`，只为这一次运行把本脚本设成 cargo runner（绝对路径），
#                                               每次运行 wisp-desktop 之前自动签名；不影响其他终端与其他 Rust 项目
#   scripts/dev-sign-macos.sh --run <exe> [参数…]   cargo runner：<exe> 的文件名是 wisp-desktop 时先签名再 exec，
#                                               其他可执行文件（cargo test 的测试二进制等）直接 exec、不签名
#   scripts/dev-sign-macos.sh --check           检查证书是否可用、默认产物的签名状态
#   scripts/dev-sign-macos.sh --self-test       在临时钥匙串里验证「生成证书 → 导出 p12 → 导入 → 能作为代码签名身份找到」，
#                                               不碰登录钥匙串（CI 用）。不设置信任、不签名：设置信任需要有人在授权框里确认，
#                                               无人值守时做不到。无论成功、失败还是被中断，结束时都删除临时钥匙串、
#                                               恢复钥匙串搜索列表
# 环境变量：WISP_DEV_IDENTITY 覆盖证书名（默认 "Wisp Dev"）。
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
identity="${WISP_DEV_IDENTITY:-Wisp Dev}"
bundle_id="io.github.yzlltyyh.wisp"
openssl_bin=/usr/bin/openssl # 系统自带的 LibreSSL：导出的 p12 与 security import 兼容（OpenSSL 3 需要 -legacy）

die() {
  echo "dev-sign-macos: $*" >&2
  exit 1
}

[ "$(uname -s)" = Darwin ] || die "只能在 macOS 上运行"

# 生成自签代码签名证书并导出 p12：create_p12 <目录> <p12 口令>，输出 <目录>/cert.pem 与 <目录>/wisp-dev.p12
create_p12() {
  local dir="$1" pass="$2"
  cat > "$dir/openssl.cnf" << CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $identity
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
CNF
  "$openssl_bin" req -x509 -newkey rsa:3072 -sha256 -days 3650 -nodes \
    -config "$dir/openssl.cnf" -keyout "$dir/key.pem" -out "$dir/cert.pem" 2> /dev/null
  "$openssl_bin" pkcs12 -export -inkey "$dir/key.pem" -in "$dir/cert.pem" -name "$identity" \
    -out "$dir/wisp-dev.p12" -passout "pass:$pass" 2> /dev/null
  rm -f "$dir/key.pem"
}

# 是否有可用（已信任）的代码签名身份
# 自签证书默认不受信任（CSSMERR_TP_NOT_TRUSTED），codesign 会报 "no identity found"，必须设置「代码签名：始终信任」
has_identity() {
  security find-identity -v -p codesigning 2> /dev/null | grep -F "\"$identity\"" > /dev/null
}

# 证书（不论是否受信任）是否已在钥匙串里：has_cert [钥匙串]
has_cert() {
  security find-certificate -c "$identity" ${1:+"$1"} > /dev/null 2>&1
}

# 签名：sign_one <路径>
sign_one() {
  codesign --force --sign "$identity" --identifier "$bundle_id" --timestamp=none "$1"
}

# 签名结果中的 Authority 与 Identifier
describe() {
  codesign -dv --verbose=2 "$1" 2>&1 | grep -E '^(Identifier|Authority|Signature)=' | tr '\n' ' '
}

# 函数体在子 shell 里运行：任何一步失败（die / set -e）或被中断时，EXIT trap 都会删掉临时目录（里面有私钥）
init() (
  if has_identity; then
    echo "钥匙串中已有可用的代码签名身份「${identity}」，无需重复创建"
    return 0
  fi
  local tmp pass login="$HOME/Library/Keychains/login.keychain-db"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 130' INT TERM HUP
  if has_cert; then
    echo "钥匙串中已有证书「${identity}」，但还没有被信任用于代码签名"
    security find-certificate -c "$identity" -p > "$tmp/cert.pem"
  else
    pass="$(uuidgen)"
    create_p12 "$tmp" "$pass"
    # -T：codesign 使用私钥时不弹「允许访问」
    security import "$tmp/wisp-dev.p12" -k "$login" -P "$pass" -T /usr/bin/codesign -T /usr/bin/security > /dev/null
    echo "已在登录钥匙串中导入证书「${identity}」"
  fi
  echo "设置「代码签名：始终信任」（系统会弹窗要求输入登录密码）……"
  security add-trusted-cert -r trustRoot -p codeSign -k "$login" "$tmp/cert.pem"
  has_identity || die "设置信任后仍找不到可用的「${identity}」，请在「钥匙串访问」中检查该证书的信任设置"
  echo "✓ 代码签名身份「${identity}」可用。"
  echo "接下来：pnpm build:debug && scripts/dev-sign-macos.sh；开发时用 scripts/dev-sign-macos.sh --dev 代替 pnpm dev（每次运行前自动签名）。"
  echo "首次签名时如果弹出「codesign 想要使用钥匙串中的密钥」，输入登录密码并选择「始终允许」。"
)

sign_paths() {
  has_identity || die "钥匙串里没有可用的代码签名身份「${identity}」：先执行 scripts/dev-sign-macos.sh --init"
  local p
  for p in "$@"; do
    [ -e "$p" ] || die "$p 不存在"
    sign_one "$p"
    echo "已签名 ${p}：$(describe "$p")"
  done
}

check() {
  local status=0 p d
  if has_identity; then
    echo "✓ 代码签名身份「${identity}」可用"
  elif has_cert; then
    echo "✗ 证书「${identity}」存在但没有被信任用于代码签名（scripts/dev-sign-macos.sh --init 会补上信任设置）"
    status=1
  else
    echo "✗ 没有代码签名身份「${identity}」（scripts/dev-sign-macos.sh --init）"
    status=1
  fi
  for p in "$root/target/debug/bundle/macos/Wisp.app" "$root/target/debug/wisp-desktop"; do
    [ -e "$p" ] || continue
    d="$(describe "$p")"
    if grep -Fq "Authority=$identity" <<< "$d"; then
      echo "✓ ${p#"$root"/}：$d"
    else
      echo "! ${p#"$root"/} 不是用「${identity}」签的：$d"
      status=1
    fi
  done
  return $status
}

# 只验证证书的生成、导出与导入（--init 复用同一个 create_p12），不设置信任也不签名：设置信任要有人在授权框里确认，
# 无人值守的 CI 上做不到。函数体在子 shell 里运行，清理挂在 EXIT trap 上（bash 的 RETURN trap 不会在 exit 时触发）：
# 任何一步失败（die / set -e）或被中断，都会删除临时钥匙串、恢复钥匙串搜索列表
self_test() (
  local tmp kc kc_pw pass line
  local saved=()
  tmp="$(mktemp -d)"
  kc="$tmp/wisp-dev-selftest.keychain-db"
  # create-keychain 会把临时钥匙串加进搜索列表：先记下原列表（每行一个带引号的路径），结束时恢复
  while IFS= read -r line; do
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line#\"}"
    line="${line%\"}"
    if [ -n "$line" ]; then saved+=("$line"); fi
  done < <(security list-keychains -d user)
  # shellcheck disable=SC2329 # 由下面的 EXIT trap 调用
  cleanup_self_test() {
    security delete-keychain "$kc" > /dev/null 2>&1 || true
    if [ ${#saved[@]} -gt 0 ]; then security list-keychains -d user -s "${saved[@]}" > /dev/null 2>&1 || true; fi
    rm -rf "$tmp"
  }
  trap cleanup_self_test EXIT
  trap 'exit 130' INT TERM HUP
  kc_pw="$(uuidgen)"
  pass="$(uuidgen)"
  create_p12 "$tmp" "$pass"
  security create-keychain -p "$kc_pw" "$kc"
  security set-keychain-settings -lut 600 "$kc"
  security unlock-keychain -p "$kc_pw" "$kc"
  security import "$tmp/wisp-dev.p12" -k "$kc" -P "$pass" -T /usr/bin/codesign > /dev/null
  has_cert "$kc" || die "自检失败：临时钥匙串中找不到证书「${identity}」"
  # 不加 -v：证书没有被信任，只看私钥与证书能否配成代码签名身份
  security find-identity -p codesigning "$kc" | grep -F "\"$identity\"" > /dev/null ||
    die "自检失败：证书没有对应的私钥，或不是代码签名用途"
  echo "✓ dev-sign-macos.sh 自检通过：证书生成、p12 导出与导入正常（扩展用途 codeSigning）"
)

case "${1:-}" in
  --init) init ;;
  --check) check ;;
  --self-test) self_test ;;
  --run)
    shift
    [ $# -ge 1 ] || die "--run 需要可执行文件路径"
    # 只签 Wisp 本身：测试二进制、build script 等不需要固定身份，也不该被签成 io.github.yzlltyyh.wisp
    if [ "$(basename "$1")" = wisp-desktop ]; then
      if has_identity; then
        sign_one "$1" 2> /dev/null || echo "dev-sign-macos: 签名 $1 失败，按原签名运行" >&2
      else
        echo "dev-sign-macos: 没有可用的「${identity}」身份，按 ad-hoc 签名运行（scripts/dev-sign-macos.sh --init）" >&2
      fi
    fi
    exec "$@"
    ;;
  --dev)
    shift
    self="$root/scripts/dev-sign-macos.sh"
    # cargo 按空白切分 runner 字符串：路径里有空白就没法用
    case "$self" in *[[:space:]]*) die "仓库路径含空白（${root}），cargo runner 无法使用；请把仓库放到不含空格的路径" ;; esac
    has_identity || echo "dev-sign-macos: 还没有「${identity}」身份，本次按 ad-hoc 签名运行（先执行 scripts/dev-sign-macos.sh --init）" >&2
    cd "$root"
    # 只对这一次 pnpm dev 生效（子进程环境），Apple Silicon 与 Intel 两个目标都设上
    exec env CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER="$self --run" \
      CARGO_TARGET_X86_64_APPLE_DARWIN_RUNNER="$self --run" pnpm dev "$@"
    ;;
  -h | --help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" ;;
  -*) die "未知参数 $1（--help 查看用法）" ;;
  "")
    targets=()
    for p in "$root/target/debug/bundle/macos/Wisp.app" "$root/target/debug/wisp-desktop"; do
      if [ -e "$p" ]; then targets+=("$p"); fi
    done
    [ ${#targets[@]} -gt 0 ] || die "没有找到开发构建产物（先执行 pnpm build:debug 或 pnpm dev），也可以直接给出路径"
    sign_paths "${targets[@]}"
    ;;
  *) sign_paths "$@" ;;
esac
