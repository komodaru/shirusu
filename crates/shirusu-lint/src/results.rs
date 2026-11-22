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

// --- d4fc3c0 レビュー (2026-11-22) -------------------------------------------------------------
// 型で防げた分 (LastErrorRule、ValueBody / CapturedValue::read_value、Borrowed の値列、
// ReserveReaderError、借用する SlotClaim、LossCount、Use / gaps_for) は型にした。ここに残るのは、
// 型があっても書けてしまう形。

/// 本番コード (テスト・kani の中を除く) で `visit` を回す
struct ProductOnly<V>(V);

macro_rules! product_only {
    ($t:ty) => {
        impl<'a> Visit<'a> for ProductOnly<$t> {
            fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
                if !is_test_only(&m.attrs) {
                    syn::visit::visit_item_mod(self, m);
                }
            }
            fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
                if !is_test_only(&f.attrs) {
                    self.0.enter_fn(&f.sig.ident.to_string());
                    syn::visit::visit_item_fn(self, f);
                }
            }
            fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
                if !is_test_only(&f.attrs) {
                    self.0.enter_fn(&f.sig.ident.to_string());
                    syn::visit::visit_impl_item_fn(self, f);
                }
            }
            fn visit_pat_or(&mut self, p: &'a syn::PatOr) {
                self.0.pat_or(p);
                syn::visit::visit_pat_or(self, p);
            }
            fn visit_expr(&mut self, e: &'a syn::Expr) {
                self.0.expr(e);
                syn::visit::visit_expr(self, e);
            }
        }
    };
}

/// 規則ごとの見方
trait Look {
    fn enter_fn(&mut self, _name: &str) {}
    fn pat_or(&mut self, _p: &syn::PatOr) {}
    fn expr(&mut self, _e: &syn::Expr) {}
}

fn run<L: Look>(tree: &Tree, files: Vec<String>, make: impl Fn() -> L, take: impl Fn(L) -> Vec<(usize, String)>) -> Hits
where
    ProductOnly<L>: for<'a> Visit<'a>,
{
    let mut out = Hits::new();
    for f in files {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        let mut v = ProductOnly(make());
        v.visit_file(&file);
        out.extend(take(v.0).into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}

fn last_segment(p: &syn::Pat) -> Option<String> {
    let path = match p {
        syn::Pat::Path(p) => &p.path,
        syn::Pat::TupleStruct(p) => &p.path,
        syn::Pat::Struct(p) => &p.path,
        syn::Pat::Ident(i) => return Some(i.ident.to_string()),
        _ => return None,
    };
    path.segments.last().map(|s| s.ident.to_string())
}

// --- never-needs-no-return ----------------------------------------------------------------------

#[derive(Default)]
struct NeverWithReturn(Vec<(usize, String)>);

impl Look for NeverWithReturn {
    fn pat_or(&mut self, p: &syn::PatOr) {
        let names: Vec<String> = p.cases.iter().filter_map(last_segment).collect();
        let never = names.iter().any(|n| n == "Never");
        let on_return = names.iter().find(|n| n.starts_with("WhenReturnIs"));
        if let (true, Some(r)) = (never, on_return) {
            let at = p.cases.first().map_or_else(proc_macro2::Span::call_site, |c| {
                syn::spanned::Spanned::span(c)
            });
            self.0.push((line(at), format!("Never と {r} を同じ腕に置いている")));
        }
    }
}
product_only!(NeverWithReturn);

/// d4fc3c0 レビュー P1: `LastErrorMeaning::Never` を戻り値で決まる条件と同じ腕に置き、戻り値の
/// ない Sleep などの正しい Stale を不完全と数えた (期待値を作るテストも同じ腕を写した)。Never は
/// 戻り値を要らない (`LastErrorMeaning::rule` が `Fixed` に分ける)。テストのコードも見る
pub(super) fn never_needs_no_return(tree: &Tree) -> Hits {
    let files: Vec<String> = tree
        .paths()
        .into_iter()
        .filter(|p| p.starts_with("crates/") && p.ends_with(".rs") && !p.starts_with("crates/shirusu-lint/"))
        .collect();
    let mut out = Hits::new();
    for f in files {
        let Ok(file) = syn::parse_file(&tree.text(&f)) else {
            continue;
        };
        // テストの中も見る (期待値側に同じ誤りを写さない)
        struct All(NeverWithReturn);
        impl<'a> Visit<'a> for All {
            fn visit_pat_or(&mut self, p: &'a syn::PatOr) {
                self.0.pat_or(p);
                syn::visit::visit_pat_or(self, p);
            }
        }
        let mut v = All(NeverWithReturn::default());
        v.visit_file(&file);
        out.extend(v.0 .0.into_iter().map(|(l, w)| (f.clone(), l, w)));
    }
    out
}

// --- integrity-gate -----------------------------------------------------------------------------

/// 完全性の集計 (IntegritySummary) を判定する、ただ一つの場所
pub(super) const INTEGRITY_HOME: &str = "crates/shirusu-analysis/src/view.rs";

/// IntegritySummary の欠けの材料になるフィールド
const INTEGRITY_FIELDS: [&str; 12] = [
    "has_finalization",
    "finalized_cleanly",
    "event_count_mismatch",
    "capture_coverage",
    "event_integrity",
    "semantic_completeness",
    "loss_records_by_kind",
    "dropped_payload_events",
    "degraded_events",
    "recovery_issues",
    "missing_blob_refs",
    "inconsistent_calls",
];

/// `a || b || ..` の項
fn or_terms<'e>(e: &'e syn::Expr, out: &mut Vec<&'e syn::Expr>) {
    match e {
        syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::Or(_)) => {
            or_terms(&b.left, out);
            or_terms(&b.right, out);
        }
        syn::Expr::Paren(p) => or_terms(&p.expr, out),
        other => out.push(other),
    }
}

