//! Rust 1.97..1.100 の機能で型に移した決まり (2026-11-21、こた「Rust 1.100 の新機能を全部
//! 入れて、変更箇所の退行がないように自作リンタも」)。
//!
//! - `wildcard-unreachable`: `_ => unreachable!(..)` の腕を書かない。来ない値は型で消す:
//!   never 型 `!` を型引数に入れる (`TypedValue<Generic>`: 来ない変種の中身が `!`)、番号の
//!   代わりに enum (`shared_com::Unknown`)、const 引数ごとの trait 実装 (`CallOriginal`)。
//! - `bin-dead-pub`: どのバイナリも `dead_code_pub_in_binary` (1.97) を crate 属性で有効にする
//!   (`#![cfg_attr(not(test), warn(dead_code_pub_in_binary))]`: テストは多くを使うので外す)。
//!   この lint は crate の階層でしか効かないので Cargo.toml でなく各 main に書く。
use syn::visit::Visit;

use super::{line, Hits, Tree};

/// 検査する Rust ファイル (fuzz と lint 自身の fixture 文字列は構文の外なので数えない)
fn rust(tree: &Tree) -> Vec<String> {
    tree.paths()
        .into_iter()
        .filter(|p| (p.starts_with("crates/") || p.starts_with("e2e/")) && p.ends_with(".rs"))
        .filter(|p| tree.has_or_real(p))
        .collect()
}

#[derive(Default)]
struct Wild(Vec<usize>);

/// `unreachable!` / `core::unreachable!` / `std::unreachable!` の呼び出しか
fn is_unreachable(e: &syn::Expr) -> bool {
    let m = match e {
        syn::Expr::Macro(m) => &m.mac,
        syn::Expr::Block(b) => match b.block.stmts.as_slice() {
            [syn::Stmt::Macro(m)] => &m.mac,
            [syn::Stmt::Expr(e, _)] => return is_unreachable(e),
            _ => return false,
        },
        _ => return false,
    };
    m.path
        .segments
        .last()
        .is_some_and(|s| s.ident == "unreachable")
}

impl<'a> Visit<'a> for Wild {
    fn visit_arm(&mut self, a: &'a syn::Arm) {
        if matches!(a.pat, syn::Pat::Wild(_)) && a.guard.is_none() && is_unreachable(&a.body) {
            self.0.push(line(a.fat_arrow_token.spans[0]));
        }
        syn::visit::visit_arm(self, a);
    }
}

pub(super) fn wildcard_unreachable(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for p in rust(tree) {
        let Ok(file) = syn::parse_file(&tree.text(&p)) else {
            continue;
        };
        let mut w = Wild::default();
        w.visit_file(&file);
        out.extend(w.0.into_iter().map(|l| {
            (
                p.clone(),
                l,
                "`_ => unreachable!()`: 来ない値は型で消す (`!` の型引数、enum、trait 実装)"
                    .to_owned(),
            )
        }));
    }
    out
}

/// バイナリの根 (src/main.rs と src/bin/*.rs)
fn is_bin_root(p: &str) -> bool {
    p.ends_with("/src/main.rs")
        || p.split_once("/src/bin/")
            .is_some_and(|(_, f)| !f.contains('/') && f.ends_with(".rs"))
}

/// crate 属性 `#![..]` のどれかが dead_code_pub_in_binary を warn / deny にするか
fn enables(file: &syn::File) -> bool {
    file.attrs
        .iter()
        .filter(|a| matches!(a.style, syn::AttrStyle::Inner(_)))
        .any(|a| {
            let mut words = Vec::new();
            super::words(quote::ToTokens::to_token_stream(&a.meta), &mut words);
            words.iter().any(|w| w == "dead_code_pub_in_binary")
                && words
                    .iter()
                    .any(|w| w == "warn" || w == "deny" || w == "forbid")
                && !words.iter().any(|w| w == "allow" || w == "expect")
        })
}

pub(super) fn bin_dead_pub(tree: &Tree) -> Hits {
    rust(tree)
        .into_iter()
        .filter(|p| is_bin_root(p))
        .filter(|p| syn::parse_file(&tree.text(p)).is_ok_and(|f| !enables(&f)))
        .map(|p| {
            (
                p,
                1,
                "`#![cfg_attr(not(test), warn(dead_code_pub_in_binary))]` がない".to_owned(),
            )
        })
        .collect()
}

