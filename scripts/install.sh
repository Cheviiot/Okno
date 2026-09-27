#!/bin/sh
# Installs Okno from a release build: binaries, desktop entry, AppStream
# data and icons.
#   scripts/install.sh [--prefix /usr] [--destdir DIR] [--target-dir target/release]
set -eu
prefix=/usr/local
destdir=
bindir=target/release
while [ $# -gt 0 ]; do
    case $1 in
        --prefix) prefix=$2; shift 2 ;;
        --destdir) destdir=$2; shift 2 ;;
        --target-dir) bindir=$2; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
root="$destdir$prefix"
id=io.github.cheviiot.okno
install -Dm755 "$bindir/okno" "$root/bin/okno"
install -Dm755 "$bindir/okno-cli" "$root/bin/okno-cli"
install -Dm644 "data/$id.desktop" "$root/share/applications/$id.desktop"
install -Dm644 "data/$id.metainfo.xml" "$root/share/metainfo/$id.metainfo.xml"
install -Dm644 "data/icons/hicolor/scalable/apps/$id.svg" "$root/share/icons/hicolor/scalable/apps/$id.svg"
install -Dm644 "data/icons/hicolor/symbolic/apps/$id-symbolic.svg" "$root/share/icons/hicolor/symbolic/apps/$id-symbolic.svg"
install -Dm644 COPYING "$root/share/licenses/okno/COPYING"
echo "installed into $root"
