#!/bin/sh
set -eu

TAP="NotTanJune/locator"
URL="https://github.com/NotTanJune/locator"

if ! command -v brew >/dev/null 2>&1; then
  echo "Homebrew is required: https://brew.sh" >&2
  exit 1
fi

brew tap "$TAP" "$URL"
brew install lctr
lctr --version

if [ -r /dev/tty ]; then
  setup_status=0
  lctr setup-shell < /dev/tty || setup_status=$?
else
  setup_status=0
  lctr setup-shell || setup_status=$?
fi

if [ "$setup_status" -ne 0 ]; then
  echo "Shell integration skipped. Run lctr setup-shell later to enable scan auto-cd."
fi
