//! 各規則を退行に当てる: 架空の木 (見つけること)、正しい書き方 (見つけないこと)、実際の
//! ファイルに退行を入れた木 (見つけること)。規則に fixture がないとコンパイルが通らない。
use super::*;

type Files = &'static [(&'static str, &'static str)];

struct Fixture {
    bad: Files,
    good: Files,
    /// (実ファイル, 中身 -> 退行を入れた中身; None は削除)
    mutate: (&'static str, fn(String) -> Option<String>),
}

fn fixture(r: Rule) -> Fixture {
    match r {
        Rule::LintsInherited => Fixture {
            bad: &[("crates/shirusu-x/Cargo.toml", "[package]\nname = \"shirusu-x\"\n")],
            good: &[("crates/shirusu-x/Cargo.toml", "[package]\nname = \"shirusu-x\"\n\n[lints]\nworkspace = true\n")],
            mutate: ("crates/shirusu-transport/Cargo.toml", |t| Some(t.replace("[lints]", "[lints-gone]"))),
        },
        Rule::DenyKept => Fixture {
            bad: &[("Cargo.toml", "[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = \"warn\"\n")],
            good: &[(
                "Cargo.toml",
                "[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = \"deny\"\n[workspace.lints.clippy]\nnot_unsafe_ptr_arg_deref = \"deny\"\nmissing_safety_doc = \"deny\"\nundocumented_unsafe_blocks = \"deny\"\n",
            )],
            mutate: ("Cargo.toml", |t| Some(t.replace("undocumented_unsafe_blocks = \"deny\"", "undocumented_unsafe_blocks = \"warn\""))),
        },
        Rule::NoLocalAllow => Fixture {
            bad: &[("crates/shirusu-x/src/lib.rs", "#![expect(unsafe_op_in_unsafe_fn)]\n#[allow(clippy::undocumented_unsafe_blocks)]\nfn f() {}\n"), ("crates/shirusu-y/src/lib.rs", "#[cfg_attr(all(), allow(\n    clippy::undocumented_unsafe_blocks,\n))]\nfn f() {}\n"), ("crates/shirusu-z/src/lib.rs", "macro_rules! m {\n    () => {\n        #[allow(unsafe_op_in_unsafe_fn)]\n        unsafe fn f() {}\n    };\n}\n")],
            good: &[("crates/shirusu-x/src/lib.rs", "// #[allow(clippy::undocumented_unsafe_blocks)] とは書かない\n#[allow(clippy::too_many_arguments)]\nfn f() { let _ = \"#[allow(unsafe_op_in_unsafe_fn)]\"; }\n#[cfg_attr(not(test), allow(dead_code))]\nfn g() {}\n")],
            mutate: ("crates/shirusu-transport/src/shm.rs", |t| Some(t + "\n#[allow(clippy::undocumented_unsafe_blocks)]\nfn _lint_regression() {}\n")),
        },
        Rule::ShmRawUnsafe => Fixture {
            bad: &[(
                "crates/shirusu-transport/src/shm.rs",
                "impl Shm {\n    pub fn copy_out(&self, r: Range<usize>) -> Option<Vec<u8>> { None }\n    pub fn snapshot(&self) -> Vec<u8> { vec![] }\n    pub fn as_slice(&self) -> &[u8] { &[] }\n}\n",
            )],
            good: &[(
                "crates/shirusu-transport/src/shm.rs",
                "impl Shm {\n    pub unsafe fn copy_out(&self, r: Range<usize>) -> Option<Vec<u8>> { None }\n    pub unsafe fn snapshot(&self) -> Vec<u8> { vec![] }\n}\nimpl ControlSnapshot {\n    pub fn snapshot(self) -> [u8; 64] { [0; 64] }\n}\nimpl ControlBytes {\n    pub fn as_slice(&self) -> &[u8] { &[] }\n}\n",
            )],
            mutate: ("crates/shirusu-transport/src/shm.rs", |t| Some(t.replace("pub unsafe fn copy_out(", "pub fn copy_out("))),
        },
        Rule::CompileFailKept => Fixture {
            bad: &[(UI_LIST, "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs\n"), ("crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.stderr", "e")],
            good: &[
                (UI_LIST, "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs\n"),
                ("crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs", "fn main() {}"),
                ("crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.stderr", "e"),
                ("crates/shirusu-transport/tests/compile_fail.rs", "t.compile_fail(\"tests/ui/*.rs\");"),
            ],
            mutate: ("crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs", |_| None),
        },
        Rule::BuildOutput => Fixture {
            bad: &[("target/debug/shirusu.exe", "x"), ("dist/shirusu_agent.dll", "x")],
            good: &[("corpus/x.json", "x"), ("crates/shirusu-hook/src/target.rs", "x")],
            mutate: ("crates/shirusu-agent-dll/target/release/shirusu_agent.dll", |_| Some("x".into())),
        },
        Rule::Required => Fixture {
            bad: &[("scripts/ci/linux.sh", "cargo build\n")],
            good: &[("scripts/ci/linux.sh", "cargo run -q -p shirusu-lint\ncargo clippy --workspace --all-targets -- -D warnings\ncargo test --workspace\n")],
            mutate: ("scripts/ci/linux.sh", |t| Some(t.replace("cargo run -q -p shirusu-lint", ""))),
        },
    }
}

fn made_up(files: Files) -> Tree {
    Tree {
        root: PathBuf::new(),
        base: Some(
            files
                .iter()
                .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
                .collect(),
        ),
        overlay: BTreeMap::new(),
    }
}

#[test]
fn each_rule_finds_its_regression_in_a_fixture() {
    for r in Rule::ALL {
        assert!(
            !r.check(&made_up(fixture(r).bad)).is_empty(),
            "{}: 退行の fixture で何も見つけない",
            r.id()
        );
    }
}

#[test]
fn each_rule_lets_the_right_spelling_through() {
    for r in Rule::ALL {
        let hits = r.check(&made_up(fixture(r).good));
        assert!(
            hits.is_empty(),
            "{}: 正しい fixture で発火する: {hits:?}",
            r.id()
        );
    }
}

#[test]
fn each_rule_finds_its_regression_put_back_into_the_real_tree() {
    let real = Tree::repo(&root());
    for r in Rule::ALL {
        let before = r.check(&real).len();
        let (file, mutate) = fixture(r).mutate;
        let mut tree = Tree::repo(&root());
        tree.overlay
            .insert(file.to_owned(), mutate(real.text(file)));
        assert!(
            r.check(&tree).len() > before,
            "{}: {file} に入れた退行を見逃す",
            r.id()
        );
    }
}

#[test]
fn the_tree_itself_passes() {
    let found = check(&Tree::repo(&root()));
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn every_allow_spelling_is_found() {
    // (複数行、cfg_attr の中、マクロの本体の中: どれも一件ずつ)
    for one in fixture(Rule::NoLocalAllow).bad.chunks(1) {
        let found = Rule::NoLocalAllow.check(&made_up(one));
        assert!(!found.is_empty(), "{}: no-local-allow が見つけない", one[0].0);
    }
}
