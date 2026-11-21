#!/usr/bin/env bash
# MSRV check (Rust 1.100 — 2026-11-21 に こた が決定: never 型 `!` を型引数に使う。
# それまでは 1.87 (2026-11-09 に kota が決定。依存グラフが成立する最小)。)
#
# 1.100 で読めることを --all-targets で確かめる (テストコードまで: dev-dependency の
# rust-version も効く)。1.87 時代の Cargo.lock のピン (generator ≤ 0.8.11、
# trybuild ≤ 1.0.119、proptest ≤ 1.11.x) は 1.100 では要らないが、ロックはそのまま
# --locked で使う (上げるのは別の `cargo update` で)。
set -euo pipefail
rustc --version
cargo check --workspace --all-targets --locked
