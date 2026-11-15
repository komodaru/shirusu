//! trybuild: 検証していない共有メモリは安全な API へ届かない (レビュー P0)。
//! 各 ui/*.rs はコンパイルに失敗しなければならない。期待する診断は
//! ui/*.stderr にある (Rust を上げて文言が変わったら
//! `TRYBUILD=overwrite cargo test -p shirusu-transport --test compile_fail`
//! で作り直し、差分が「同じ理由で落ちている」ことを確かめてから入れる)。

#[test]
fn unvalidated_memory_cannot_reach_safe_apis() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
