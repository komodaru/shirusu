#!/usr/bin/env bash
# scripts/ci/mirror.txt の 1 行のファイルを、このリポの release ci-bundles から取る
# (2026-11-24: CI は実行時に GitHub の外を読まない)。mirror.txt に書いた sha256 で確かめる。
#   scripts/ci/fetch.sh NAME OUT
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 2 ] || { echo "usage: scripts/ci/fetch.sh NAME OUT" >&2; exit 2; }
name=$1
out=$2
sum=$(awk -v n="$name" '!/^#/ && $1 == n { print $2 }' "$here/mirror.txt")
[ -n "$sum" ] || { echo "fetch: $name は scripts/ci/mirror.txt の行にない" >&2; exit 1; }
url=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-komodaru/shirusu}/releases/download/ci-bundles/$name
curl -fsSL --retry 5 --retry-all-errors --connect-timeout 30 -o "$out.part" "$url" \
    || { echo "fetch: $url を取れない: ci-bundles ワークフローで上げてあるか" >&2; exit 1; }
echo "$sum  $out.part" | sha256sum -c --quiet || { rm -f "$out.part"; echo "fetch: $name の sha256 が違う" >&2; exit 1; }
mv "$out.part" "$out"
