#!/usr/bin/env bash
# Package only this tag's build. Does not launch SciPort, Science, or any provider.
set -euo pipefail
: "${RUNNER_TEMP:?GitHub runner temporary directory is required}"
: "${RELEASE_TAG:?Release tag is required}"
: "${BUILD_SHA:?Source commit is required}"
[[ "${RELEASE_TAG}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${RELEASE_TAG#v}"
APP="$ROOT/desktop/src-tauri/target/aarch64-apple-darwin/release/bundle/macos/SciPort.app"
OUTPUT="$RUNNER_TEMP/sciport-release"
STAGING="$(mktemp -d "$RUNNER_TEMP/sciport-stage.XXXXXX")"
MOUNT="$(mktemp -d "$RUNNER_TEMP/sciport-mount.XXXXXX")"
MOUNTED=false
cleanup() {
  if [[ "$MOUNTED" == true ]]; then hdiutil detach "$MOUNT" || true; fi
  rm -rf "$STAGING"
  rmdir "$MOUNT" 2>/dev/null || true
}
trap cleanup EXIT
mkdir "$OUTPUT"
[[ -d "$APP" ]]
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")" == "$VERSION" ]]
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist")" == com.csswitch.menubar ]]
for executable in desktop csswitch-gateway; do
  [[ "$(lipo -archs "$APP/Contents/MacOS/$executable")" == arm64 ]]
done
codesign --force --sign - "$APP/Contents/MacOS/csswitch-gateway"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"
ditto "$APP" "$STAGING/SciPort.app"
ln -s /Applications "$STAGING/Applications"
DMG="SciPort_${VERSION}_aarch64.dmg"
hdiutil create -volname "SciPort $VERSION" -srcfolder "$STAGING" -ov -format UDZO "$OUTPUT/$DMG"
hdiutil verify "$OUTPUT/$DMG"
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT" "$OUTPUT/$DMG"
MOUNTED=true
# Packaging checks only: exact payload, architecture, signing, and byte identity.
python3 - "$MOUNT" "$APP" <<'PY'
import hashlib, os, pathlib, sys
mount, app = map(pathlib.Path, sys.argv[1:])
entries = {p.name for p in mount.iterdir()} - {'.DS_Store', '.VolumeIcon.icns', '.fseventsd', '.Trashes'}
assert entries == {'SciPort.app', 'Applications'}, entries
assert (mount / 'Applications').is_symlink()
assert os.readlink(mount / 'Applications') == '/Applications'
assert len(list(mount.glob('*.app'))) == 1
for name in ('desktop', 'csswitch-gateway'):
    relative = pathlib.Path('Contents/MacOS') / name
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    assert digest(app / relative) == digest(mount / 'SciPort.app' / relative)
PY
codesign --verify --deep --strict "$MOUNT/SciPort.app"
hdiutil detach "$MOUNT"
MOUNTED=false
(
  cd "$OUTPUT"
  shasum -a 256 "$DMG" > SHA256SUMS
)
{
  printf 'Product: SciPort\nVersion: %s\nSource: %s\nArchitecture: arm64\n' "$VERSION" "$BUILD_SHA"
  printf 'Built by GitHub Actions after frontend and release-notes checks; full source gate and live-provider tests not run.\n'
  printf 'Signing: ad-hoc; not Developer ID signed or Apple notarized.\n'
  rustc --version
  cargo --version
  node --version
  sw_vers
  for executable in desktop csswitch-gateway; do
    printf '%s SHA-256: ' "$executable"
    shasum -a 256 "$APP/Contents/MacOS/$executable" | awk '{print $1}'
  done
} > "$OUTPUT/BUILD_INFO.txt"
cat > "$OUTPUT/RELEASE_NOTES.md" <<EOF
## 中文

研舟 · SciPort ${RELEASE_TAG}：新增启动前目录访问选择，以及从 GitHub 检查、下载并验证签名的 SciPort 应用更新。

- 下载 **$DMG**，打开后将 **SciPort.app** 拖入“应用程序”。仅适用于 Apple Silicon（M 系列）Mac。
- 请先退出旧版 CSSwitch。应用标识和配置目录保留兼容，不自动删除原有数据。
- 需要自行安装官方 Claude Science；不随包提供 Science。
- **本次由 GitHub 构建发布，构建前运行前端与发布说明回归检查；完整源码门禁、真实 Science 和供应商调用未验证。构建成功不代表功能全部通过。**
- 先前本地候选的完整测试未通过；本次不宣称完整源码门禁通过。
- claude.ai 会话过期 / Directory connectors unavailable **未修复**，API 连通不等于官方账号授权。
- 启动前可选择本机主目录、仅本次隔离或取消；本机主目录访问默认关闭，须知情同意。HOME 外目录仍需 Science granted-root 授权。
- v0.9.1 及更早版本须手动安装本版一次。此后启动时及每 6 小时检查更新，用户确认后才下载验签、停止服务、安装并重启；请先保存工作。仅更新 SciPort，不更新 Science。
- 安装包为 ad-hoc 签名，未经 Apple Developer ID 签名或公证。请核对 SHA256SUMS，不要关闭系统安全保护。

[中文说明](https://github.com/Vonfre/claude-science-api/blob/${RELEASE_TAG}/README.md) · [English README](https://github.com/Vonfre/claude-science-api/blob/${RELEASE_TAG}/README.en.md)

## English

SciPort ${RELEASE_TAG} adds a pre-launch directory-access choice and signed SciPort application updates from GitHub.

- Download **$DMG** and drag **SciPort.app** into Applications. Apple Silicon Macs only.
- Quit CSSwitch before installing. The application identifier and configuration directory remain compatible; existing data is not automatically deleted.
- Install official Claude Science separately; it is not bundled.
- **GitHub runs frontend and release-notes regression checks before building. The full source gate, real Science runtime, and live providers remain unverified. A successful build is not proof that all functionality works.**
- The earlier local candidate did not pass the complete test gate; this release does not claim full source-gate closure.
- Expired claude.ai sessions / unavailable directory connectors are **not fixed**. API connectivity does not restore official account authorization.
- Choose real-home access, isolation for this launch, or cancel before launch. Real-home access remains off by default and requires informed consent. Paths outside HOME still require Science granted-root authorization.
- Install this version manually once when upgrading from v0.9.1 or earlier. Later versions are checked at startup and every six hours; confirmation is required before download, signature verification, service shutdown, installation, and restart. Save your work first. Only SciPort is updated, not Science.
- Ad-hoc signed, not Apple Developer ID signed or notarized. Check SHA256SUMS and do not disable system-wide security protections.

Source commit: \`$BUILD_SHA\`. Build environment and executable hashes: \`BUILD_INFO.txt\`.
EOF
printf 'Installer ready: %s\n' "$OUTPUT/$DMG"
