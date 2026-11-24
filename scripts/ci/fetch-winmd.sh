#!/usr/bin/env bash
# Win32 Metadata (Windows.Win32.winmd) を取得する (L03)。
#
# 生成器 win32-metadata-gen の入力。Microsoft の Win32 Metadata
# (NuGet `Microsoft.Windows.SDK.Win32Metadata` の固定版) から取る。
# 版は crates/shirusu-metadata-gen/apis.toml の winmd_version と
# 同じにすること。nupkg は scripts/ci/mirror.txt の行で、release ci-bundles から取る
# (scripts/ci/fetch.sh、2026-11-24: CI は実行時に GitHub の外を読まない)。版を変えたら
# mirror.txt の行も変える。取得済みで sha256 が一致すれば何もしない。
#
# 使い方: bash scripts/ci/fetch-winmd.sh
# 出力: target/winmd/Windows.Win32.winmd
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION="71.0.45-preview"
WINMD_SHA256="b9e476a81e14060040804e4cf09b2973fc53a174befc7b3b670d4db72eef55af"
OUT_DIR="target/winmd"
OUT="$OUT_DIR/Windows.Win32.winmd"

if [ -f "$OUT" ]; then
    if [ "$(sha256sum "$OUT" | cut -d' ' -f1)" = "$WINMD_SHA256" ]; then
        echo "winmd already present: $OUT"
        exit 0
    fi
    echo "winmd hash mismatch, re-downloading: $OUT"
fi

mkdir -p "$OUT_DIR"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
NUPKG="$TMP/win32metadata.nupkg"
bash scripts/ci/fetch.sh "microsoft.windows.sdk.win32metadata.${VERSION}.nupkg" "$NUPKG"
unzip -o -q "$NUPKG" Windows.Win32.winmd -d "$TMP"
if [ "$(sha256sum "$TMP/Windows.Win32.winmd" | cut -d' ' -f1)" != "$WINMD_SHA256" ]; then
    echo "winmd sha256 mismatch (expected $WINMD_SHA256)" >&2
    exit 1
fi
cp "$TMP/Windows.Win32.winmd" "$OUT"
echo "fetched $OUT (Microsoft.Windows.SDK.Win32Metadata $VERSION)"
