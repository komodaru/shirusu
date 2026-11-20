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
        Rule::OpaqueHandles => Fixture {
            bad: &[(review::EMITTER, "pub struct CallToken { pub call_id: u64 }")],
            good: &[(review::EMITTER, "pub struct CallToken { call_id: u64 }")],
            mutate: (review::EMITTER, |s| Some(s.replace("struct CallToken {", "struct CallToken { pub injected: u64,"))),
        },
        Rule::HookLockPoison => Fixture {
            bad: &[(
                "crates/shirusu-hook/src/runtime.rs",
                "fn f(&self) { let g = self.state.lock().unwrap(); let r = self.map.read().expect(\"r\"); }\nimpl X {\n    fn g(&self) { self.slot.write().unwrap().take(); }\n}\n",
            ), ("crates/shirusu-agent-dll/src/lib.rs", "fn shutdown() { STATE.lock().expect(\"state\").take(); }\n")],
            good: &[
                (
                    "crates/shirusu-hook/src/runtime.rs",
                    "fn f(&self) { let g = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner); let n = s.read(&mut b).unwrap(); }\n#[cfg(all(test, windows))]\nmod tests {\n    fn t() { X.lock().unwrap(); }\n}\n",
                ),
                ("crates/shirusu-hook/src/engine/tests.rs", "fn t() { X.lock().unwrap(); }\n"),
                ("crates/shirusu-host/src/trace_pipeline.rs", "fn f() { X.lock().unwrap(); }\n"),
                ("crates/shirusu-agent/src/lib.rs", "fn f(&self) { self.threads.read().unwrap_or_else(PoisonError::into_inner).len(); }\n#[cfg(test)]\nmod tests {\n    fn t() { X.lock().unwrap(); }\n}\n"),
            ],
            mutate: ("crates/shirusu-agent/src/lib.rs", |t| Some(t.replacen(".read().unwrap_or_else(PoisonError::into_inner)", ".read().unwrap()", 1))),
        },
        Rule::RwxPages => Fixture {
            bad: &[
                ("crates/shirusu-hook/src/engine/trampoline.rs", "fn alloc() { unsafe { VirtualAlloc(null(), n, MEM_COMMIT, PAGE_EXECUTE_READWRITE) }; }\n"),
                (regress::RWX_HOME, "fn write() { protect(t, n, PAGE_EXECUTE_READWRITE, &mut old); protect(t, n, PAGE_EXECUTE_READWRITE, &mut old); finish_inline_write(t); }\n"),
                ("crates/shirusu-agent/src/x.rs", "const P: u32 = windows_sys::Win32::System::Memory::PAGE_EXECUTE_WRITECOPY;\n"),
            ],
            good: &[
                (regress::RWX_HOME, "use x::PAGE_EXECUTE_READWRITE;\nfn write() { protect(t, n, PAGE_EXECUTE_READWRITE, &mut old); finish_inline_write(t); }\n"),
                ("crates/shirusu-hook/src/engine.rs", "use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE};\nfn alloc() { VirtualAlloc(p, n, MEM_COMMIT, PAGE_READWRITE); VirtualProtect(p, n, PAGE_EXECUTE_READ, &mut o); }\n"),
                ("crates/shirusu-hook/src/caller.rs", "#[cfg(all(test, windows))]\nmod tests {\n    fn t() { VirtualAlloc(p, n, MEM_COMMIT, PAGE_EXECUTE_READWRITE); }\n}\n"),
                ("crates/shirusu-hook/src/engine/tests.rs", "fn t() { VirtualAlloc(p, n, MEM_COMMIT, PAGE_EXECUTE_READWRITE); }\n"),
            ],
            mutate: ("crates/shirusu-hook/src/caller.rs", |t| Some(t + "\nfn rwx() -> u32 { windows_sys::Win32::System::Memory::PAGE_EXECUTE_READWRITE }\n")),
        },
        Rule::PanicUnwindKept => Fixture {
            bad: &[
                ("Cargo.toml", "[profile.release]\npanic = \"abort\"\n"),
                (".cargo/config.toml", "[target.x86_64-pc-windows-msvc]\nrustflags = [\"-C\", \"panic=abort\"]\n"),
                ("scripts/ci/windows.ps1", "$env:RUSTFLAGS = '-C panic=abort'\n"),
                (".github/workflows/ci.yml", "      - run: cargo build -Zbuild-std=std,panic_abort\n"),
            ],
            good: &[
                ("Cargo.toml", "[profile.release]\nlto = true\n# panic = \"abort\" では contain_panic が効かない\n"),
                ("scripts/ci/linux.sh", "RUSTFLAGS=\"-D warnings\" cargo test\n"),
                ("docs/x.md", "panic=abort\n"),
            ],
            mutate: ("Cargo.toml", |t| Some(t + "\n[profile.release]\npanic = \"abort\"\n")),
        },
        Rule::EscapeComment => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", "// TO\x44O: 残り\nfn f() {} // for now\n/// コールバックは未対応。\nfn g() { eprintln!(\"not implemented\"); }\n"),
                ("scripts/ci/x.ps1", "# best effort\n"),
            ],
            good: &[
                ("crates/shirusu-x/src/lib.rs", "// Unsupported (値の種類の名前)\n// (not supported, as on Windows)\nfn f() { let s = r\"for now\"; }\n"),
                ("scripts/ci/x.sh", "echo \"# for now\"  # 実測どおり\n"),
            ],
            mutate: ("crates/shirusu-transport/src/shm.rs", |t| Some(t + "\n// for now: 0\nfn _later() {}\n")),
        },
        Rule::NoUnwrap => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", "fn f(o: Option<u8>) -> u8 {\n    o.unwrap()\n}\nfn g(r: Result<u8, ()>) -> u8 {\n    r.expect(\"x\")\n}\nfn h(m: &std::sync::Mutex<u8>) -> u8 {\n    *m.lock().unwrap()\n}\nfn v(o: Option<u8>) -> Vec<u8> {\n    vec![1, o.unwrap()]\n}\nfn s(o: Option<u8>) -> String {\n    format!(\"{}\", o.expect(\"x\"))\n}\n"),
                ("crates/shirusu-y/build.rs", "fn main() { let _ = std::env::var(\"TARGET\").expect(\"cargo\"); }\n"),
            ],
            good: &[
                ("crates/shirusu-x/src/lib.rs", "// .unwrap() とは書かない\nfn f(o: Option<u8>) -> u8 {\n    o.unwrap_or(0)\n}\nfn g(r: Result<u8, ()>) -> Option<u8> {\n    let _ = \".expect(1)\";\n    r.ok()\n}\nstruct S { expect: u8 }\nfn h(s: S) -> u8 { s.expect }\n#[cfg(test)]\nmod tests {\n    fn t() { Some(1).unwrap(); }\n}\n#[cfg(test)]\nfn helper() { Some(1).expect(\"t\"); }\n#[cfg(windows)]\n#[test]\nfn win() { Some(1).unwrap(); }\nimpl S {\n    #[cfg(test)]\n    fn t(&self) { Some(1).unwrap(); }\n}\n#[cfg(kani)]\nmod proofs {\n    fn p() { Some(1).unwrap(); }\n}\n#[kani::proof]\nfn q() { Some(1).unwrap(); }\n"),
                ("crates/shirusu-x/tests/t.rs", "fn main() { Some(1).unwrap(); }\n"),
                ("crates/shirusu-x/src/x_tests.rs", "fn t() { Some(1).unwrap(); }\n"),
                ("e2e/src/bin/x.rs", "fn main() { std::env::var(\"A\").unwrap(); }\n"),
                ("crates/shirusu-x/examples/bench.rs", "fn main() { Some(1).unwrap(); }\n"),
            ],
            mutate: ("crates/shirusu-transport/src/slot.rs", |t| Some(t.replace("self.slots.read().unwrap_or_else(PoisonError::into_inner)", "self.slots.read().unwrap()"))),
        },
        Rule::ShellSpecialVar => Fixture {
            bad: &[
                ("scripts/ci/x.sh", "#!/bin/bash\nGROUPS=gdi\nexport RANDOM=4\nSECONDS=0 cargo test\nPIPESTATUS+=(1)\n"),
                ("scripts/ci/x.ps1", "$input = Get-Content a\nparam([string]$Host)\nforeach ($args in $list) { }\n$PID += 1\n$Error = @()\n"),
                (".github/workflows/ci.yml", "      - env:\n          GROUPS: ${{ inputs.groups }}\n        run: echo\n      - shell: pwsh\n        run: $input = 1\n"),
            ],
            good: &[
                ("scripts/ci/x.sh", "MY_GROUPS=1\necho \"$SECONDS $UID\"\nwhile IFS=$'\\t' read -r a; do :; done\n# GROUPS=1 は捨てられる\n[ \"$x\" = RANDOM ]\n"),
                ("scripts/ci/x.ps1", "$inputPath = 'a'\n$hostName = hostname\nif ($args.Count -eq 0) { }\nWrite-Host $PID\nif ($Error -eq $null) { }\n[int]$args[0]\n$LASTEXITCODE = 0\n# $input = 1\n"),
                (".github/workflows/ci.yml", "      - env:\n          ASKED: ${{ inputs.groups }}\n        run: echo $GROUPS\n"),
            ],
            mutate: ("scripts/ci/windows.ps1", |t| Some(t + "\n$input = 'regression'\n")),
        },
        Rule::ReviewTests => Fixture {
            bad: &[(review::DURABILITY, "/* #[test] fn partial_batches_report_the_exact_retry_suffix() {} */")],
            good: &[(review::DURABILITY, "#[test] fn partial_batches_report_the_exact_retry_suffix() { verify(); }")],
            mutate: (review::DURABILITY, |s| Some(s.replace("fn partial_batches_report_the_exact_retry_suffix", "#[ignore] fn partial_batches_report_the_exact_retry_suffix"))),
        },
        Rule::MutationCases => Fixture {
            bad: &[
                (mutations::SCRIPT, "cases = [\n    ('m', 'crates/shirusu-x/src/lib.rs', [('a == b', 'a != b')], 'shirusu-x', 'it', 'renamed'),\n]\n"),
                ("crates/shirusu-x/src/lib.rs", "fn f(a: u8, b: u8) -> bool { a == b || a == b }"),
                ("crates/shirusu-x/tests/it.rs", "#[test] fn equal_values() {}"),
            ],
            good: &[
                (mutations::SCRIPT, "cases = [\n    ('m', 'crates/shirusu-x/src/lib.rs', [('a == b', 'a != b')], 'shirusu-x', 'it', 'equal'),\n]\n"),
                ("crates/shirusu-x/src/lib.rs", "fn f(a: u8, b: u8) -> bool { a == b }"),
                ("crates/shirusu-x/tests/it.rs", "#[test] fn equal_values() {}\n#[test] #[ignore] fn equal_slow() {}\nfn equal_helper() {}"),
            ],
            mutate: ("crates/shirusu-test/tests/rules.rs", |s| Some(s.replace("fn uncaptured_fields_control_integrity", "fn uncaptured_values_control_integrity"))),
        },
        Rule::Required => Fixture {
            bad: &[("scripts/ci/linux.sh", "cargo build\n")],
            good: &[("scripts/ci/linux.sh", "cargo run -q -p shirusu-lint\ncargo clippy --workspace --all-targets -- -D warnings\ncargo test --workspace\ncargo run -q -p shirusu-lint -- --test-log \"$test_log\"\n")],
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
        assert!(
            !found.is_empty(),
            "{}: no-local-allow が見つけない",
            one[0].0
        );
    }
}

