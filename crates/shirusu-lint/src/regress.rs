//! 一度直した不具合の形を戻さない規則 (2026-11-20、「これまでに色々直してきたと思うけど再発防止の
//! ためにリンター整備してな」)。各規則はそれを決めた修正を名指しする。型で禁じられるものは型に
//! したので、ここに残るのは「書けてしまうが、一度間違えた書き方」。
use std::collections::BTreeSet;

use syn::visit::Visit;

use super::{line, Hits, Tree};

/// `#[cfg(test)]`、`#[cfg(all(test, windows))]` など test でだけ作られる項目
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && matches!(&a.meta, syn::Meta::List(l) if {
                let s: String = l.tokens.to_string().split_whitespace().collect();
                s == "test" || s.strip_prefix("all(").and_then(|r| r.strip_suffix(')')).is_some_and(|r| r.split(',').any(|c| c == "test"))
            })
    })
}

/// テストのファイル (tests/ の下、tests.rs、*_tests.rs)
fn is_test_file(p: &str) -> bool {
    p.contains("/tests/") || p.ends_with("/tests.rs") || p.ends_with("_tests.rs")
}

/// `#[cfg(test)] mod x;` が持ち込むファイル (x.rs と x/mod.rs)
fn test_mod_files(tree: &Tree, files: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in files {
        let Ok(file) = syn::parse_file(&tree.text(f)) else {
            continue;
        };
        let dir = match f.rsplit_once('/') {
            Some((d, "mod.rs" | "lib.rs" | "main.rs")) => d.to_owned(),
            Some(_) => f.trim_end_matches(".rs").to_owned(),
            None => String::new(),
        };
        for it in &file.items {
            if let syn::Item::Mod(m) = it {
                if m.content.is_none() && is_cfg_test(&m.attrs) {
                    out.insert(format!("{dir}/{}.rs", m.ident));
                    out.insert(format!("{dir}/{}/mod.rs", m.ident));
                }
            }
        }
    }
    out
}

/// `prefix` の下の、テストでない Rust ファイル
fn product_rs(tree: &Tree, prefix: &str) -> Vec<String> {
    let files: Vec<String> = tree
        .paths()
        .into_iter()
        .filter(|p| p.starts_with(prefix) && p.ends_with(".rs") && !is_test_file(p))
        .collect();
    let tests = test_mod_files(tree, &files);
    files.into_iter().filter(|f| !tests.contains(f)).collect()
}

// --- hook-lock-poison ---------------------------------------------------------------------------

/// 対象プロセスの中で動く crate (Detour、Agent、Agent DLL: どのスレッドからでも呼ばれ、
/// 一つの panic でロックが毒されると以後のすべての呼び出しが panic する)
pub(super) const IN_TARGET: [&str; 3] = [
    "crates/shirusu-hook/src/",
    "crates/shirusu-agent/src/",
    "crates/shirusu-agent-dll/src/",
];

/// テストでない場所の `x.lock().unwrap()` / `.read().expect(..)` / `.write().unwrap()`
#[derive(Default)]
struct LockUnwrap(Vec<(usize, String)>);

impl<'a> Visit<'a> for LockUnwrap {
    fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
        if !is_cfg_test(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        if !is_cfg_test(&f.attrs) {
            syn::visit::visit_item_fn(self, f);
        }
    }
    fn visit_expr_method_call(&mut self, m: &'a syn::ExprMethodCall) {
        if m.method == "unwrap" || m.method == "expect" {
            if let syn::Expr::MethodCall(r) = &*m.receiver {
                let lock = r.method == "lock" || r.method == "read" || r.method == "write";
                if lock && r.args.is_empty() {
                    self.0.push((
                        line(m.method.span()),
                        format!(".{}().{}()", r.method, m.method),
                    ));
                }
            }
        }
        syn::visit::visit_expr_method_call(self, m);
    }
}

