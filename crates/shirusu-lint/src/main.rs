//! shirusu の構造の lint: レビュー (2026-11-15/16) で型と構造に移した決まりが、次の変更で
//! 黙って戻らないように検査する。clippy の deny 設定や trybuild は「その場の 1 式」を見るが、
//! ここで見るのは「その設定や検査そのものが残っているか」と「どこに何を書いてよいか」。
//! Rust のファイルは syn で構文として読む (コメントや文字列に書かれた名前は数えない)。
//!
//!   cargo run -q -p shirusu-lint      検査 (CI: scripts/ci/linux.sh)
//!   cargo run -q -p shirusu-lint -- --test-log FILE  Cargo の pretty 形式の結果で回帰テストの実行を確認
//!   cargo test -p shirusu-lint        各規則が、退行を入れた架空の木と、実際のファイルに
//!                                     退行を入れた木の両方で発火し、正しい書き方では
//!                                     発火しないこと (tests.rs: 規則を足して fixture を
//!                                     書かないとコンパイルが通らない)
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod regress;
mod review;

use quote::ToTokens;
use syn::visit::Visit;

/// deny にしてある unsafe の lint (workspace の Cargo.toml)
const DENY: [&str; 4] = [
    "unsafe_op_in_unsafe_fn",
    "not_unsafe_ptr_arg_deref",
    "missing_safety_doc",
    "undocumented_unsafe_blocks",
];

/// 規則のすべて: 変種を足すと tests.rs の fixture 表 (網羅的な match) が書かれるまでコンパイルが通らない
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    LintsInherited,
    DenyKept,
    NoLocalAllow,
    ShmRawUnsafe,
    CompileFailKept,
    BuildOutput,
    Required,
    OpaqueHandles,
    ReviewTests,
    HookLockPoison,
    RwxPages,
    PanicUnwindKept,
    ShellSpecialVar,
}

impl Rule {
    const ALL: [Rule; 13] = [
        Rule::LintsInherited,
        Rule::DenyKept,
        Rule::NoLocalAllow,
        Rule::ShmRawUnsafe,
        Rule::CompileFailKept,
        Rule::BuildOutput,
        Rule::Required,
        Rule::OpaqueHandles,
        Rule::ReviewTests,
        Rule::HookLockPoison,
        Rule::RwxPages,
        Rule::PanicUnwindKept,
        Rule::ShellSpecialVar,
    ];

