//! trybuild: ハンドルの所有・借用・他プロセスの値を型で分ける (レビュー P1-4)。
//! 作り直しは `TRYBUILD=overwrite cargo test -p shirusu-transport-win --test
//! compile_fail` (同じ理由で落ちていることを差分で確かめる)。
//!
//! Windows では std の `OwnedHandle` と名前が重なり、rustc が型をフルパスで
//! 表示するため、型名を含む診断は OS ごとの期待出力に分ける。

#[test]
fn handle_kinds_do_not_mix() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
    #[cfg(windows)]
    t.compile_fail("tests/ui/windows/*.rs");
    #[cfg(not(windows))]
    t.compile_fail("tests/ui/portable/*.rs");
}