pub(super) fn hook_lock_poison(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in IN_TARGET.iter().flat_map(|p| product_rs(tree, p)) {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        let mut v = LockUnwrap::default();
        v.visit_file(&file);
        out.extend(v.0.into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}

// --- no-unwrap ----------------------------------------------------------------------------------

/// テストでない式の `.unwrap()` と `.expect(..)` (2026-11-20「unwrapを根絶する」)
#[derive(Default)]
struct Unwraps(Vec<(usize, String)>);

/// テストか証明でだけ作られる項目: `#[cfg(test)]` の類、`#[test]`、Kani の `#[cfg(kani)]` と
/// `#[kani::proof]`
fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    is_cfg_test(attrs)
        || attrs.iter().any(|a| {
            let p = a.path();
            p.is_ident("test")
                || (p.segments.len() == 2 && p.segments[0].ident == "kani")
                || (p.is_ident("cfg")
                    && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string().split_whitespace().collect::<String>() == "kani"))
        })
}

impl<'a> Visit<'a> for Unwraps {
    fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
        if !is_test_only(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        if !is_test_only(&f.attrs) {
            syn::visit::visit_item_fn(self, f);
        }
    }
    fn visit_item_impl(&mut self, i: &'a syn::ItemImpl) {
        if !is_test_only(&i.attrs) {
            syn::visit::visit_item_impl(self, i);
        }
    }
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        if !is_test_only(&f.attrs) {
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
    fn visit_expr_method_call(&mut self, m: &'a syn::ExprMethodCall) {
        let n = m.method.to_string();
        if (n == "unwrap" && m.args.is_empty()) || (n == "expect" && m.args.len() == 1) {
            self.0.push((line(m.method.span()), format!(".{n}()")));
        }
        syn::visit::visit_expr_method_call(self, m);
    }
    /// `vec![..]`、`format!(..)`、`assert!(..)` などの引数 (式をカンマで並べたもの) の中も見る
    fn visit_macro(&mut self, m: &'a syn::Macro) {
        use syn::punctuated::Punctuated;
        if let Ok(args) =
            m.parse_body_with(Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
        {
            let mut inner = Unwraps::default();
            for e in &args {
                inner.visit_expr(e);
            }
            self.0.extend(inner.0);
        }
        syn::visit::visit_macro(self, m);
    }
}

/// syn で読むので cfg(windows) の側も Linux の CI で見る (clippy は自分のターゲットの側しか見ない)。
/// 対象は crates/ の製品のコード (build.rs も)。テストと同じ扱いで外すのは、テストのファイル、
/// e2e/ (端から端までのテストのハーネスとその計測プログラム) と examples/ (開発用の計測)。
pub(super) fn no_unwrap(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in product_rs(tree, "crates/")
        .into_iter()
        .filter(|f| !f.contains("/examples/"))
    {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        let mut v = Unwraps::default();
        v.visit_file(&file);
        out.extend(v.0.into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}

// --- rwx-pages ----------------------------------------------------------------------------------

/// 書き込みと実行が同時にできるページの保護値
const RWX: [&str; 2] = ["PAGE_EXECUTE_READWRITE", "PAGE_EXECUTE_WRITECOPY"];

/// 書いている間だけ RWX にして、finish_inline_write で元の保護に戻す、ただ一つの場所
pub(super) const RWX_HOME: &str = "crates/shirusu-hook/src/engine/inline_hooks.rs";

/// テストでない式の中の RWX (`use` での取り込みは数えない)
#[derive(Default)]
struct Rwx(Vec<(usize, String)>);

impl<'a> Visit<'a> for Rwx {
    fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
        if !is_cfg_test(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        if !is_cfg_test(&f.attrs) {
            syn::visit::visit_item_fn(self, f);
        }
    }
    fn visit_expr_path(&mut self, p: &'a syn::ExprPath) {
        if let Some(s) = p.path.segments.last() {
            let n = s.ident.to_string();
            if RWX.contains(&n.as_str()) {
                self.0.push((line(s.ident.span()), n));
            }
        }
        syn::visit::visit_expr_path(self, p);
    }
}

pub(super) fn rwx_pages(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in product_rs(tree, "crates/") {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        let mut v = Rwx::default();
        v.visit_file(&file);
        if f == RWX_HOME {
            // (ここでも一つだけ: パッチを書く間。戻す側が残っていること)
            if v.0.len() > 1 || (!v.0.is_empty() && !tree.text(&f).contains("finish_inline_write("))
            {
                out.extend(v.0.into_iter().map(|(l, w)| {
                    (
                        f.clone(),
                        l,
                        format!("{w}: パッチを書く一か所だけ、戻す finish_inline_write と一緒に"),
                    )
                }));
            }
            continue;
        }
        out.extend(v.0.into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}

// --- panic-unwind-kept --------------------------------------------------------------------------

/// Cargo.toml、.cargo の設定、CI のスクリプトとワークフローでの panic=abort
pub(super) fn panic_unwind_kept(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for p in tree.paths() {
        let toml = p.ends_with(".toml");
        let script = p.ends_with(".sh")
            || p.ends_with(".ps1")
            || (p.starts_with(".github/workflows/")
                && (p.ends_with(".yml") || p.ends_with(".yaml")));
        if !toml && !script {
            continue;
        }
        for (k, l) in tree.text(&p).lines().enumerate() {
            let code = l.split('#').next().unwrap_or(l);
            let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let bad = if toml {
                compact.contains("panic=\"abort\"") || compact.contains("panic=abort")
            } else {
                compact.contains("panic=abort") || compact.contains("panic_abort")
            };
            if bad {
                out.push((p.clone(), k.saturating_add(1), "panic=abort".into()));
            }
        }
    }
    out
}

// --- shell-special-var --------------------------------------------------------------------------

/// bash 自身の変数: 代入が捨てられる (GROUPS、ID)、すぐ置き換わる (RANDOM、SECONDS、LINENO..)、
/// シェルの動きが変わる (OPTIND、PWD)。IFS は `IFS=$'\t' read` が行の分け方なので外す
pub const BASH_SPECIAL: [&str; 31] = [
    "GROUPS",
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "LINENO",
    "BASHPID",
    "BASHOPTS",
    "SHELLOPTS",
    "EUID",
    "UID",
    "PPID",
    "FUNCNAME",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "HISTCMD",
    "DIRSTACK",
    "PIPESTATUS",
    "BASH_COMMAND",
    "BASH_SUBSHELL",
    "BASH_LINENO",
    "BASH_SOURCE",
    "BASH_ARGC",
    "BASH_ARGV",
    "BASH_VERSINFO",
    "HOSTTYPE",
    "OSTYPE",
    "MACHTYPE",
    "OPTIND",
    "OPTARG",
    "PWD",
    "OLDPWD",
];

/// PowerShell の自動変数: 代入しても PowerShell が上書きするか、読む側の意味が変わる
/// ($input はパイプラインの入力、$args は引数、$Host はホスト、$PID は自分のプロセス、
/// $Error は直前のエラー、$_ / $PSItem はいまの要素)
pub const PS_AUTOMATIC: [&str; 22] = [
    "input",
    "args",
    "host",
    "pid",
    "error",
    "psitem",
    "_",
    "this",
    "foreach",
    "switch",
    "myinvocation",
    "psboundparameters",
    "pscmdlet",
    "psscriptroot",
    "pscommandpath",
    "event",
    "eventargs",
    "sender",
    "home",
    "pwd",
    "executioncontext",
    "psversiontable",
];

/// `name=` (前が語の一部でも `$` でもない)。`name+=`、`name[i]=` も
fn sh_sets(code: &str) -> Option<String> {
    let b = code.as_bytes();
    for name in BASH_SPECIAL {
        let mut from = 0;
        while let Some(k) = code
            .get(from..)
            .and_then(|s| s.find(name))
            .map(|k| k.saturating_add(from))
        {
            from = k.saturating_add(1);
            let before = k.checked_sub(1).and_then(|i| b.get(i)).copied();
            if before.is_some_and(|c| {
                c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'.' | b'/' | b'-')
            }) {
                continue;
            }
            let mut rest = code.get(k.saturating_add(name.len())..).unwrap_or_default();
            if let Some(r) = rest.strip_prefix('[') {
                rest = r.split_once(']').map_or("", |(_, x)| x);
            }
            let rest = rest.strip_prefix('+').unwrap_or(rest);
            if rest.starts_with('=') {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// ワークフローの `env:` のキーとしての名前
fn yaml_key(code: &str) -> Option<String> {
    let t = code.trim_start();
    BASH_SPECIAL
        .iter()
        .find(|n| {
            t.strip_prefix(**n)
                .is_some_and(|r| r.trim_start().starts_with(':'))
        })
        .map(|n| (*n).to_owned())
}

/// `$name = ..`、`$name += ..`、`[type]$name` (param)、`foreach ($name in ..)`: 大文字小文字は問わない
fn ps_sets(code: &str) -> Option<String> {
    let low = code.to_ascii_lowercase();
    let b = low.as_bytes();
    for (k, _) in low.match_indices('$') {
        let name: String = low
            .get(k.saturating_add(1)..)
            .unwrap_or_default()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !PS_AUTOMATIC.contains(&name.as_str())
            || (k > 0 && b.get(k.saturating_sub(1)) == Some(&b'`'))
        {
            continue;
        }
        let after = low
            .get(k.saturating_add(1).saturating_add(name.len())..)
            .unwrap_or_default()
            .trim_start();
        let assigned = (after.starts_with('=') && !after.starts_with("=="))
            || ["+=", "-=", "*=", "/="]
                .iter()
                .any(|o| after.starts_with(o));
        let before = low.get(..k).unwrap_or_default().trim_end();
        // (`param([string]$Host)`: 宣言。`[int]$args[0]` の読み出しは後ろが `[`)
        let typed = before.ends_with(']')
            && (after.is_empty()
                || after.starts_with(',')
                || after.starts_with(')')
                || after.starts_with('='));
        let looped = (before.ends_with("foreach (") || before.ends_with("foreach("))
            && after.starts_with("in ");
        if assigned || typed || looped {
            return Some(format!("${name}"));
        }
    }
    None
}

/// `text` の中で特殊変数に代入する行: (行, 名前)。コメントは見ない
pub fn special_sets(text: &str, kind: Script) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (k, l) in text.lines().enumerate() {
        let code = l.split(" #").next().unwrap_or(l);
        if code.trim_start().starts_with('#') {
            continue;
        }
        let hit = match kind {
            Script::Bash => sh_sets(code),
            Script::PowerShell => ps_sets(code),
            // (ワークフロー: bash の手順と pwsh の手順が混ざる)
            Script::Workflow => sh_sets(code)
                .or_else(|| yaml_key(code))
                .or_else(|| ps_sets(code)),
        };
        if let Some(n) = hit {
            out.push((k.saturating_add(1), n));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    Bash,
    PowerShell,
    Workflow,
}

fn script_kind(p: &str) -> Option<Script> {
    if p.ends_with(".sh") {
        Some(Script::Bash)
    } else if p.ends_with(".ps1") || p.ends_with(".psm1") {
        Some(Script::PowerShell)
    } else if p.starts_with(".github/workflows/") && (p.ends_with(".yml") || p.ends_with(".yaml")) {
        Some(Script::Workflow)
    } else {
        None
    }
}

pub(super) fn shell_special_var(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for p in tree.paths() {
        let Some(kind) = script_kind(&p) else {
            continue;
        };
        out.extend(
            special_sets(&tree.text(&p), kind)
                .into_iter()
                .map(|(l, n)| (p.clone(), l, format!("{n} に代入している"))),
        );
    }
    out
}