    fn id(self) -> &'static str {
        match self {
            Rule::LintsInherited => "lints-inherited",
            Rule::DenyKept => "deny-kept",
            Rule::NoLocalAllow => "no-local-allow",
            Rule::ShmRawUnsafe => "shm-raw-unsafe",
            Rule::CompileFailKept => "compile-fail-kept",
            Rule::BuildOutput => "build-output",
            Rule::Required => "required",
            Rule::OpaqueHandles => "opaque-handles",
            Rule::ReviewTests => "review-tests",
            Rule::HookLockPoison => "hook-lock-poison",
            Rule::RwxPages => "rwx-pages",
            Rule::PanicUnwindKept => "panic-unwind-kept",
            Rule::ShellSpecialVar => "shell-special-var",
        }
    }

    /// 何を守るか、代わりに何を書くか
    fn why(self) -> &'static str {
        match self {
            Rule::LintsInherited => "どの crate も workspace の lint (unsafe の deny 4 つ) を継ぐ: `[lints] workspace = true` のない crate は deny を受けずに unsafe を書ける (レビュー 2026-11-15)",
            Rule::DenyKept => "workspace の Cargo.toml が unsafe の 4 lint を deny のまま持つ (レビュー 2026-11-15)",
            Rule::NoLocalAllow => "deny した unsafe の lint を #[allow] / #[expect] で局所的に外さない: 理由は // SAFETY: と # Safety に書く",
            Rule::ShmRawUnsafe => "共有メモリの生コピーは unsafe のまま (レビュー 2026-11-16 P0): Shm::copy_out と snapshot は unsafe fn、Shm / Ring に借用の as_slice を戻さない",
            Rule::CompileFailKept => "型の守りを示す trybuild fixture は減らない (scripts/ci/arch-lint-ui.txt、各 .rs に .stderr、crate の tests が tests/ui を読む); 新しいものは一覧に足す",
            Rule::BuildOutput => "ビルド成果物を git に入れない (target/ 以下、コンパイル・圧縮済みのファイル)",
            Rule::OpaqueHandles => "Token・Guard・Slot 登録権のフィールドと生成経路を隠す",
            Rule::ReviewTests => "Trace・API・並行処理の回帰テストを残し、CI で実行する",
            Rule::HookLockPoison => "hook のテストでないコードでロックの結果を unwrap / expect しない (55e6774、レビュー 2026-11-14 P2-9: 毒されたロックで以後の Detour がすべて panic した): `.unwrap_or_else(std::sync::PoisonError::into_inner)` で取り戻す",
            Rule::RwxPages => "書き込みと実行が同時にできるページを作らない (a12dc8e、レビュー 2026-11-14 P3-6: トランポリンが RWX のまま残った): 書いてから PAGE_EXECUTE_READ に。例外はパッチを書く間だけの inline_hooks.rs の一か所",
            Rule::PanicUnwindKept => "panic=abort にしない: Agent DLL の contain_panic と CallGuard の catch_unwind は巻き戻しで panic を止める (masu の 82cdd0d0 は panic=abort で catch_unwind が効かず落ちた)",
            Rule::ShellSpecialVar => "bash の特殊変数 (GROUPS、RANDOM、SECONDS..) と PowerShell の自動変数 ($input、$args、$Host、$PID、$Error..) に代入しない (masu の bench.yml が入力を GROUPS で渡していた): 自分の名前を付ける",
            Rule::Required => "守りそのものが残る: CI の手順、生成 Detour 表の契約 golden、Control 範囲のテスト",
        }
    }

    fn check(self, tree: &Tree) -> Vec<Finding> {
        let hits = match self {
            Rule::LintsInherited => lints_inherited(tree),
            Rule::DenyKept => deny_kept(tree),
            Rule::NoLocalAllow => no_local_allow(tree),
            Rule::ShmRawUnsafe => shm_raw_unsafe(tree),
            Rule::CompileFailKept => compile_fail_kept(tree),
            Rule::BuildOutput => build_output(tree),
            Rule::Required => required(tree),
            Rule::OpaqueHandles => review::opaque_handles(tree),
            Rule::ReviewTests => review::registered_tests(tree),
            Rule::HookLockPoison => regress::hook_lock_poison(tree),
            Rule::RwxPages => regress::rwx_pages(tree),
            Rule::PanicUnwindKept => regress::panic_unwind_kept(tree),
            Rule::ShellSpecialVar => regress::shell_special_var(tree),
        };
        hits.into_iter()
            .map(|(file, line, what)| Finding {
                rule: self,
                file,
                line,
                what,
            })
            .collect()
    }
}

#[derive(Debug)]
struct Finding {
    rule: Rule,
    file: String,
    line: usize,
    what: String,
}

type Hits = Vec<(String, usize, String)>;

/// git の追跡ファイル (base を渡せば架空の木) に overlay (None は削除) を重ねたもの
struct Tree {
    root: PathBuf,
    base: Option<BTreeMap<String, String>>,
    overlay: BTreeMap<String, Option<String>>,
}

impl Tree {
    fn repo(root: &Path) -> Tree {
        Tree {
            root: root.to_owned(),
            base: None,
            overlay: BTreeMap::new(),
        }
    }

    fn paths(&self) -> BTreeSet<String> {
        let mut ps: BTreeSet<String> = match &self.base {
            Some(b) => b.keys().cloned().collect(),
            None => std::process::Command::new("git")
                .args(["ls-files", "-z"])
                .current_dir(&self.root)
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split('\0')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        };
        for (p, t) in &self.overlay {
            if t.is_some() {
                ps.insert(p.clone());
            } else {
                ps.remove(p);
            }
        }
        ps
    }

