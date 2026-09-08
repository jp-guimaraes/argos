#!/usr/bin/env bash
# Builds Argos.app from already-built release binaries, then a .dmg from
# that. No cargo-bundle: the bundle needs three binaries from three
# different crates side by side (argos, argos-gui, argos-helper), plus a
# lipo step (for the universal release build) and an hdiutil step, none of
# which a bundler does.
#
# Usage:
#   packaging/build-macos-app.sh [bin-dir]
#
# bin-dir defaults to target/release and, when left at that default, this
# script builds it first with a plain `cargo build --release --workspace`
# (the same shape as packaging/build-deb.sh). Pass an explicit directory --
# the release workflow's macos-app job does, pointing at a lipo'd universal
# set of binaries -- to skip that build and package whatever is already
# there.
#
# Produces target/macos/Argos.app and target/macos/Argos-<version>.dmg.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

bin_dir="${1:-target/release}"

# Checked on argument *count*, not on the resolved path: comparing the
# value would also match someone explicitly passing "target/release" and
# silently rebuild anyway, which defeats the whole point of the argument
# for the release workflow's universal-binary case (there is nothing to
# `cargo build` for a lipo'd "target" triple).
if [ "$#" -eq 0 ]; then
    echo "==> building the workspace"
    cargo build --release --workspace
fi

for bin in argos argos-gui argos-helper; do
    if [ ! -f "$bin_dir/$bin" ]; then
        echo "missing $bin_dir/$bin -- build the workspace (or the universal" >&2
        echo "binaries, for the release job) before running this script" >&2
        exit 1
    fi
done

version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

app="target/macos/Argos.app"
contents="$app/Contents"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"

echo "==> copying binaries into Contents/MacOS"
# All three binaries live side by side in Contents/MacOS, not the more
# conventional Contents/Resources or Contents/Helpers -- that placement is
# load-bearing, not a style choice. locate_helper_binary() (in
# argos-session, shared by the CLI and the GUI) looks for argos-helper as a
# sibling of current_exe(), and current_exe() resolves symlinks, so a
# double-clicked Argos.app's argos-gui finds its helper with no code change
# at all, the same way a plain `target/release/` checkout does. Moving the
# helper to Resources or Helpers, which sounds tidier, breaks that lookup.
cp "$bin_dir/argos" "$bin_dir/argos-gui" "$bin_dir/argos-helper" "$contents/MacOS/"
chmod +x "$contents/MacOS/argos" "$contents/MacOS/argos-gui" "$contents/MacOS/argos-helper"

echo "==> building the icon (sips + iconutil, both ship with Xcode Command Line Tools)"
# Generated from the committed 1024x1024 PNG rather than a committed .icns:
# this project is allergic to vendored binary assets (it wrote its own MBR
# boot code rather than link a GPL one), and an icon isn't worth being the
# exception when the only extra cost is ten sips calls.
iconset="$(mktemp -d)/argos.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" packaging/macos/icon-1024.png \
        --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" packaging/macos/icon-1024.png \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/argos.icns"

echo "==> writing Info.plist"
# CFBundleLocalizations = (en, pt-BR) is what makes AppleLanguages report
# pt-BR for this app specifically, which is how the GUI's language
# detection (G6, #93) tells the two apart from the system default.
#
# LSMinimumSystemVersion is read from the binary that will actually run,
# not guessed: `otool -l` on a release argos-gui reports minos 11.0 (the
# LC_BUILD_VERSION load command), which is what the Rust/Xcode toolchain
# already targets by default on this project's supported hosts.
cat >"$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Argos</string>
    <key>CFBundleDisplayName</key>
    <string>Argos</string>
    <key>CFBundleIdentifier</key>
    <string>org.argos.gui</string>
    <key>CFBundleExecutable</key>
    <string>argos-gui</string>
    <key>CFBundleIconFile</key>
    <string>argos</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version}</string>
    <key>CFBundleVersion</key>
    <string>${version}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleLocalizations</key>
    <array>
        <string>en</string>
        <string>pt-BR</string>
    </array>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright the Argos contributors. MIT OR Apache-2.0.</string>
</dict>
</plist>
PLIST

echo "==> Argos.app built at $app"

echo "==> building the .dmg"
# hdiutil ships with macOS -- zero dependencies, matching the project's
# general posture. A symlink to /Applications alongside Argos.app is the
# ordinary "drag to install" layout every macOS user already knows.
stage="$(mktemp -d)/dmg"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
dmg="target/macos/Argos-${version}.dmg"
rm -f "$dmg"
hdiutil create -volname Argos -srcfolder "$stage" -ov -format UDZO "$dmg" >/dev/null
echo "==> $dmg built"