/// 項が IntegritySummary のフィールドを見ているか
fn names_integrity_field(e: &syn::Expr) -> bool {
    struct Fields(bool);
    impl<'a> Visit<'a> for Fields {
        fn visit_expr_field(&mut self, f: &'a syn::ExprField) {
            if let syn::Member::Named(n) = &f.member {
                self.0 |= INTEGRITY_FIELDS.iter().any(|x| n == x);
            }
            syn::visit::visit_expr_field(self, f);
        }
    }
    let mut v = Fields(false);
    v.visit_expr(e);
    v.0
}

#[derive(Default)]
struct HandGate(Vec<(usize, String)>, std::collections::BTreeSet<usize>);

impl Look for HandGate {
    fn expr(&mut self, e: &syn::Expr) {
        let syn::Expr::Binary(b) = e else {
            return;
        };
        if !matches!(b.op, syn::BinOp::Or(_)) {
            return;
        }
        let mut terms = Vec::new();
        or_terms(e, &mut terms);
        let n = terms.iter().filter(|t| names_integrity_field(t)).count();
        let at = line(syn::spanned::Spanned::span(&b.op));
        // 同じ `||` の連なりの内側の二項を二度数えない
        if n >= 2 && self.1.insert(at) {
            self.0.push((at, format!("完全性の項目を {n} 個、手で並べた条件")));
        }
    }
}
product_only!(HandGate);

/// d4fc3c0 レビュー 1.4: Probe の replay と差分判定は完全性の条件を `||` で手で並べ、
/// event_count_mismatch を落とした。判定は `IntegritySummary::incompleteness` / `gaps_for(Use)` /
/// `require(Use)` で行い、項目を並べるのは view.rs だけ
pub(super) fn integrity_gate(tree: &Tree) -> Hits {
    let files = product_rs(tree, "crates/").into_iter().filter(|f| f != INTEGRITY_HOME).collect();
    run(tree, files, HandGate::default, |v| v.0)
}

// --- blob-ref-body ------------------------------------------------------------------------------

#[derive(Default)]
struct RawBody(Vec<(usize, String)>);

impl Look for RawBody {
    fn expr(&mut self, e: &syn::Expr) {
        let (scrutinee, pats): (&syn::Expr, Vec<&syn::Pat>) = match e {
            syn::Expr::Match(m) => (&m.expr, m.arms.iter().map(|a| &a.pat).collect()),
            syn::Expr::Let(l) => (&l.expr, vec![&*l.pat]),
            _ => return,
        };
        let names_blob = pats
            .iter()
            .any(|p| p.to_token_stream().to_string().contains("TypedValue :: BlobRef"));
        if !names_blob {
            return;
        }
        let through_read_value = matches!(scrutinee, syn::Expr::MethodCall(m) if m.method == "read_value");
        if !through_read_value {
            let at = match e {
                syn::Expr::Match(m) => m.match_token.span,
                syn::Expr::Let(l) => l.let_token.span,
                _ => proc_macro2::Span::call_site(),
            };
            self.0.push((line(at), "TypedValue::BlobRef を read_value() を通さずに読む".into()));
        }
    }
}
product_only!(RawBody);

