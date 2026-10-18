#!/usr/bin/env bash
# 用固定的自签证书重签 macOS 产物（release.yml 的 build (macos) 在 tauri 打包之后调用）。
#
# 为什么不让 tauri 直接签：codesign 只肯用系统信任的身份签名（自签证书不受信任时报 "no identity found"），而 GitHub 的
# macos-15 runner 不允许无人值守地把证书设为信任（改管理员域的信任设置要在授权框里确认，临时放开授权规则也被拒绝）。
# rcodesign 直接读取证书与私钥签名，不经过钥匙串，也不看信任设置。
#
# 流程：tauri 先按 ad-hoc 签名打出 Wisp.app、dmg 与更新包；这里用 rcodesign 重签 Wisp.app（hardened runtime、
# Entitlements.plist、显式的指定要求），再用重签后的 .app 重新打更新包 Wisp.app.tar.gz、重新生成它的更新器签名，并替换 dmg
# 里的 Wisp.app。每一步之后都用系统的 codesign 核对。
#
# 指定要求（designated requirement）显式写成「bundle id + 证书 SHA-1」，与 codesign 对自签证书生成的默认写法一致：系统的权限
# 授权（辅助功能、屏幕录制）按它识别应用，每一版都必须完全一样，授权才能在更新后保持。
#
# 用法：resign-macos.sh <bundle 目录> <版本号>
#   环境变量：WISP_SIGN_CERT / WISP_SIGN_KEY（PEM 格式的证书与私钥）、RCODESIGN（rcodesign 可执行文件）；
#   原来有更新器签名（.sig）时还需要 TAURI_SIGNING_PRIVATE_KEY 与 TAURI_SIGNING_PRIVATE_KEY_PASSWORD。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENTITLEMENTS="$ROOT/apps/desktop/src-tauri/Entitlements.plist"

die() {
  echo "::error::resign-macos: $*" >&2
  exit 1
}

[ $# -eq 2 ] || die "用法：resign-macos.sh <bundle 目录> <版本号>"
[ -d "$1" ] || die "找不到 bundle 目录 $1"
bundle="$(cd "$1" && pwd)"
version="$2"
: "${WISP_SIGN_CERT:?需要 WISP_SIGN_CERT}" "${WISP_SIGN_KEY:?需要 WISP_SIGN_KEY}" "${RCODESIGN:?需要 RCODESIGN}"
app="$bundle/macos/Wisp.app"
[ -d "$app" ] || die "找不到 $app"

work="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/wisp-resign.XXXXXX")"
mnt=""
cleanup() {
  if [ -n "$mnt" ]; then hdiutil detach "$mnt" -quiet -force 2> /dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")"
executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Contents/Info.plist")"
sha1="$(/usr/bin/openssl x509 -in "$WISP_SIGN_CERT" -noout -fingerprint -sha1 | sed 's/^.*=//; s/://g')"
[ "${#sha1}" -eq 40 ] || die "读不出证书的 SHA-1"
requirement="designated => identifier \"$identifier\" and certificate root = H\"$sha1\""
csreq -r "=$requirement" -b "$work/requirement.bin"

# 核对一个 .app：签名有效、不是 ad-hoc、指定要求与上面一致、开着 hardened runtime、带着 Entitlements.plist 里的权限
verify_app() {
  local a="$1" info ent
  codesign --verify --strict --deep "$a" || die "$a 的签名校验失败"
  info="$(codesign -dvv "$a" 2>&1)"
  grep -q '^Authority=' <<< "$info" || die "$a 没有证书签名（仍是 ad-hoc？）"
  grep -Eq '^CodeDirectory .*flags=0x[0-9a-f]*\([^)]*runtime' <<< "$info" || die "$a 没有开 hardened runtime"
  codesign -d -r- "$a" 2>&1 | grep -qiF "$requirement" || die "$a 的指定要求不是 $requirement"
  ent="$(codesign -d --entitlements - --xml "$a" 2> /dev/null)"
  grep -q 'com.apple.security.cs.disable-library-validation' <<< "$ent" || die "$a 缺少 Entitlements.plist 里的权限"
}

# ① 重签 .app（主程序与包内嵌套的代码一并重签；时间戳对自签证书没有意义，关掉）
"$RCODESIGN" sign \
  --pem-file "$WISP_SIGN_CERT" --pem-file "$WISP_SIGN_KEY" \
  --code-signature-flags runtime \
  --entitlements-xml-file "$ENTITLEMENTS" \
  --code-requirements-file "$work/requirement.bin" \
  --timestamp-url none \
  "$app"
verify_app "$app"
echo "✓ Wisp.app 已用证书 $sha1 重签"

# ② 更新包：客户端解压时去掉第一级目录，所以条目必须以 Wisp.app/ 开头（不能带 ./）；不带 AppleDouble 与扩展属性
tarball="$bundle/macos/Wisp.app.tar.gz"
if [ -f "$tarball" ]; then
  had_sig=0
  [ -f "$tarball.sig" ] && had_sig=1
  rm -f "$tarball" "$tarball.sig"
  COPYFILE_DISABLE=1 tar --no-mac-metadata -czf "$tarball" -C "$bundle/macos" Wisp.app
  if tar -tzf "$tarball" | grep -qv '^Wisp\.app/'; then die "更新包里有不在 Wisp.app/ 下的条目"; fi
  if [ "$had_sig" = 1 ]; then
    [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ] || die "更新包原来有签名，但没有私钥重新签"
    (cd "$ROOT/apps/desktop" && pnpm exec tauri signer sign --app-version "$version" "$tarball") > /dev/null
    [ -s "$tarball.sig" ] || die "没有生成 $tarball.sig"
  fi
  echo "✓ 更新包已用重签后的 Wisp.app 重新打包$([ "$had_sig" = 1 ] && echo '并签名')"
fi

# ③ dmg：转成可写镜像、留出余量（证书签名比 ad-hoc 签名大几 KB），只替换 Wisp.app，背景与图标位置都保留
shopt -s nullglob
dmgs=("$bundle"/dmg/*.dmg)
[ "${#dmgs[@]}" -eq 1 ] || die "dmg 目录里应当恰好有一个 dmg（实际 ${#dmgs[@]} 个）"
dmg="${dmgs[0]}"
hdiutil convert "$dmg" -quiet -format UDRW -o "$work/rw.dmg"
sectors="$(hdiutil resize -limits "$work/rw.dmg" | awk 'NR == 1 { print $2 }')"
hdiutil resize -quiet -sectors "$((sectors + 40960))" "$work/rw.dmg"
mnt="$work/mnt"
mkdir "$mnt"
hdiutil attach "$work/rw.dmg" -quiet -nobrowse -noautoopen -mountpoint "$mnt"
[ -d "$mnt/Wisp.app" ] || die "dmg 里没有 Wisp.app"
rm -rf "$mnt/Wisp.app"
ditto "$app" "$mnt/Wisp.app"
hdiutil detach "$mnt" -quiet
rm -f "$dmg"
hdiutil convert "$work/rw.dmg" -quiet -format UDZO -imagekey zlib-level=9 -o "$dmg"
hdiutil attach "$dmg" -quiet -readonly -nobrowse -noautoopen -mountpoint "$mnt"
verify_app "$mnt/Wisp.app"
cmp -s "$app/Contents/MacOS/$executable" "$mnt/Wisp.app/Contents/MacOS/$executable" || die "dmg 里的主程序与重签后的不一致"
hdiutil detach "$mnt" -quiet
mnt=""
echo "✓ $(basename "$dmg") 里的 Wisp.app 已替换"
