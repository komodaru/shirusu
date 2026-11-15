// 計画済みの呼び出しは読めるが、引数を書き換えることはできない。
use shirusu_test::runner_exec::plan_steps;

fn main() {
    let mut program = plan_steps(&[]).unwrap();
    program[0].args.clear();
}