/// d4fc3c0 レビュー 1.3: Blob 参照の列挙が取得状態を見ず、NotApplicable のダミー (ゼロの Blob ID) を
/// 参照と数えた。refs.rs は引数の BlobRef を `CapturedValue::read_value()` (ValueBody::Dummy なら
/// None) の結果でだけ読み、所有権付きの `decode_values` で複製しない (同レビュー 4)
pub(super) fn blob_ref_body(tree: &Tree) -> Hits {
    let mut out = run(tree, vec![BLOB_REFS_HOME.to_string()], RawBody::default, |v| v.0);
    for (i, l) in tree.text(BLOB_REFS_HOME).lines().enumerate() {
        if l.contains("decode_values") && !l.trim_start().starts_with("//") {
            out.push((BLOB_REFS_HOME.to_string(), i + 1, "decode_values で引数を複製している (values_ref で借りて読む)".into()));
        }
    }
    out
}

// --- value-reader -------------------------------------------------------------------------------

/// 値本体を読む、ただ一つの関数
pub(super) const VALUE_READER: (&str, &str) = ("crates/shirusu-trace/src/compact.rs", "read_value_ref");

#[derive(Default)]
struct SecondReader {
    current: String,
    hits: Vec<(usize, String)>,
}

impl Look for SecondReader {
    fn enter_fn(&mut self, name: &str) {
        self.current = name.to_string();
    }
    fn expr(&mut self, e: &syn::Expr) {
        let syn::Expr::Match(m) = e else {
            return;
        };
        let arms: Vec<String> = m.arms.iter().map(|a| a.pat.to_token_stream().to_string()).collect();
        let over_ty = arms.iter().any(|a| a.contains("Ty :: Ptr64")) && arms.iter().any(|a| a.contains("Ty :: Str16"));
        let reads = m
            .arms
            .iter()
            .any(|a| a.body.to_token_stream().to_string().contains(". read_"));
        if over_ty && reads && self.current != VALUE_READER.1 {
            self.hits.push((line(m.match_token.span), format!("{} の外で値本体を型ごとに読んでいる", VALUE_READER.1)));
        }
    }
}
product_only!(SecondReader);

/// d4fc3c0 レビュー 2.1 / 4: 値列の検証が復号と別に値を読み、ポインタの戻り値を片方だけが結果情報から
/// 落とした。値本体を型ごとに読むのは compact.rs の read_value_ref だけ (復号・検証・結果の判定・
/// 参照の列挙は values_ref を通す)
pub(super) fn value_reader(tree: &Tree) -> Hits {
    run(tree, product_rs(tree, "crates/"), SecondReader::default, |v| v.hits)
}

// --- last-error-inputs-home ---------------------------------------------------------------------

/// `resolve_inputs` を呼んでよい場所: 規則そのもの (schema.rs) と取得側 (hook の capture.rs)
const RESOLVE_INPUTS_CALLERS: [&str; 2] = ["crates/shirusu-trace/src/schema.rs", "crates/shirusu-hook/src/capture.rs"];

/// d4fc3c0 レビュー 1.6: 入力で決まる LastError の照合が conformance にしかなく、完全性の判定・
/// no-integrity-loss・比較は誤った Stale を受理した。照合は `LastErrorMeaning::check_stale_inputs`
/// (shirusu-analysis の CallChecks が全体読み込みとストリーミング読みで通す) だけで行う
pub(super) fn last_error_inputs_home(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in product_rs(tree, "crates/")
        .into_iter()
        .filter(|f| !RESOLVE_INPUTS_CALLERS.contains(&f.as_str()) && !f.starts_with("crates/shirusu-lint/"))
    {
        for (i, l) in tree.text(&f).lines().enumerate() {
            if l.contains("resolve_inputs(") && !l.trim_start().starts_with("//") {
                out.push((f.clone(), i + 1, "resolve_inputs を直接呼んでいる (check_stale_inputs を使う)".into()));
            }
        }
    }
    out
}

// --- storage-full-count -------------------------------------------------------------------------

/// d4fc3c0 レビュー 1.5: 容量不足の後に届いたバッチの全件 (`events.len()`) を失ったと数え、
/// Filtered で間引くはずのイベントまで数えた。数えるのは記録モードの選択規則
/// (`TraceRecorder::discard_unwritable`) だけ
pub(super) fn storage_full_count(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for f in product_rs(tree, "crates/shirusu-host/src/") {
        for (i, l) in tree.text(&f).lines().enumerate() {
            let code = l.split("//").next().unwrap_or("");
            if code.contains("dropped_events") && code.contains("+=") && code.contains(".len()") {
                out.push((f.clone(), i + 1, "失ったイベントをバッチの件数で数えている".into()));
            }
        }
    }
    out
}
