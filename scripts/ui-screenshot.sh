#!/bin/sh
# Renders the Okno window on a private virtual X display and saves a PNG.
# Never touches the user's own display. Run inside the okno-dev container.
#
#   scripts/ui-screenshot.sh OUT.png [PAGE] [KIT] [LANG] [SCHEME]
#   PAGE: 0 devices, 1 this computer, 2 settings; KIT: adwaita|fluent;
#   LANG: ru|en; SCHEME: light|dark
set -eu
out=${1:?output png}
page=${2:-0}
kit=${3:-adwaita}
lang=${4:-ru}
scheme=${5:-light}
display=:$((90 + $$ % 9))
cfg=$(mktemp -d)
OKNO_UI_KIT=$kit cargo build -q -p okno-app
Xvfb "$display" -screen 0 1100x720x24 -nolisten tcp >/dev/null 2>&1 &
xvfb=$!
trap 'kill $app $xvfb 2>/dev/null; rm -rf "$cfg"' EXIT
sleep 1
env -u WAYLAND_DISPLAY DISPLAY="$display" OKNO_CONFIG_DIR="$cfg" OKNO_UI_PAGE="$page" OKNO_LANG="$lang" \
    OKNO_COLOR_SCHEME="$scheme" OKNO_NO_SOUND=1 SLINT_BACKEND=winit-software XDG_CONFIG_HOME="$cfg" \
    target/debug/okno >/dev/null 2>&1 &
app=$!
sleep 4
import -display "$display" -window root "$out"
echo "saved $out"
