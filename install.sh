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
