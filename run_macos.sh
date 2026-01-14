#!/bin/bash
set -euo pipefail

cargo tauri build "$@"

path="$PWD/src-tauri/target/release/bundle/macos/tidewave.app"
trap 'pkill -f "$path/Contents/MacOS/tidewave"' INT
tty=$(tty)
open -W --stdout $tty --stderr $tty "$path"
