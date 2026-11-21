//! 7c5a553 レビュー (2026-11-21) で直した「結果の一部だけを見る」書き方を戻さない規則。
//! 型で防げた分 (SlotClaim、ReaderRegistration、BatchWriteError、IoOutcome、Verdict) は型にした。
//! ここに残るのは、型があっても書けてしまう形。
use quote::ToTokens;
use syn::visit::Visit;

use super::regress::{is_test_only, product_rs};
use super::{line, Hits, Tree};

// --- blob-ref-walk ------------------------------------------------------------------------------

/// イベントが参照する Blob を一か所で列挙する場所 (`TraceEvent::blob_refs`)
pub(super) const BLOB_REFS_HOME: &str = "crates/shirusu-trace/src/refs.rs";

/// Blob を指す二つの場所: Payload と、引数の BlobRef 値
const BLOB_SITES: [&str; 2] = ["PayloadRef :: Blob", "TypedValue :: BlobRef"];

fn names_blob_site(tokens: &str) -> Option<&'static str> {
    BLOB_SITES.iter().copied().find(|s| tokens.contains(s))
}

/// 一つの変種だけを拾う書き方 (`if let`、`let .. else`、`matches!`、`_` の腕を持つ `match`)
#[derive(Default)]
struct BlobPick(Vec<(usize, String)>);

impl BlobPick {
    fn pattern(&mut self, pat: &syn::Pat, span: proc_macro2::Span, how: &str) {
        if let Some(site) = names_blob_site(&pat.to_token_stream().to_string()) {
            self.0.push((line(span), format!("{how} {}", site.replace(' ', ""))));
        }
    }
}

impl<'a> Visit<'a> for BlobPick {
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
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        if !is_test_only(&f.attrs) {
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
    fn visit_expr_let(&mut self, e: &'a syn::ExprLet) {
        self.pattern(&e.pat, e.let_token.span, "if let");
        syn::visit::visit_expr_let(self, e);
    }
    fn visit_local(&mut self, l: &'a syn::Local) {
        if l.init.as_ref().is_some_and(|i| i.diverge.is_some()) {
            self.pattern(&l.pat, l.let_token.span, "let .. else");
        }
        syn::visit::visit_local(self, l);
    }
    fn visit_expr_match(&mut self, m: &'a syn::ExprMatch) {
        let wild = m.arms.iter().any(|a| matches!(a.pat, syn::Pat::Wild(_)));
        if wild {
            for a in &m.arms {
                self.pattern(&a.pat, m.match_token.span, "match with `_`:");
            }
        }
        syn::visit::visit_expr_match(self, m);
    }
    fn visit_macro(&mut self, m: &'a syn::Macro) {
        if m.path.is_ident("matches") {
            if let Some(site) = names_blob_site(&m.tokens.to_string()) {
                let at = m.path.segments.first().map_or_else(proc_macro2::Span::call_site, |s| s.ident.span());
                self.0.push((line(at), format!("matches! {}", site.replace(' ', ""))));
            }
        }
        syn::visit::visit_macro(self, m);
    }
}

/// P1-1: repair は Payload の BlobRef だけを見て、引数の BlobRef が指す Blob を落とした。同じ形が
/// reader / writer / analysis / conformance / probe にもあった。参照の列挙は
/// `TraceEvent::blob_refs` (refs.rs) だけが行う。各変種を扱う `match` (符号化・表示・解決) は
/// 一つを拾う書き方ではないので数えない。
pub(super) fn blob_ref_walk(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in product_rs(tree, "crates/").into_iter().filter(|f| f != BLOB_REFS_HOME) {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        let mut v = BlobPick::default();
        v.visit_file(&file);
        out.extend(v.0.into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}

// --- compare-verdict ----------------------------------------------------------------------------

/// 一致の判定を下す、ただ一つの場所 (`DiffReport::verdict`)
pub(super) const VERDICT_HOME: &str = "crates/shirusu-compare/src/diff.rs";

#[derive(Default)]
struct CandidatesEmpty(Vec<(usize, String)>);

impl<'a> Visit<'a> for CandidatesEmpty {
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
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        if !is_test_only(&f.attrs) {
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
    fn visit_expr_method_call(&mut self, m: &'a syn::ExprMethodCall) {
        let on_candidates = matches!(&*m.receiver, syn::Expr::Field(f)
            if matches!(&f.member, syn::Member::Named(n) if n == "candidates"));
        if on_candidates && m.method == "is_empty" && m.args.is_empty() {
            self.0.push((line(m.method.span()), format!("candidates.{}()", m.method)));
        }
        syn::visit::visit_expr_method_call(self, m);
    }
}

/// P1-2: 候補が空なら一致とした判定は、読み手が検出した破損 (回復、欠けた Blob、終了しない
/// 記録) を見落とした。一致かどうかは `DiffReport::verdict()` (Identical / ObservedPrefixAgrees /
/// Differs) で決め、候補の数から決めない。候補を数える場所は diff.rs の verdict だけ。
pub(super) fn compare_verdict(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in product_rs(tree, "crates/").into_iter().filter(|f| f != VERDICT_HOME) {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        let mut v = CandidatesEmpty::default();
        v.visit_file(&file);
        out.extend(v.0.into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}
