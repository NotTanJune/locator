#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

case "$#" in
  0)
    search_root=$PWD
    ;;
  1)
    case "$1" in
      /*) search_root=$1 ;;
      *) search_root=$PWD/$1 ;;
    esac
    ;;
  *)
    echo "Usage: $0 [SEARCH_ROOT]" >&2
    exit 64
    ;;
esac

recording_dir=$repo_root/target/lctr-input-recordings/$(date +%Y%m%d-%H%M%S)-$$
raw_log=$recording_dir/terminal.raw
events_log=$recording_dir/events.jsonl
mkdir -p "$recording_dir"

echo "Privacy warning: terminal.raw and events.jsonl contain exact keystrokes/search text and terminal output. Inspect both files before sharing."

set +e
(
  cd "$repo_root" || exit 1
  cargo build --locked || exit $?
  script -q -r "$raw_log" env "LCTR_INPUT_TRACE=$events_log" \
    "$repo_root/target/debug/lctr" search "$search_root"
)
status=$?
set -e

printf 'Raw terminal log: %s\n' "$raw_log"
printf 'Decoded event log: %s\n' "$events_log"
printf 'Replay: script -d -p %s\n' "$raw_log"

exit "$status"
