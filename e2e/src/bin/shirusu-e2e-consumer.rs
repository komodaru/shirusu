//! E2E consumer: producer が書いたレコードファイル / 自己完結の検証を行う専用プログラム。
// Rust 1.97: バイナリの中で誰も使わない pub 項目は死んだコード (テストは多くを使うので test では見ない)
#![cfg_attr(not(test), warn(dead_code_pub_in_binary))]
use std::io::Read as _;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pos) = args.iter().position(|a| a == "--file") else {
        eprintln!("usage: shirusu-e2e-consumer --file <path>");
        std::process::exit(2);
    };
    let path = &args[pos + 1];
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut buf)
        .unwrap();
    let mut idx = 0usize;
    let mut n = 0u64;
    while idx < buf.len() {
        let seq = u64::from_le_bytes(buf[idx..idx + 8].try_into().unwrap());
        let len = u32::from_le_bytes(buf[idx + 8..idx + 12].try_into().unwrap()) as usize;
        let payload = &buf[idx + 12..idx + 12 + len];
        assert_eq!(seq, n, "sequence mismatch");
        assert_eq!(payload, seq.to_le_bytes(), "payload mismatch");
        idx += 12 + len;
        n += 1;
    }
    println!("consumer: verified {n} records from {path}");
}
