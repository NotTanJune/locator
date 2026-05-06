#!/bin/sh
set -eu

if command -v rustup >/dev/null 2>&1; then
  rustup update
fi

cargo update
cargo test
cargo clippy --all-targets -- -D warnings
