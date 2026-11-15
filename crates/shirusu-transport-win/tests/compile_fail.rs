//! trybuild: ハンドルの所有・借用・他プロセスの値を型で分ける (レビュー P1-4)。
//! 作り直しは `TRYBUILD=overwrite cargo test -p shirusu-transport-win --test
//! compile_fail` (同じ理由で落ちていることを差分で確かめる)。

#[test]
fn handle_kinds_do_not_mix() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