    fn text(&self, p: &str) -> String {
        if let Some(t) = self.overlay.get(p) {
            return t.clone().unwrap_or_default();
        }
        match &self.base {
            Some(b) => b.get(p).cloned().unwrap_or_default(),
            None => std::fs::read(self.root.join(p))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default(),
        }
    }

    /// 架空の木では、持っているファイルだけを検査の対象にする
    fn has_or_real(&self, p: &str) -> bool {
        self.base.as_ref().is_none_or(|b| b.contains_key(p))
    }
}

fn line(span: proc_macro2::Span) -> usize {
    span.start().line
}

fn lints_inherited(tree: &Tree) -> Hits {
    tree.paths()
        .into_iter()
        .filter(|p| {
            p.starts_with("crates/") && p.ends_with("/Cargo.toml") && p.matches('/').count() == 2
        })
        .filter(|p| {
            let t = tree.text(p);
            let lines: Vec<&str> = t
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            !lines.windows(2).any(|w| {
                w.first() == Some(&"[lints]")
                    && w.get(1)
                        .is_some_and(|l| l.replace(' ', "") == "workspace=true")
            })
        })
        .map(|p| (p, 1, "[lints] workspace = true がない".to_owned()))
        .collect()
}

fn deny_kept(tree: &Tree) -> Hits {
    let t = tree.text("Cargo.toml");
    DENY.iter()
        .filter(|l| {
            !t.lines()
                .any(|x| x.replace(' ', "") == format!("{l}=\"deny\""))
        })
        .map(|l| ("Cargo.toml".to_owned(), 1, format!("{l} = \"deny\" がない")))
        .collect()
}

/// 属性の中の識別子 (`clippy::undocumented_unsafe_blocks` の各語)
fn words(ts: proc_macro2::TokenStream, out: &mut Vec<String>) {
    for t in ts {
        match t {
            proc_macro2::TokenTree::Ident(i) => out.push(i.to_string()),
            proc_macro2::TokenTree::Group(g) => words(g.stream(), out),
            proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => {}
        }
    }
}

#[derive(Default)]
struct Allows(Vec<(usize, String)>);

impl Allows {
    /// `#[allow(..)]` / `#[expect(..)]`、または `#[cfg_attr(条件, allow(..))]` の中身
    fn attr(&mut self, line: usize, name: &str, tokens: proc_macro2::TokenStream) {
        let mut w = vec![];
        words(tokens, &mut w);
        let allows = name == "allow"
            || name == "expect"
            || (name == "cfg_attr" && w.iter().any(|x| x == "allow" || x == "expect"));
        if !allows {
            return;
        }
        for d in DENY.iter().filter(|d| w.iter().any(|x| x == *d)) {
            self.0.push((line, format!("#[{name}(.. {d} ..)]")));
        }
    }

    /// マクロの本体の中の属性 (syn は属性として読まない)
    fn tokens(&mut self, ts: proc_macro2::TokenStream) {
        use proc_macro2::{Delimiter, TokenTree};
        let toks: Vec<TokenTree> = ts.into_iter().collect();
        for (i, t) in toks.iter().enumerate() {
            match t {
                TokenTree::Punct(p) if p.as_char() == '#' => {
                    let mut j = i + 1;
                    if matches!(toks.get(j), Some(TokenTree::Punct(b)) if b.as_char() == '!') {
                        j += 1;
                    }
                    let Some(TokenTree::Group(g)) = toks.get(j) else {
                        continue;
                    };
                    if g.delimiter() != Delimiter::Bracket {
                        continue;
                    }
                    let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                    if let (Some(TokenTree::Ident(n)), Some(TokenTree::Group(l))) =
                        (inner.first(), inner.get(1))
                    {
                        self.attr(line(p.span()), &n.to_string(), l.stream());
                    }
                }
                TokenTree::Group(g) => self.tokens(g.stream()),
                TokenTree::Punct(_) | TokenTree::Ident(_) | TokenTree::Literal(_) => {}
            }
        }
    }
}

