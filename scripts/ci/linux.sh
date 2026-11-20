#!/usr/bin/env bash
# Linux CI: fmt / clippy / unit / PBT / loom. ローカルでも同じ手順を再現できる。
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$HOME/.cargo/bin:$PATH"
# 構造の lint (レビューで決めたことが戻っていないか; crates/shirusu-lint)。各規則が退行で
# 発火することはその tests が cargo test --workspace で確かめる
cargo run -q -p shirusu-lint
# L03: 生成表の最新性検査。winmd は NuGet の固定版から取得する。
bash scripts/ci/fetch-winmd.sh
cargo run -q -p shirusu-metadata-gen -- --winmd target/winmd/Windows.Win32.winmd --com-catalog crates/shirusu-trace/src/com_catalog_generated.inc --check
cargo run -q -p shirusu-metadata-gen -- --winmd target/winmd/Windows.Win32.winmd --refresh-broad --check
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
# unwrap / expect はテストでないコードに書かない (shirusu-lint の no-unwrap は syn で cfg(windows) の側も
# 見る; こちらは型で解決したマクロ展開の後を見る)。e2e/ はテストのハーネスなので外す
cargo clippy --workspace --exclude shirusu-e2e --lib --bins -- -D clippy::unwrap_used -D clippy::expect_used
test_log=$(mktemp)
trap 'rm -f "$test_log"' EXIT
cargo test --workspace --color never -- --format pretty 2>&1 | tee "$test_log"
cargo run -q -p shirusu-lint -- --test-log "$test_log"
cargo test -p shirusu-loom-tests --release

cargo run -q -p shirusu-metadata-gen -- --file-io-contracts --check
