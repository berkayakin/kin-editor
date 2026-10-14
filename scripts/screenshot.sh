#!/usr/bin/env bash
# Takes README screenshots of Kin (macOS): the window at Retina resolution, as a PNG with
# its rounded corners and drop shadow on a transparent background.
#
#   scripts/screenshot.sh [options] [<folder> [<file>…]]
#
# Launches target/release/kin with its own config (target/screenshot-config, so your
# ~/.config/kin isn't shown or changed, and the theme and panels you set up are kept for the
# next run), sizes the window, then asks for a name per shot. Arrange the window, type a name
# and press Enter: Kin comes to the front and is captured after the delay. Saves
# docs/screenshots/<name>.png. An empty name quits (and closes the Kin it launched).
#
# Options:
#   -s WxH    window size in points (default 1400x875)
#   -d SEC    delay before each capture, e.g. to hover something (default 2)
#   -o DIR    output folder (default docs/screenshots)
#   -p PID    use an already running Kin instead of launching one
#   -n        no drop shadow
#   -f        start from a fresh screenshot config
#
# The terminal needs Screen Recording access (for screencapture) and Accessibility access
# (to size the window). oxipng, if installed, shrinks the PNGs.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
size=1400x875
delay=2
out="$root/docs/screenshots"
pid=""
shadow=()
fresh=0

while getopts "s:d:o:p:nfh" opt; do
    case $opt in
        s) size=$OPTARG ;;
        d) delay=$OPTARG ;;
        o) out=$OPTARG ;;
        p) pid=$OPTARG ;;
        n) shadow=(-o) ;;
        f) fresh=1 ;;
        *) sed -n '2,24s/^# \{0,1\}//p' "$0"; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[[ $size =~ ^([0-9]+)x([0-9]+)$ ]] || { echo "size must look like 1400x875" >&2; exit 1; }
width=${BASH_REMATCH[1]}
height=${BASH_REMATCH[2]}

# Build the helper once, and again whenever its source changes.
helper="$root/target/screenshot-helper"
if [[ ! -x $helper || $root/scripts/screenshot.swift -nt $helper ]]; then
    echo "Building the screenshot helper…"
    mkdir -p "$root/target"
    swiftc -O "$root/scripts/screenshot.swift" -o "$helper"
fi

launched=""
if [[ -z $pid ]]; then
    kin="$root/target/release/kin"
    [[ -x $kin ]] || { echo "No $kin; run cargo build --release first" >&2; exit 1; }
    config="$root/target/screenshot-config"
    [[ $fresh == 1 ]] && rm -rf "$config"
    mkdir -p "$config"
    (( $# > 0 )) || set -- "$root"
    XDG_CONFIG_HOME="$config" "$kin" "$@" </dev/null >/dev/null 2>&1 &
    pid=$!
    launched=$pid
fi

# Only the Kin this script started, by PID: other Kin windows may be open.
trap '[[ -n $launched ]] && kill "$launched" 2>/dev/null || true' EXIT

window=""
for _ in $(seq 100); do
    window=$("$helper" window "$pid" 2>/dev/null) && break
    window=""
    sleep 0.2
done
[[ -n $window ]] || { echo "Kin's window didn't appear (pid $pid)" >&2; exit 1; }

"$helper" resize "$pid" "$width" "$height" || echo "Keeping the current window size."
"$helper" activate "$pid"
mkdir -p "$out"

echo "Arrange the Kin window, then type a name for the shot here and press Enter (empty to quit)."
while true; do
    read -r -p "Shot name: " name || break
    [[ -n $name ]] || break
    name=${name%.png}
    file="$out/$name.png"
    # Find the window again: it changes if the project is reopened.
    window=$("$helper" window "$pid") || { echo "Kin's window is gone" >&2; exit 1; }
    "$helper" activate "$pid"
    sleep "$delay"
    if ! screencapture -x ${shadow[@]+"${shadow[@]}"} -l "$window" -t png "$file"; then
        echo "Couldn't capture the window. The app running this terminal needs Screen Recording access:" >&2
        echo "System Settings → Privacy & Security → Screen & System Audio Recording (then restart that app)." >&2
        continue
    fi
    command -v oxipng >/dev/null && oxipng -q -o 3 --strip safe "$file"
    echo "Saved ${file#"$root"/} ($(sips -g pixelWidth -g pixelHeight "$file" | awk '/pixel/ {printf "%s%s", sep, $2; sep="×"}'))"
done
