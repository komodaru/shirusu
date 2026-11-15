// 検証を飛ばして呼び出し列を作ることはできない。
use shirusu_test::runner_exec::PlannedProgram;

fn main() {
    let _program = PlannedProgram { calls: Vec::new() };
}