impl<'a> Visit<'a> for Allows {
    fn visit_attribute(&mut self, a: &'a syn::Attribute) {
        if let (syn::Meta::List(l), Some(n)) = (&a.meta, a.path().get_ident()) {
            self.attr(line(a.pound_token.span), &n.to_string(), l.tokens.clone());
        }
        syn::visit::visit_attribute(self, a);
    }
    fn visit_macro(&mut self, m: &'a syn::Macro) {
        self.tokens(m.tokens.clone());
        syn::visit::visit_macro(self, m);
    }
}

fn no_local_allow(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for p in tree.paths().into_iter().filter(|p| p.ends_with(".rs")) {
        let Ok(f) = syn::parse_file(&tree.text(&p)) else {
            continue;
        };
        let mut v = Allows::default();
        v.visit_file(&f);
        out.extend(v.0.into_iter().map(|(l, w)| (p.clone(), l, w)));
    }
    out
}

const SHM: [&str; 2] = [
    "crates/shirusu-transport/src/shm.rs",
    "crates/shirusu-transport/src/ring.rs",
];

/// 共有メモリそのものを表す型 (値の写しの ControlSnapshot などは別)
fn is_shared(ty: &syn::Type) -> bool {
    let syn::Type::Path(p) = ty else { return false };
    p.path.segments.last().is_some_and(|s| {
        let n = s.ident.to_string();
        n == "Shm" || n.starts_with("Ring")
    })
}

fn shm_raw_unsafe(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for p in SHM {
        let Ok(f) = syn::parse_file(&tree.text(p)) else {
            continue;
        };
        for item in &f.items {
            let syn::Item::Impl(i) = item else { continue };
            for it in &i.items {
                let syn::ImplItem::Fn(m) = it else { continue };
                let name = m.sig.ident.to_string();
                let safe = m.sig.unsafety.is_none();
                let by_ref = m.sig.receiver().is_some_and(|r| r.reference.is_some());
                let ret = match &m.sig.output {
                    syn::ReturnType::Type(_, t) => t.to_token_stream().to_string(),
                    syn::ReturnType::Default => String::new(),
                };
                let l = line(m.sig.ident.span());
                if safe && name == "copy_out" {
                    out.push((p.to_owned(), l, "unsafe でない copy_out".into()));
                }
                if safe && name == "snapshot" && by_ref && ret.replace(' ', "") == "Vec<u8>" {
                    out.push((
                        p.to_owned(),
                        l,
                        "unsafe でない snapshot(&self) -> Vec<u8>".into(),
                    ));
                }
                if is_shared(&i.self_ty) && (name == "as_slice" || name == "as_mut_slice") {
                    out.push((p.to_owned(), l, format!("共有メモリの借用 {name}")));
                }
            }
        }
    }
    out
}

const UI_LIST: &str = "scripts/ci/arch-lint-ui.txt";

fn compile_fail_kept(tree: &Tree) -> Hits {
    let have = tree.paths();
    let list = tree.text(UI_LIST);
    let mut out = Hits::new();
    for (k, l) in list.lines().enumerate() {
        let p = l.trim();
        if p.is_empty() || p.starts_with('#') {
            continue;
        }
        let at = k.saturating_add(1);
        if !have.contains(p) {
            out.push((UI_LIST.to_owned(), at, format!("{p} がない")));
        } else if !have.contains(&format!("{}.stderr", p.trim_end_matches(".rs"))) {
            out.push((p.to_owned(), 1, ".stderr がない".into()));
        } else {
            let harness = format!(
                "{}/tests/compile_fail.rs",
                p.split("/tests/ui/").next().unwrap_or_default()
            );
            if !tree.text(&harness).contains("tests/ui") {
                out.push((harness, 1, "tests/ui を読まない".into()));
            }
        }
    }
    if tree.base.is_none() {
        for p in have
            .iter()
            .filter(|p| p.contains("/tests/ui/") && p.ends_with(".rs"))
        {
            if !list.lines().any(|l| l.trim() == p) {
                out.push((p.clone(), 1, format!("{UI_LIST} にない fixture (足す)")));
            }
        }
    }
    out
}

