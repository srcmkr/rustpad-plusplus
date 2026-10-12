#!/usr/bin/env bash
# Packs the release build into what a release offers: an archive, a Debian
# package and an AppImage, in dist/. Run after `cargo build --release`; the
# release workflow does exactly this, then signs the files.
#
# Usage: packaging/build-packages.sh VERSION
set -euo pipefail
umask 022
cd "$(dirname "$0")/.."

version=${1:?usage: packaging/build-packages.sh VERSION}
arch=$(uname -m)
case $arch in
    x86_64) deb_arch=amd64 ;;
    aarch64) deb_arch=arm64 ;;
    *) echo "unsupported architecture $arch" >&2; exit 1 ;;
esac
bin=target/release/rustpad
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 1; }

rm -rf dist
mkdir -p dist
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Puts the icons into the hicolor theme under $1.
icons() {
    for png in assets/logo/png/rustpad-*.png; do
        size=$(basename "$png" .png); size=${size#rustpad-}
        case $size in *[!0-9]*) continue ;; esac
        install -Dm 644 "$png" "$1/${size}x${size}/apps/rustpad.png"
    done
    install -Dm 644 assets/logo/rustpad.svg "$1/scalable/apps/rustpad.svg"
}

# --- Archive: the program, for "Install for all users" and updates.
name=rustpad-$version-$arch-linux
mkdir "$work/$name"
cp "$bin" README.md LICENSE-MIT LICENSE-APACHE "$work/$name/"
strip "$work/$name/rustpad"
tar -C "$work" -czf "dist/$name.tar.gz" "$name"

# --- Debian package.
deb=$work/deb
app=/usr/bin/rustpad
mkdir -p "$deb/DEBIAN" "$deb/usr/bin" "$deb/usr/share/applications" "$deb/usr/share/polkit-1/actions" \
    "$deb/usr/share/doc/rustpad"
install -m 755 "$work/$name/rustpad" "$deb$app"
"$deb$app" --desktop-entry "$app" > "$deb/usr/share/applications/rustpad.desktop"
sed "s|@APP@|$app|g" packaging/io.github.srcmkr.rustpad.policy > "$deb/usr/share/polkit-1/actions/io.github.srcmkr.rustpad.policy"
icons "$deb/usr/share/icons/hicolor"
cat LICENSE-MIT LICENSE-APACHE > "$deb/usr/share/doc/rustpad/copyright"
chmod 644 "$deb"/usr/share/applications/* "$deb"/usr/share/polkit-1/actions/* "$deb/usr/share/doc/rustpad/copyright"

cat > "$deb/DEBIAN/control" <<CONTROL
Package: rustpad
Version: $version
Section: editors
Priority: optional
Architecture: $deb_arch
Maintainer: srcmkr <srcmkr@users.noreply.github.com>
Homepage: https://github.com/srcmkr/rustpad-plusplus
Depends: libc6 (>= 2.35), libgcc-s1, libxkbcommon0, libxkbcommon-x11-0, libxcb1, libfontconfig1, libwayland-client0, libvulkan1
Recommends: pkexec | policykit-1, xdg-utils
Installed-Size: $(du -sk "$deb" | cut -f1)
Description: Fast text editor for files of any size, in the spirit of Notepad++
 rustpad++ opens files of any size instantly - as text, hex, JSON tree or
 rendered Markdown - and keeps every tab, unsaved changes included, across
 restarts.
CONTROL
dpkg-deb --root-owner-group -Zxz --build "$deb" "dist/rustpad_${version}_${deb_arch}.deb" >/dev/null

# --- AppImage: one file to download and run.
appdir=$work/Rustpad.AppDir
mkdir -p "$appdir/usr/bin"
install -m 755 "$work/$name/rustpad" "$appdir/usr/bin/"
cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/rustpad" "$@"
APPRUN
chmod 755 "$appdir/AppRun"
"$appdir/usr/bin/rustpad" --desktop-entry rustpad > "$appdir/rustpad.desktop"
cp assets/logo/rustpad.svg "$appdir/rustpad.svg"
ln -s rustpad.svg "$appdir/.DirIcon"
icons "$appdir/usr/share/icons/hicolor"

tool=${APPIMAGETOOL:-$work/appimagetool}
if [ ! -x "$tool" ]; then
    curl -sSfL -o "$tool" "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage"
    chmod 755 "$tool"
fi
ARCH=$arch "$tool" --appimage-extract-and-run --no-appstream "$appdir" "dist/Rustpad-$version-$arch.AppImage" >/dev/null 2>&1

# The same under names without the version, so links to the latest release
# (README) always work.
cp "dist/rustpad_${version}_${deb_arch}.deb" "dist/rustpad_${deb_arch}.deb"
cp "dist/Rustpad-$version-$arch.AppImage" "dist/Rustpad-$arch.AppImage"

( cd dist && sha256sum -- * > SHA256SUMS )
ls -l dist