#[test]
fn commenting_out_either_ci_lint_invocation_is_detected() {
    let real = Tree::repo(&root());
    let path = "scripts/ci/linux.sh";
    let script = real.text(path);
    let lines: Vec<_> = script.lines().collect();
    let commands: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("cargo run -q -p shirusu-lint"))
        .collect();
    assert_eq!(commands.len(), 2);
    for (index, command) in commands {
        let commented = format!("# {command}");
        let mut modified = lines.clone();
        modified[index] = &commented;
        for newline in ["\n", "\r\n"] {
            let mut tree = Tree::repo(&root());
            tree.overlay
                .insert(path.into(), Some(modified.join(newline)));
            assert!(!Rule::Required.check(&tree).is_empty(), "{command}");
        }
    }
}

#[test]
fn each_regression_spelling_alone_is_found() {
    // (一つの綴りが見つかっても、見逃した別の綴りが隠れないように一件ずつ)
    for r in [
        Rule::RwxPages,
        Rule::PanicUnwindKept,
        Rule::ShellSpecialVar,
        Rule::EscapeComment,
    ] {
        for one in fixture(r).bad.chunks(1) {
            assert!(
                !r.check(&made_up(one)).is_empty(),
                "{}: {} に見つけない",
                r.id(),
                one[0].0
            );
        }
    }
    // (それぞれの行が一件: bash 4 行、PowerShell 5 行、ワークフロー 2 行)
    assert_eq!(
        Rule::ShellSpecialVar
            .check(&made_up(fixture(Rule::ShellSpecialVar).bad))
            .len(),
        11
    );
    assert_eq!(
        Rule::HookLockPoison
            .check(&made_up(fixture(Rule::HookLockPoison).bad))
            .len(),
        4
    );
    // (unwrap、expect、lock().unwrap()、vec! の中、format! の中、build.rs の expect)
    assert_eq!(
        Rule::NoUnwrap
            .check(&made_up(fixture(Rule::NoUnwrap).bad))
            .len(),
        6
    );
}

