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
#   scripts/dev-sign-macos.sh --self-test       在临时钥匙串里走一遍「建证书 → 信任 → 签名 → 校验」，不碰登录钥匙串（CI 用；
#                                               信任与签名部分需要免密 sudo、且能临时放开授权规则 com.apple.trust-settings.admin，
#                                               做不到时（本机没有免密 sudo；macOS 15 的 runner）只验证证书生成与导入，见 admin_trust）。
#                                               无论成功、失败还是被中断，结束时都删除临时钥匙串、恢复钥匙串搜索列表、
#                                               撤销临时信任
#   scripts/dev-sign-macos.sh --trust-admin <cert.pem> [钥匙串]
#                                               在管理员域把证书设为「代码签名：始终信任」（需要免密 sudo；release.yml
#                                               导入自签 p12 后用它）。授权规则放不开时以退出码 3 失败、不会卡住（见 admin_trust）
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

# 是否有可用（已信任）的代码签名身份：has_identity [钥匙串]
# 自签证书默认不受信任（CSSMERR_TP_NOT_TRUSTED），codesign 会报 "no identity found"，必须设置「代码签名：始终信任」
has_identity() {
  security find-identity -v -p codesigning ${1:+"$1"} 2> /dev/null | grep -F "\"$identity\"" > /dev/null
}

# 证书（不论是否受信任）是否已在钥匙串里：has_cert [钥匙串]
has_cert() {
  security find-certificate -c "$identity" ${1:+"$1"} > /dev/null 2>&1
}

# 签名：sign_one <路径> [钥匙串]
sign_one() {
  local path="$1" keychain="${2:-}"
  local args=(--force --sign "$identity" --identifier "$bundle_id" --timestamp=none)
  [ -n "$keychain" ] && args+=(--keychain "$keychain")
  codesign "${args[@]}" "$path"
}

# 签名结果中的 Authority 与 Identifier
describe() {
  codesign -dv --verbose=2 "$1" 2>&1 | grep -E '^(Identifier|Authority|Signature)=' | tr '\n' ' '
}

# 有上限地运行一条命令：bounded <秒> <命令…>，超时先 TERM、1 秒后 KILL，返回 124。授权服务需要交互确认时 security 会一直
# 等一个没人看得见的授权框（GitHub 的 macOS runner 有图形会话：曾在这里卡住 27 分钟，直到手动取消），
# 所以凡是可能等交互的特权调用都经它运行。标准输入原样交给命令（`<&0`：后台命令默认读 /dev/null）
bounded() {
  local secs="$1" pid ticks=0
  shift
  "$@" <&0 &
  pid=$!
  while kill -0 "$pid" 2> /dev/null; do
    if [ "$ticks" -ge $((secs * 10)) ]; then
      kill -TERM "$pid" 2> /dev/null || true
      sleep 1
      kill -KILL "$pid" 2> /dev/null || true
      wait "$pid" 2> /dev/null || true
      echo "dev-sign-macos: ${secs} s 内没有结束，已中止：$*（多半是在等一个看不见的授权框）" >&2
      return 124
    fi
    sleep 0.1
    ticks=$((ticks + 1))
  done
  wait "$pid"
}

# 在管理员域设置 / 撤销证书的「代码签名：始终信任」：admin_trust add|remove <cert.pem> [钥匙串]（需要免密 sudo）。
# macOS 11 起，即使用 sudo，改管理员域的信任设置也要管理员在授权框里确认：没有图形会话时被拒绝（SecTrustSettingsSetTrustSettings:
# The authorization was denied since no user interaction was possible），有图形会话但没人操作时（GitHub 的 macOS runner）则一直
# 等下去。所以先把授权规则 com.apple.trust-settings.admin 临时改为 allow，执行完立即写回原规则。改不了规则时（读不到原规则，
# 或 `authorizationdb write` 没有回 YES——macOS 15 起 runner 上返回 NO (-60005)）**不再尝试**，以退出码 3 结束：这台机器上没法
# 无人值守地设置信任。特权调用都有时限（bounded）。函数体在子 shell 里运行：中途失败或被中断时 EXIT trap 同样会写回原规则；
# 调用方可能在 `||` 里调用它（那里 set -e 不生效），所以每一步都显式判断、显式 exit
admin_trust() (
  local action="$1" cert="$2" keychain="${3:-}" right=com.apple.trust-settings.admin saved relaxed=0 out=""
  local args=()
  saved="$(mktemp "${TMPDIR:-/tmp}/wisp-authz.XXXXXX")" || exit 1
  # shellcheck disable=SC2329 # 由下面的 EXIT trap 调用
  restore_right() {
    # 原规则文件属于当前用户，由当前 shell 读出后经标准输入交给 sudo（不需要 root 读文件）
    # shellcheck disable=SC2024
    if [ "$relaxed" = 1 ] && ! bounded 30 sudo -n security authorizationdb write "$right" < "$saved" > /dev/null 2>&1; then
      echo "dev-sign-macos: 没能恢复授权规则 ${right}；原规则保存在 ${saved}，请执行 sudo security authorizationdb write ${right} < ${saved}" >&2
      return 0
    fi
    rm -f "$saved"
  }
  trap restore_right EXIT
  trap 'exit 130' INT TERM HUP
  if security authorizationdb read "$right" > "$saved" 2> /dev/null && [ -s "$saved" ]; then
    out="$(bounded 30 sudo -n security authorizationdb write "$right" allow 2>&1)" || true
    case "$out" in *YES*) relaxed=1 ;; esac
  else
    out="读不到原规则"
  fi
  if [ "$relaxed" != 1 ]; then
    echo "dev-sign-macos: 没能临时放开授权规则 ${right}（${out:-没有输出}）：这台机器上改管理员域的信任设置需要有人在授权框里确认，不再尝试" >&2
    exit 3
  fi
  case "$action" in
    add)
      args=(add-trusted-cert -d -r trustRoot -p codeSign)
      if [ -n "$keychain" ]; then args+=(-k "$keychain"); fi
      bounded 60 sudo -n security "${args[@]}" "$cert" || exit $?
      ;;
    remove) bounded 60 sudo -n security remove-trusted-cert -d "$cert" || exit $? ;;
    *) die "admin_trust：未知操作 $action" ;;
  esac
)

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

