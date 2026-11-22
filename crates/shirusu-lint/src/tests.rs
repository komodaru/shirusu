//! 各規則を退行に当てる: 架空の木 (見つけること)、正しい書き方 (見つけないこと)、実際の
//! ファイルに退行を入れた木 (見つけること)。規則に fixture がないとコンパイルが通らない。
use super::*;

type Files = &'static [(&'static str, &'static str)];

/// dup の fixture: 数えられる長さの関数 (e2e::perf の p95 と同じ形)
const DUP_FN: &str = "pub fn percentile(mut v: Vec<u64>) -> u64 {\n    if v.is_empty() {\n        return 0;\n    }\n    v.sort_unstable();\n    let idx = ((v.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);\n    v[idx.min(v.len() - 1)]\n}\n";

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
                "[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = \"deny\"\nraw_borrows_via_references = \"deny\"\n[workspace.lints.clippy]\nnot_unsafe_ptr_arg_deref = \"deny\"\nmissing_safety_doc = \"deny\"\nundocumented_unsafe_blocks = \"deny\"\n",
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
        Rule::Dup => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", DUP_FN),
                ("crates/shirusu-y/tests/it.rs", "/// 別名の写し\nfn p95(mut v: Vec<u64>) -> u64 {\n    if v.is_empty() {\n        return 0;\n    }\n    v.sort_unstable();\n    let idx = ((v.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);\n    v[idx.min(v.len() - 1)]\n}\n"),
            ],
            good: &[
                ("crates/shirusu-x/src/lib.rs", DUP_FN),
                ("crates/shirusu-y/tests/it.rs", "fn p95(mut v: Vec<u64>) -> u64 {\n    if v.is_empty() {\n        return 0;\n    }\n    v.sort_unstable();\n    let idx = ((v.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);\n    v[idx.min(v.len() - 1)]\n}\n"),
            ],
            // (bench が e2e::perf の p95 を写して持ち直す)
            mutate: ("e2e/src/bin/shirusu-tuning-bench.rs", |t| Some(t + "\nfn p95_again(mut v: Vec<u64>) -> u64 {\n    if v.is_empty() {\n        return 0;\n    }\n    v.sort_unstable();\n    let idx = ((v.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);\n    v[idx.min(v.len() - 1)]\n}\n")),
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
        Rule::WildcardUnreachable => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", "fn f(m: usize) -> u8 {\n    match m {\n        0 => 1,\n        _ => unreachable!(),\n    }\n}\nfn g(m: u8) -> u8 {\n    match m {\n        0 => 1,\n        _ => { core::unreachable!(\"m < 1\") }\n    }\n}\n"),
                ("e2e/src/bin/x.rs", "fn main() { match 1 { 1 => {}, _ => std::unreachable!() } }\n"),
            ],
            good: &[("crates/shirusu-x/src/lib.rs", "// _ => unreachable!() とは書かない\nenum M { A, B }\nfn f(m: M) -> u8 {\n    match m {\n        M::A => 1,\n        M::B => 2,\n    }\n}\nfn g(m: u8) -> u8 {\n    match m {\n        0 => 1,\n        _ => 2,\n    }\n}\nfn h(v: Result<u8, !>) -> u8 {\n    let Ok(x) = v;\n    x\n}\n")],
            mutate: ("crates/shirusu-hook/src/shared_com.rs", |t| Some(t.replace("Unknown::Release => WRAPPERS[index].2 as usize,", "Unknown::Release => WRAPPERS[index].2 as usize,\n        #[allow(unreachable_patterns)]\n        _ => unreachable!(),"))),
        },
        Rule::BlobRefWalk => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", "fn present(e: &TraceEvent, has: &dyn Fn(&BlobId) -> bool) -> bool {\n    if let PayloadRef::Blob(id) = &e.payload {\n        return has(id);\n    }\n    true\n}\nfn arg(v: &TypedValue) -> Option<BlobId> {\n    let TypedValue::BlobRef(id) = v else {\n        return None;\n    };\n    Some(*id)\n}\n"),
                ("crates/shirusu-y/src/lib.rs", "fn any(e: &TraceEvent) -> bool { matches!(e.payload, shirusu_trace::PayloadRef::Blob(_)) }\nfn pick(e: &TraceEvent) -> Option<BlobId> {\n    match &e.payload {\n        PayloadRef::Blob(id) => Some(*id),\n        _ => None,\n    }\n}\n"),
            ],
            good: &[
                ("crates/shirusu-x/src/lib.rs", "// if let PayloadRef::Blob(id) = &e.payload とは書かない\nfn present(e: &TraceEvent, r: &SchemaRegistry, has: impl Fn(&BlobId) -> bool) -> Option<bool> {\n    e.blob_refs(r).all_present(has)\n}\nfn show(p: &PayloadRef) -> String {\n    match p {\n        PayloadRef::None => String::new(),\n        PayloadRef::Blob(id) => id.to_hex(),\n        PayloadRef::Inline(b) => format!(\"{b:?}\"),\n        PayloadRef::Dropped { .. } => \"dropped\".into(),\n    }\n}\n#[cfg(test)]\nmod tests {\n    fn t(e: &TraceEvent) -> bool { matches!(e.payload, PayloadRef::Blob(_)) }\n}\n"),
                (results::BLOB_REFS_HOME, "fn walk(e: &TraceEvent) { if let PayloadRef::Blob(id) = &e.payload { use_it(id); } }\n"),
            ],
            mutate: ("crates/shirusu-cli/src/repair.rs", |t| Some(t + "\nfn _lint_regression(e: &TraceEvent) -> bool {\n    if let PayloadRef::Blob(_) = e.payload {\n        return true;\n    }\n    false\n}\n")),
        },
        Rule::NeverNeedsNoReturn => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", "fn f(m: LastErrorMeaning, ret: Option<u64>) -> bool {\n    match m {\n        LastErrorMeaning::WhenReturnIsFalse | LastErrorMeaning::Never => ret.is_some_and(|r| r != 0),\n        _ => false,\n    }\n}\n"),
                ("crates/shirusu-y/tests/t.rs", "#[test]\nfn t() {\n    let want = match m {\n        Never | WhenReturnIsNull => f.ret.is_some(),\n        _ => false,\n    };\n}\n"),
            ],
            good: &[("crates/shirusu-x/src/lib.rs", "// Never | WhenReturnIsFalse とは書かない\nfn rule(m: LastErrorMeaning) -> LastErrorRule {\n    match m {\n        LastErrorMeaning::Never => LastErrorRule::Fixed { meaningful: false },\n        LastErrorMeaning::WhenReturnIsFalse | LastErrorMeaning::WhenReturnIsNull => LastErrorRule::OnReturn(ReturnTest::IsFalse),\n        LastErrorMeaning::Unspecified | LastErrorMeaning::Always => LastErrorRule::Fixed { meaningful: true },\n    }\n}\n")],
            mutate: ("crates/shirusu-trace/src/schema.rs", |t| Some(t.replace("            Self::WhenReturnIsFalse => LastErrorRule::OnReturn(ReturnTest::IsFalse),", "            Self::WhenReturnIsFalse | Self::Never => LastErrorRule::OnReturn(ReturnTest::IsFalse),"))),
        },
        Rule::IntegrityGate => Fixture {
            bad: &[("crates/shirusu-x/src/lib.rs", "fn gate(i: &IntegritySummary) -> Result<(), String> {\n    if !i.has_finalization\n        || i.event_integrity != Some(0)\n        || i.degraded_events != 0\n    {\n        return Err(\"incomplete\".into());\n    }\n    Ok(())\n}\n")],
            good: &[
                ("crates/shirusu-x/src/lib.rs", "// i.event_integrity != Some(0) || i.degraded_events != 0 と並べない\nfn gate(i: &IntegritySummary) -> Result<(), String> {\n    i.require(Use::RecordedSequence)?;\n    if i.dropped_payload_events > 0 { println!(\"{}\", i.dropped_payload_events); }\n    if a || b { }\n    Ok(())\n}\n#[cfg(test)]\nmod tests {\n    fn t(i: &IntegritySummary) -> bool { i.degraded_events != 0 || i.missing_blob_refs != 0 }\n}\n"),
                (results::INTEGRITY_HOME, "fn incompleteness(&self) -> bool { self.event_count_mismatch || self.degraded_events > 0 }\n"),
            ],
            mutate: ("crates/shirusu-probe/src/replay.rs", |t| Some(t + "\nfn _lint_regression(i: &shirusu_analysis::IntegritySummary) -> bool {\n    i.event_integrity != Some(0) || i.degraded_events != 0\n}\n")),
        },
        Rule::BlobRefBody => Fixture {
            bad: &[(results::BLOB_REFS_HOME, "fn walk(v: &CapturedValue<Borrowed<'_>>, known: &mut Vec<BlobId>) {\n    match &v.value {\n        TypedValue::BlobRef(id) => known.push(*id),\n        TypedValue::U32(_) => {}\n    }\n    let values = decode_values(types, args);\n}\n")],
            good: &[
                (results::BLOB_REFS_HOME, "// decode_values で複製しない\nfn walk(v: &CapturedValue<Borrowed<'_>>, known: &mut Vec<BlobId>) {\n    match v.read_value() {\n        Some(TypedValue::BlobRef(id)) => known.push(*id),\n        Some(TypedValue::U32(_)) | None => {}\n    }\n}\n"),
                ("crates/shirusu-x/src/lib.rs", "fn show(v: &CapturedValue) -> String {\n    match &v.value {\n        TypedValue::BlobRef(id) => id.to_hex(),\n        TypedValue::U32(x) => x.to_string(),\n    }\n}\n"),
            ],
            mutate: (results::BLOB_REFS_HOME, |t| Some(t.replace("match v.read_value() {", "match Some(&v.value) {"))),
        },
        Rule::ValueReader => Fixture {
            bad: &[("crates/shirusu-trace/src/compact.rs", "fn validate(cur: &mut Cursor, ty: Ty) -> Result<Option<u64>, CompactError> {\n    Ok(match ty {\n        Ty::Str16 => { cur.read_uleb()?; None }\n        Ty::Ptr64 => { cur.read_exact(8)?; None }\n        Ty::U64 => Some(cur.read_uleb()?),\n    })\n}\n")],
            good: &[("crates/shirusu-trace/src/compact.rs", "fn read_value_ref<'a>(cur: &mut Cursor<'a>, ty: Ty) -> Result<TypedValue<Borrowed<'a>>, CompactError> {\n    Ok(match ty {\n        Ty::Str16 => TypedValue::Str16(Str16Bytes::new(cur.read_exact(2)?).ok_or(CompactError::Truncated)?),\n        Ty::Ptr64 => TypedValue::Ptr64(u64::from_le_bytes(cur.read_array::<8>()?)),\n    })\n}\nfn tag(ty: Ty) -> u32 {\n    match ty {\n        Ty::Str16 => 9,\n        Ty::Ptr64 => 13,\n    }\n}\n")],
            mutate: ("crates/shirusu-trace/src/compact.rs", |t| Some(t + "\nfn _lint_regression(cur: &mut Cursor, ty: Ty) -> Result<u64, CompactError> {\n    Ok(match ty {\n        Ty::Str16 => cur.read_uleb()?,\n        Ty::Ptr64 => u64::from_le_bytes(cur.read_array::<8>()?),\n        _ => 0,\n    })\n}\n")),
        },
        Rule::LastErrorInputsHome => Fixture {
            bad: &[("crates/shirusu-conformance/src/check.rs", "fn check(f: &str, input: &dyn Fn(&str) -> Option<u64>, ret: u64) -> bool {\n    LastErrorMeaning::resolve_inputs(f, input, ret, 0).is_meaningful(ret)\n}\n")],
            good: &[
                ("crates/shirusu-conformance/src/check.rs", "// resolve_inputs( を直接呼ばない\nfn check(i: &IntegritySummary) -> bool {\n    i.inconsistent_calls.is_empty()\n}\n"),
                ("crates/shirusu-hook/src/capture.rs", "fn m(f: &str) -> LastErrorMeaning { LastErrorMeaning::resolve_inputs(f, &|_| None, 0, 0) }\n"),
                ("crates/shirusu-trace/src/schema.rs", "fn c(f: &str) -> LastErrorMeaning { LastErrorMeaning::resolve_inputs(f, &|_| None, 0, 0) }\n"),
            ],
            mutate: ("crates/shirusu-conformance/src/check.rs", |t| Some(t + "\nfn _lint_regression() -> shirusu_trace::schema::LastErrorMeaning {\n    shirusu_trace::schema::LastErrorMeaning::resolve_inputs(\"CreateFileW\", &|_| None, 0, 0)\n}\n")),
        },
        Rule::StorageFullCount => Fixture {
            bad: &[("crates/shirusu-host/src/trace_pipeline.rs", "fn write(totals: &mut PipelineReport, staged: Staged) {\n    if totals.storage_full {\n        totals.storage_full_dropped_events += staged.events.len() as u64;\n    }\n}\n")],
            good: &[("crates/shirusu-host/src/trace_pipeline.rs", "// dropped_events += events.len() と数えない\nfn write(totals: &mut PipelineReport, recorder: &mut TraceRecorder<W>, staged: Staged) {\n    if totals.storage_full {\n        totals.storage_full_dropped_events += recorder.discard_unwritable(&staged.context, &staged.events);\n    }\n    totals.storage_full_dropped_losses += losses.len() as u64;\n}\n")],
            mutate: ("crates/shirusu-host/src/trace_pipeline.rs", |t| Some(t.replace("totals.storage_full_dropped_events += recorder.discard_unwritable(&staged.context, &staged.events);", "totals.storage_full_dropped_events += staged.events.len() as u64;"))),
        },
        Rule::CompareVerdict => Fixture {
            bad: &[("crates/shirusu-x/src/bin/x.rs", "fn main() {\n    let report = compare_traces(&a, &b);\n    if report.candidates.is_empty() {\n        println!(\"identical\");\n    }\n}\n")],
            good: &[
                ("crates/shirusu-x/src/bin/x.rs", "// report.candidates.is_empty() で一致としない\nfn main() {\n    let report = compare_traces(&a, &b);\n    if report.verdict().is_identical() {\n        println!(\"identical\");\n    }\n    let n = report.candidates.len();\n    let empty = report.left_input.is_empty();\n}\n#[cfg(test)]\nmod tests {\n    fn t(r: &DiffReport) { assert!(r.candidates.is_empty()); }\n}\n"),
                (results::VERDICT_HOME, "impl DiffReport {\n    pub fn verdict(&self) -> Verdict {\n        if !self.candidates.is_empty() { return Verdict::Differs { candidates: 1 }; }\n        Verdict::Identical\n    }\n}\n"),
            ],
            mutate: ("crates/shirusu-compare/src/bin/shirusu-compare.rs", |t| Some(t.replace("Ok(!report.verdict().is_identical())", "Ok(!report.candidates.is_empty())"))),
        },
        Rule::BinDeadPub => Fixture {
            bad: &[
                ("crates/shirusu-x/src/main.rs", "fn main() {}\n"),
                ("crates/shirusu-y/src/bin/y.rs", "#![cfg_attr(not(test), allow(dead_code_pub_in_binary))]\nfn main() {}\n"),
                ("e2e/src/bin/z.rs", "//! z\n#![allow(clippy::all)]\nfn main() {}\n"),
            ],
            good: &[
                ("crates/shirusu-x/src/main.rs", "//! x\n#![cfg_attr(not(test), warn(dead_code_pub_in_binary))]\nfn main() {}\n"),
                ("e2e/src/bin/z.rs", "#![deny(dead_code_pub_in_binary)]\nfn main() {}\n"),
                ("crates/shirusu-x/src/lib.rs", "pub fn f() {}\n"),
                ("crates/shirusu-x/src/bin/common/mod.rs", "pub fn f() {}\n"),
            ],
            mutate: ("crates/shirusu-cli/src/main.rs", |t| Some(t.replace("#![cfg_attr(not(test), warn(dead_code_pub_in_binary))]\n", ""))),
        },
        Rule::HandStd => Fixture {
            bad: &[
                ("crates/shirusu-x/src/lib.rs", "fn a(s: &mut S) { s.on = !s.on; }\nfn b(n: &str) -> &str { n.strip_prefix(\"x\").unwrap_or(n) }\nfn c(o: Option<u8>) -> u8 { o.map(|v| v + 1).unwrap_or_default() }\nfn d(o: Option<&str>) -> String { o.map_or(String::new(), str::to_owned) }\n"),
                ("crates/shirusu-y/src/lib.rs", "fn e(ok: bool) -> Result<(), E> { if ok { Ok(()) } else { Err(E::Bad) } }\nfn f(v: &[u8]) -> Vec<u32> { (0..v.len()).map(|i| u32::from(v[i])).collect::<Vec<u32>>() }\nstruct Clear<'a>(&'a AtomicBool);\nimpl Drop for Clear<'_> {\n    fn drop(&mut self) {\n        self.0.store(false, Ordering::Release);\n    }\n}\n"),
            ],
            good: &[(
                "crates/shirusu-x/src/lib.rs",
                "fn a(s: &mut S) { s.on.toggle(); s.a = !s.b; }\nfn b(n: &str, d: &str) -> &str { n.strip_prefix(\"x\").unwrap_or(d) }\nfn c(o: Option<u8>) -> u8 { o.map_or_default(|v| v + 1) }\nfn e(ok: bool) -> Result<u8, E> { if ok { Ok(1) } else { Err(E::Bad) } }\nfn f(n: u32) -> Vec<u32> { (0..n).map(|i| i * 2).collect::<Vec<u32>>() }\nstruct Stop(Arc<AtomicBool>);\nimpl Drop for Stop {\n    fn drop(&mut self) {\n        self.0.store(true, Ordering::Relaxed);\n        wake();\n    }\n}\n",
            )],
            mutate: ("crates/shirusu-agent/src/writer_cell.rs", |t| Some(t + "\nstruct ClearOnDrop<'a>(&'a AtomicBool);\nimpl Drop for ClearOnDrop<'_> {\n    fn drop(&mut self) {\n        self.0.store(false, Ordering::Release);\n    }\n}\n")),
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
        let mutated = mutate(real.text(file));
        // (変異が木に当たらなければ検出の確認にならない: 先に変わったことを確かめる)
        assert_ne!(
            mutated.as_deref(),
            Some(real.text(file).as_str()),
            "{}: {file} への変異が何も変えない",
            r.id()
        );
        let mut tree = Tree::repo(&root());
        tree.overlay.insert(file.to_owned(), mutated);
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

#[test]
fn crlf_checkouts_read_like_lf() {
    // (Windows の autocrlf: 同じ木を CRLF にしても、どの規則の変異も当たって検出される)
    let real = Tree::repo(&root());
    for r in Rule::ALL {
        let (file, mutate) = fixture(r).mutate;
        let crlf = real.text(file).replace('\n', "\r\n");
        let mut tree = Tree::repo(&root());
        // (変異がファイルを新しく作る規則では、元のファイルは無いまま比べる)
        if real.paths().contains(file) {
            tree.overlay.insert(file.to_owned(), Some(crlf));
            assert_eq!(tree.text(file), real.text(file), "{}: {file}", r.id());
        }
        let before = r.check(&tree).len();
        let mutated = mutate(tree.text(file));
        assert_ne!(mutated.as_deref(), Some(tree.text(file).as_str()), "{}: CRLF の {file} に変異が当たらない", r.id());
        tree.overlay.insert(file.to_owned(), mutated.map(|m| m.replace('\n', "\r\n")));
        assert!(r.check(&tree).len() > before, "{}: CRLF の {file} で退行を見逃す", r.id());
    }
}
