//! shirusu-conformance CLI の統合テスト：終了コードを確認する。

use std::process::Command;
#[path = "common/valid_trace.rs"]
mod valid_trace;
use valid_trace::valid_trace_bytes;

#[test]
fn cli_exit_codes() {
    let dir = std::env::temp_dir().join(format!("shirusu-conformance-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good.trace");
    std::fs::write(&good, valid_trace_bytes()).unwrap();
    let bad = dir.join("bad.trace");
    std::fs::write(&bad, b"not a trace at all").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_shirusu-conformance"))
        .arg(&good)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = Command::new(env!("CARGO_BIN_EXE_shirusu-conformance"))
        .arg(&good)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"conformant\": true"));

    let out = Command::new(env!("CARGO_BIN_EXE_shirusu-conformance"))
        .arg(&bad)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    let out = Command::new(env!("CARGO_BIN_EXE_shirusu-conformance"))
        .arg(dir.join("missing.trace"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}