# 函数体在子 shell 里运行，清理挂在 EXIT trap 上（bash 的 RETURN trap 不会在 exit 时触发）：任何一步失败（die /
# set -e）或被中断，都会删除临时钥匙串、恢复钥匙串搜索列表、撤销管理员域里的临时信任
self_test() (
  local tmp kc kc_pw pass trusted=0 line
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
    if [ "$trusted" = 1 ]; then admin_trust remove "$tmp/cert.pem" > /dev/null 2>&1 || true; fi
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
  security set-key-partition-list -S apple-tool:,apple: -s -k "$kc_pw" "$kc" > /dev/null
  has_cert "$kc" || die "自检失败：临时钥匙串中找不到证书「${identity}」"
  security find-identity -p codesigning "$kc" | grep -F "\"$identity\"" > /dev/null ||
    die "自检失败：证书没有对应的私钥，或不是代码签名用途"
  echo "✓ 证书生成、p12 导出与导入正常（扩展用途 codeSigning）"

  # 不弹窗地设置信任只能用管理员域（sudo）；本机没有免密 sudo 时跳过签名部分（--init 走用户域、会弹窗）
  if ! sudo -n true 2> /dev/null; then
    echo "! 没有免密 sudo：跳过「信任 → 签名 → 校验」部分"
    return 0
  fi
  # 先记下再设置：设置到一半失败时也要撤销
  trusted=1
  local rc=0
  echo "· 在管理员域设置信任……"
  admin_trust add "$tmp/cert.pem" "$kc" || rc=$?
  if [ "$rc" = 3 ]; then
    # 规则没放开、什么都没改：不用撤销（撤销同样要等授权框）
    trusted=0
    echo "! 这台机器不允许无人值守地改管理员域的信任设置（见上一行）：跳过「信任 → 签名 → 校验」部分。release.yml 的"
    echo "  --trust-admin（配置自签 p12 之后才用）在同样的 runner 上会以退出码 3 失败"
    return 0
  fi
  [ "$rc" = 0 ] || die "自检失败：管理员域的信任设置失败（退出码 ${rc}）"
  has_identity "$kc" || die "自检失败：设置信任后「${identity}」仍不可用"
  echo "· 信任已设置，签名并校验……"

  # 签一个可执行文件和一个最小的 .app
  cp /bin/echo "$tmp/wisp-desktop"
  mkdir -p "$tmp/Wisp.app/Contents/MacOS"
  cp /bin/echo "$tmp/Wisp.app/Contents/MacOS/wisp-desktop"
  cat > "$tmp/Wisp.app/Contents/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>wisp-desktop</string>
<key>CFBundleIdentifier</key><string>$bundle_id</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
PLIST
  local p first second
  for p in "$tmp/wisp-desktop" "$tmp/Wisp.app"; do
    sign_one "$p" "$kc"
    codesign --verify "$p"
    grep -Fq "Authority=$identity" <<< "$(describe "$p")" || die "自检失败：$p 的签名不是「${identity}」：$(describe "$p")"
    grep -Fq "Identifier=$bundle_id" <<< "$(describe "$p")" || die "自检失败：$p 的 identifier 不是 $bundle_id"
  done
  # 关键性质：重新编译（内容变化）后 designated requirement 不变
  first="$(codesign -d -r- "$tmp/wisp-desktop" 2>&1 | grep '^designated')"
  printf '\n' >> "$tmp/wisp-desktop"
  sign_one "$tmp/wisp-desktop" "$kc"
  second="$(codesign -d -r- "$tmp/wisp-desktop" 2>&1 | grep '^designated')"
  [ -n "$first" ] && [ "$first" = "$second" ] || die "自检失败：内容变化后 designated requirement 变了：$first / $second"
  grep -q 'certificate leaf' <<< "$first" || die "自检失败：designated requirement 没有绑定证书：$first"
  echo "✓ dev-sign-macos.sh 自检通过（${first}）"
)

case "${1:-}" in
  --init) init ;;
  --check) check ;;
  --self-test) self_test ;;
  --trust-admin)
    [ $# -ge 2 ] || die "--trust-admin 需要证书（PEM）路径"
    [ -f "$2" ] || die "$2 不存在"
    sudo -n true 2> /dev/null || die "--trust-admin 需要免密 sudo"
    admin_trust add "$2" "${3:-}"
    echo "已在管理员域把 $2 设为「代码签名：始终信任」"
    ;;
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