#[test]
fn bash_drops_an_assignment_to_groups() {
    // (shell-special-var の理由を、この機械の bash で示す)
    if let Ok(o) = std::process::Command::new("bash")
        .args(["-c", "GROUPS=gdi; echo \"$GROUPS\""])
        .output()
    {
        assert_ne!(String::from_utf8_lossy(&o.stdout).trim(), "gdi");
    }
}

#[test]
fn each_escape_word_is_found_alone() {
    for w in [
        concat!("TO", "DO: later"),
        concat!("FIX", "ME"),
        concat!("HA", "CK"),
        "zero for now",
        "for the moment",
        "a simplified model",
        "not implemented",
        "unimplemented",
        "not ported",
        "not modelled yet",
        "best effort",
        "best-effort install",
        "good enough",
        "stubbed",
        "a logged stub",
        "a workaround",
        "とりあえず",
        "簡略化",
        "未実装",
        "未対応",
        "暫定",
        "ひとまず",
        "手抜き",
        "近似",
        "仮実装",
        "dropped: unsupported",
        "not supported",
        "an approximation",
    ] {
        assert!(escape::is_escape(w, w), "{w:?} が見つからない");
    }
}

#[test]
fn windows_answers_with_evidence_pass() {
    for w in [
        "binary labels: not supported, as on Windows",
        "Unsupported (ValueKind の名前)",
        "RCP: approximations, each vendor's own (x86)",
        "D2D1_GEOMETRY_SIMPLIFICATION_OPTION_LINES",
        "Simplify(options)",
        "既定値は省略する",
        "not supported (実測)",
    ] {
        assert!(!escape::is_escape(w, w), "{w:?} を誤って見つけた");
    }
}

#[test]
fn a_known_site_covers_as_many_as_it_is_listed() {
    let one = made_up(&[
        ("crates/shirusu-x/src/lib.rs", "// for now\n"),
        (
            escape::KNOWN,
            "crates/shirusu-x/src/lib.rs\t// for now\t直す\n",
        ),
    ]);
    assert!(escape::escape_comment(&one).is_empty());
    let two = made_up(&[
        ("crates/shirusu-x/src/lib.rs", "// for now\n// for now\n"),
        (
            escape::KNOWN,
            "crates/shirusu-x/src/lib.rs\t// for now\t直す\n",
        ),
    ]);
    assert_eq!(escape::escape_comment(&two).len(), 1);
}
