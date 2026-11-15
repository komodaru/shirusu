//! trybuild: 検証済みの呼び出し列 (PlannedProgram) は plan_steps でしか
//! 作れず、中身も書き換えられない (レビュー P1-3)。期待する診断は
//! ui/*.stderr (作り直しは `TRYBUILD=overwrite cargo test -p shirusu-test
//! --test compile_fail`)。

#[test]
fn planned_programs_come_only_from_plan_steps() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