const BUILD_EXT: [&str; 14] = [
    "wasm", "exe", "dll", "so", "dylib", "o", "a", "lib", "rlib", "pdb", "zst", "tar", "tgz", "7z",
];

fn build_output(tree: &Tree) -> Hits {
    tree.paths()
        .into_iter()
        .filter(|p| {
            let ext = p
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            let packed = (BUILD_EXT.contains(&ext.as_str()) || ext == "zip")
                && !p.contains("/testdata/")
                && !p.starts_with("corpus/");
            p.starts_with("target/") || p.contains("/target/") || packed
        })
        .map(|p| (p.clone(), 1, p))
        .collect()
}

const REQUIRED: [(&str, &str, &str); 6] = [
    (
        "scripts/ci/linux.sh",
        "cargo run -q -p shirusu-lint",
        "この lint が毎回動く (その tests は cargo test --workspace で動く)",
    ),
    (
        "scripts/ci/linux.sh",
        "cargo test --workspace",
        "各規則が退行で発火することを毎回確かめる",
    ),
    (
        "scripts/ci/linux.sh",
        "cargo clippy --workspace --all-targets -- -D warnings",
        "deny した lint が CI で効く",
    ),
    (
        "scripts/ci/linux.sh",
        r#"cargo run -q -p shirusu-lint -- --test-log "$test_log""#,
        "回帰テストの実行結果を検査する",
    ),
    (
        "crates/shirusu-hook/tests/generic_contract.rs",
        "engine_generated.inc",
        "生成 Detour 表の契約 (GENERIC_CONTRACT) の golden",
    ),
    (
        "crates/shirusu-transport/src/shm.rs",
        "copy_out_is_bounds_checked_and_never_covers_control",
        "copy_out が Control 範囲を返さないテスト",
    ),
];

fn required(tree: &Tree) -> Hits {
    REQUIRED
        .iter()
        .filter(|&(p, t, _)| {
            if !tree.has_or_real(p) {
                return false;
            }
            let source = tree.text(p);
            if p.ends_with(".sh") {
                !source.lines().map(str::trim).any(|cmd| {
                    cmd == *t
                        || (*t == "cargo test --workspace"
                            && cmd.starts_with("cargo test --workspace "))
                })
            } else {
                !source.contains(t)
            }
        })
        .map(|(p, t, why)| ((*p).to_owned(), 1, format!("`{t}` がない: {why}")))
        .collect()
}

fn check(tree: &Tree) -> Vec<Finding> {
    Rule::ALL.iter().flat_map(|r| r.check(tree)).collect()
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag, path] if flag == "--test-log" => {
            return match std::fs::read_to_string(path) {
                Ok(log) => review::check_log(&log),
                Err(e) => {
                    eprintln!("shirusu-lint: {path:?}: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {
            eprintln!("usage: shirusu-lint [--test-log FILE]");
            return ExitCode::FAILURE;
        }
    }
    let found = check(&Tree::repo(&root()));
    for f in &found {
        println!(
            "shirusu-lint: {}:{}: {}: {}\n    ({})",
            f.file,
            f.line,
            f.rule.id(),
            f.what,
            f.rule.why()
        );
    }
    if found.is_empty() {
        println!("shirusu-lint: ok ({} 規則)", Rule::ALL.len());
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "shirusu-lint: {} 件 (規則ごとの説明に代わりの書き方がある)",
            found.len()
        );
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests;