/// 1.98..1.100 が名前をつけた形を手で書いたところ (行, 何を書くか)
#[derive(Default)]
struct HandStd(Vec<(usize, &'static str)>, Vec<String>);

/// the tuple structs of one `&AtomicBool` (a flag borrowed for one scope)
fn flag_holders(file: &syn::File) -> Vec<String> {
    file.items
        .iter()
        .filter_map(|i| match i {
            syn::Item::Struct(s) => match &s.fields {
                syn::Fields::Unnamed(f) if f.unnamed.len() == 1 => {
                    let ty = f
                        .unnamed
                        .first()
                        .map_or_default(|f| tokens(&f.ty).replace(' ', ""));
                    (ty.starts_with('&') && ty.ends_with("AtomicBool")).then(|| s.ident.to_string())
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn tokens(e: &impl quote::ToTokens) -> String {
    quote::ToTokens::to_token_stream(e).to_string()
}

/// `Ok(())` / `Err(..)` だけの block か
fn only_call(b: &syn::Block, name: &str, unit: bool) -> bool {
    let [syn::Stmt::Expr(syn::Expr::Call(c), None)] = b.stmts.as_slice() else {
        return false;
    };
    let syn::Expr::Path(p) = &*c.func else {
        return false;
    };
    p.path.is_ident(name)
        && c.args.len() == 1
        && (!unit || matches!(c.args.first(), Some(syn::Expr::Tuple(t)) if t.elems.is_empty()))
}

impl<'a> Visit<'a> for HandStd {
    fn visit_expr_assign(&mut self, e: &'a syn::ExprAssign) {
        if let syn::Expr::Unary(u) = &*e.right {
            if matches!(u.op, syn::UnOp::Not(_)) && tokens(&*u.expr) == tokens(&*e.left) {
                self.0.push((
                    line(e.eq_token.spans[0]),
                    "`x = !x` は `x.toggle()` (Rust 1.100)",
                ));
            }
        }
        syn::visit::visit_expr_assign(self, e);
    }
    fn visit_expr_method_call(&mut self, m: &'a syn::ExprMethodCall) {
        let at = line(m.method.span());
        let recv = &*m.receiver;
        let inner = |name: &str| match recv {
            syn::Expr::MethodCall(r) if r.method == name => Some(r),
            _ => None,
        };
        if m.method == "unwrap_or_default" && inner("map").is_some() {
            self.0.push((
                at,
                "`.map(f).unwrap_or_default()` は `.map_or_default(f)` (Rust 1.98)",
            ));
        }
        if m.method == "map_or"
            && m.args.first().is_some_and(|a| {
                matches!(
                    tokens(a).replace(' ', "").as_str(),
                    "Default::default()" | "String::new()" | "Vec::new()"
                )
            })
        {
            self.0.push((
                at,
                "`.map_or(Default::default(), f)` は `.map_or_default(f)` (Rust 1.98)",
            ));
        }
        if m.method == "unwrap_or" {
            if let (Some(s), Some(arg)) = (inner("strip_prefix"), m.args.first()) {
                if tokens(&*s.receiver) == tokens(arg) {
                    self.0.push((
                        at,
                        "`s.strip_prefix(p).unwrap_or(s)` は `s.trim_prefix(p)` (Rust 1.100)",
                    ));
                }
            }
        }
        if m.method == "collect"
            && m.turbofish
                .as_ref()
                .is_some_and(|t| tokens(t).replace(' ', "").starts_with("::<Vec<"))
        {
            if let Some(map) = inner("map") {
                let from_zero_to_len = match &*map.receiver {
                    syn::Expr::Paren(p) => match &*p.expr {
                        syn::Expr::Range(r) => {
                            r.start.as_ref().is_some_and(|s| tokens(&**s) == "0")
                                && r.end.as_ref().is_some_and(|e| {
                                    tokens(&**e).replace(' ', "").ends_with(".len()")
                                })
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if from_zero_to_len {
                    self.0.push((at, "`(0..v.len()).map(f).collect::<Vec<_>>()` は `Vec::from_fn(v.len(), f)` (Rust 1.100)"));
                }
            }
        }
        syn::visit::visit_expr_method_call(self, m);
    }
    fn visit_expr_if(&mut self, e: &'a syn::ExprIf) {
        if let Some((_, other)) = &e.else_branch {
            if let syn::Expr::Block(b) = &**other {
                if !matches!(&*e.cond, syn::Expr::Let(_))
                    && only_call(&e.then_branch, "Ok", true)
                    && only_call(&b.block, "Err", false)
                {
                    self.0.push((line(e.if_token.span), "`if c { Ok(()) } else { Err(e) }` は `c.ok_or(e)` / `c.ok_or_else(|| e)` (Rust 1.98)"));
                }
            }
        }
        syn::visit::visit_expr_if(self, e);
    }
    fn visit_item_impl(&mut self, i: &'a syn::ItemImpl) {
        // (1 本の `&AtomicBool` を持つだけの、落ちるときに旗を下ろす型: DropGuard (1.100) で書く)
        let holder = matches!(&*i.self_ty, syn::Type::Path(t) if t.path.segments.last().is_some_and(|s| self.1.iter().any(|n| s.ident == n)));
        if holder
            && i.trait_
                .as_ref()
                .is_some_and(|(_, p, _)| p.is_ident("Drop"))
        {
            if let Some(syn::ImplItem::Fn(f)) = i.items.first() {
                if let [syn::Stmt::Expr(syn::Expr::MethodCall(c), Some(_))] =
                    f.block.stmts.as_slice()
                {
                    if c.method == "store" && tokens(&*c.receiver).replace(' ', "") == "self.0" {
                        self.0.push((line(i.impl_token.span), "旗を下ろすだけの Drop 型は `std::mem::DropGuard::new(&flag, |f| f.store(..))` (Rust 1.100)"));
                    }
                }
            }
        }
        syn::visit::visit_item_impl(self, i);
    }
}

pub(super) fn hand_std(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    for p in rust(tree) {
        let Ok(file) = syn::parse_file(&tree.text(&p)) else {
            continue;
        };
        let mut v = HandStd(Vec::new(), flag_holders(&file));
        v.visit_file(&file);
        out.extend(v.0.into_iter().map(|(l, w)| (p.clone(), l, w.to_owned())));
    }
    out
}
