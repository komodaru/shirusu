// A fixed read past its record does not build (tests/ui).
#[test]
fn ui_reads_past_the_record_do_not_build() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
