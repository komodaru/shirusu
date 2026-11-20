//! shirusu-conformance バイナリのカバレッジ gap テスト（2026-11-12 計測分）。
//!
//! `tests/cli.rs` では到達しない分岐が対象：`--help`、`--bogus`、
//! 引数の個数違い。Trace 読み取り・適合判定の本体は cli.rs で
//! 既に駆動されている。

use std::process::Command;
#[path = "common/valid_trace.rs"]
mod valid_trace;
use valid_trace::valid_trace_bytes;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_shirusu-conformance")
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shirusu-conformance-coverage-gaps-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cli_help_exits_zero() {
    let out = Command::new(bin()).arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("usage: shirusu-conformance"));
}

#[test]
fn cli_unknown_option_exits_two() {
    let dir = tmp_dir("unknown");
    let good = dir.join("good.trace");
    std::fs::write(&good, valid_trace_bytes()).unwrap();
    let out = Command::new(bin())
        .arg(&good)
        .arg("--bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    // 位置引数の数ではなく、未知のオプションとして拒否する。
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown option: --bogus"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_wrong_arg_count_exits_two() {
    let dir = tmp_dir("argc");
    let good = dir.join("good.trace");
    std::fs::write(&good, valid_trace_bytes()).unwrap();

    // 引数なし。
    let out = Command::new(bin()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    // 位置引数が 2 個。
    let out = Command::new(bin()).arg(&good).arg(&good).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}
