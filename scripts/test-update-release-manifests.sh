#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/lctr-manifest-test.XXXXXX")

cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

assert_contains() {
  file=$1
  expected=$2

  if ! grep -F "$expected" "$file" >/dev/null 2>&1; then
    echo "Expected $file to contain:" >&2
    echo "$expected" >&2
    exit 1
  fi
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

mkdir -p "$tmp_dir/Formula" "$tmp_dir/bucket" "$tmp_dir/artifacts"
cp "$repo_root/Formula/lctr.rb" "$tmp_dir/Formula/lctr.rb"
cp "$repo_root/bucket/lctr.json" "$tmp_dir/bucket/lctr.json"
old_version=$(ruby -rjson -e 'print JSON.parse(File.read(ARGV[0])).fetch("version")' "$tmp_dir/bucket/lctr.json")

printf 'mac fixture archive\n' > "$tmp_dir/artifacts/lctr-aarch64-apple-darwin.tar.gz"
printf 'windows fixture archive\n' > "$tmp_dir/artifacts/lctr-x86_64-pc-windows-msvc.zip"

mac_sha=$(sha256_file "$tmp_dir/artifacts/lctr-aarch64-apple-darwin.tar.gz")
windows_sha=$(sha256_file "$tmp_dir/artifacts/lctr-x86_64-pc-windows-msvc.zip")

(
  cd "$tmp_dir"
  "$repo_root/scripts/update-release-manifests.sh" v1.2.3 "$tmp_dir/artifacts"
)

assert_contains "$tmp_dir/Formula/lctr.rb" \
  'url "https://github.com/NotTanJune/locator/releases/download/v1.2.3/lctr-aarch64-apple-darwin.tar.gz"'
assert_contains "$tmp_dir/Formula/lctr.rb" "sha256 \"$mac_sha\""
assert_contains "$tmp_dir/Formula/lctr.rb" 'assert_match "lctr 1.2.3", shell_output("#{bin}/lctr --version")'

ruby -rjson -e '
manifest = JSON.parse(File.read(ARGV[0]))
version, windows_sha = ARGV[1], ARGV[2]
expected_url = "https://github.com/NotTanJune/locator/releases/download/v#{version}/lctr-x86_64-pc-windows-msvc.zip"

abort "wrong version" unless manifest.fetch("version") == version
abort "wrong url" unless manifest.fetch("architecture").fetch("64bit").fetch("url") == expected_url
abort "wrong hash" unless manifest.fetch("architecture").fetch("64bit").fetch("hash") == windows_sha
abort "wrong autoupdate url" unless manifest.fetch("autoupdate").fetch("architecture").fetch("64bit").fetch("url") == "https://github.com/NotTanJune/locator/releases/download/v$version/lctr-x86_64-pc-windows-msvc.zip"
' "$tmp_dir/bucket/lctr.json" "1.2.3" "$windows_sha"

if [ "$old_version" != "1.2.3" ] &&
  grep -F "$old_version" "$tmp_dir/Formula/lctr.rb" "$tmp_dir/bucket/lctr.json" >/dev/null 2>&1; then
  echo "Old version remained in updated manifests." >&2
  exit 1
fi
