#!/bin/sh
set -eu

repo="NotTanJune/locator"
mac_asset="lctr-aarch64-apple-darwin.tar.gz"
windows_asset="lctr-x86_64-pc-windows-msvc.zip"

usage() {
  echo "Usage: $0 <version|vversion> [artifact-dir]" >&2
  exit 2
}

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  usage
fi

input_version=$1
artifact_dir=${2:-}

case "$input_version" in
  v*)
    tag=$input_version
    version=${input_version#v}
    ;;
  *)
    version=$input_version
    tag="v$input_version"
    ;;
esac

if [ -z "$version" ]; then
  usage
fi

base_url="https://github.com/$repo/releases/download/$tag"
tmp_dir=

if [ -z "$artifact_dir" ]; then
  tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/lctr-release-assets.XXXXXX")
fi

cleanup() {
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
}
trap cleanup EXIT

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

asset_path() {
  asset=$1

  if [ -n "$artifact_dir" ]; then
    path="$artifact_dir/$asset"
    if [ ! -f "$path" ]; then
      echo "Missing release artifact: $path" >&2
      exit 1
    fi
    printf '%s\n' "$path"
    return
  fi

  path="$tmp_dir/$asset"
  curl -fL -o "$path" "$base_url/$asset"
  printf '%s\n' "$path"
}

if [ ! -f Formula/lctr.rb ]; then
  echo "Formula/lctr.rb not found. Run from the repository root." >&2
  exit 1
fi

if [ ! -f bucket/lctr.json ]; then
  echo "bucket/lctr.json not found. Run from the repository root." >&2
  exit 1
fi

mac_path=$(asset_path "$mac_asset")
windows_path=$(asset_path "$windows_asset")
mac_sha=$(sha256_file "$mac_path")
windows_sha=$(sha256_file "$windows_path")

ruby - "$version" "$base_url" "$mac_asset" "$mac_sha" "$windows_asset" "$windows_sha" <<'RUBY'
require "json"

version, base_url, mac_asset, mac_sha, windows_asset, windows_sha = ARGV

def replace_once(text, pattern, replacement, label)
  abort "Could not find #{label} in Formula/lctr.rb" unless text.match?(pattern)

  text.sub(pattern, replacement)
end

formula_path = "Formula/lctr.rb"
formula = File.read(formula_path)
formula = replace_once(
  formula,
  %r{url "https://github\.com/NotTanJune/locator/releases/download/v[^"]+/lctr-aarch64-apple-darwin\.tar\.gz"},
  %(url "#{base_url}/#{mac_asset}"),
  "stable URL"
)
formula = replace_once(formula, /sha256 "[0-9a-f]{64}"/, %(sha256 "#{mac_sha}"), "stable SHA256")
formula = replace_once(
  formula,
  /assert_match "lctr [^"]+", shell_output\("#\{bin\}\/lctr --version"\)/,
  %(assert_match "lctr #{version}", shell_output("\#{bin}/lctr --version")),
  "version test"
)
File.write(formula_path, formula)

bucket_path = "bucket/lctr.json"
manifest = JSON.parse(File.read(bucket_path))
manifest["version"] = version
manifest.fetch("architecture").fetch("64bit")["url"] = "#{base_url}/#{windows_asset}"
manifest.fetch("architecture").fetch("64bit")["hash"] = windows_sha
manifest.fetch("autoupdate").fetch("architecture").fetch("64bit")["url"] =
  "https://github.com/NotTanJune/locator/releases/download/v$version/#{windows_asset}"

File.write(bucket_path, "#{JSON.pretty_generate(manifest, indent: "    ")}\n")
RUBY

echo "Updated Formula/lctr.rb for $tag with $mac_sha"
echo "Updated bucket/lctr.json for $tag with $windows_sha"
