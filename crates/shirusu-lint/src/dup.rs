//! dup: the same code written twice (docs/lints.md).
//!
//! Every function (free, method, trait default, nested) and every long table (a const or static
//! array literal) is reduced to its tokens: the item's own name, its attributes and doc comments
//! left out. Two shapes are compared:
//! - `Exact`: the tokens as written (a copy pasted under another name);
//! - `Renamed`: local names (lower-case identifiers that are not called, not a field or method,
//!   not a path segment) replaced by their order of first use (a copy whose variables were renamed).
//!
//! A group of two or more items with the same shape, at least [`Kind::min_tokens`] long, is a
//! finding unless the known list (masu: tools/ci/dup-sites.tsv; shirusu: scripts/ci/dup-known.tsv;
//! kikizake: tools/lint/dup-known.tsv) has that group with its reason (a test's
//! independent oracle, a measured Windows model...). The known list only shrinks by itself:
//! `--update` drops the groups that are gone; a new one is added by hand, with its why.
//!
//! The same file is masu-lint's, shirusu-lint's and kikizake-lint's (it reads (path, text) pairs
//! and needs syn, proc-macro2 and quote only): a change goes to all three.
use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::Visit;

/// the source line a span starts on
fn line(span: proc_macro2::Span) -> usize {
    span.start().line
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Exact,
    Renamed,
    Table,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Exact, Kind::Renamed, Kind::Table];
    /// the shortest item that counts: below it, two items alike are a coincidence (a getter, a
    /// `new` of two fields), not a copy
    pub fn min_tokens(self) -> usize {
        match self {
            Kind::Exact => 60,
            Kind::Renamed => 90,
            Kind::Table => 48,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Kind::Exact => "exact",
            Kind::Renamed => "renamed",
            Kind::Table => "table",
        }
    }
    pub fn of_id(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.id() == s)
    }
}

/// one item of a group: its file and its name (`Type::method` for a method)
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Member {
    pub file: String,
    pub name: String,
}

impl std::fmt::Display for Member {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.file, self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Group {
    pub kind: Kind,
    /// sorted, two or more
    pub members: Vec<Member>,
    /// each member's line, in `members`' order
    pub lines: Vec<usize>,
    pub tokens: usize,
}

struct Unit {
    member: Member,
    line: usize,
    exact: Vec<String>,
    renamed: Option<Vec<String>>,
}

/// the groups in `files` ((path, text); a file that does not parse is the compiler's to report)
pub fn find(files: &[(String, String)]) -> Vec<Group> {
    let mut units = Vec::new();
    for (path, text) in files {
        if let Ok(file) = syn::parse_file(text) {
            let mut c = Collect { file: path.clone(), ty: Vec::new(), out: &mut units };
            c.visit_file(&file);
        }
    }
    let mut by: BTreeMap<(Kind, &[String]), Vec<&Unit>> = BTreeMap::new();
    for u in &units {
        match &u.renamed {
            Some(r) => {
                if u.exact.len() >= Kind::Exact.min_tokens() {
                    by.entry((Kind::Exact, &u.exact)).or_default().push(u);
                }
                if r.len() >= Kind::Renamed.min_tokens() {
                    by.entry((Kind::Renamed, r)).or_default().push(u);
                }
            }
            None => {
                if u.exact.len() >= Kind::Table.min_tokens() {
                    by.entry((Kind::Table, &u.exact)).or_default().push(u);
                }
            }
        }
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut exact_sets: BTreeSet<Vec<Member>> = BTreeSet::new();
    for kind in Kind::ALL {
        for ((k, toks), us) in &by {
            if *k != kind || us.len() < 2 {
                continue;
            }
            let mut ms: Vec<(Member, usize)> = us.iter().map(|u| (u.member.clone(), u.line)).collect();
            ms.sort();
            let members: Vec<Member> = ms.iter().map(|(m, _)| m.clone()).collect();
            // (a renamed group that is an exact group already says nothing more)
            if kind == Kind::Renamed && exact_sets.contains(&members) {
                continue;
            }
            if kind == Kind::Exact {
                exact_sets.insert(members.clone());
            }
            let lines = ms.iter().map(|(_, l)| *l).collect();
            groups.push(Group { kind, members, lines, tokens: toks.len() });
        }
    }
    groups.sort();
    groups
}

struct Collect<'u> {
    file: String,
    /// the impl/trait the visitor is in
    ty: Vec<String>,
    out: &'u mut Vec<Unit>,
}

impl Collect<'_> {
    fn push_fn(&mut self, name: &syn::Ident, sig: &syn::Signature, block: &syn::Block) {
        let mut sig = sig.clone();
        sig.ident = syn::Ident::new("_", name.span());
        let mut ts = sig.to_token_stream();
        block.to_tokens(&mut ts);
        let exact = flat(ts);
        let renamed = Some(renamed(&exact));
        let name = match self.ty.last() {
            Some(t) => format!("{t}::{name}"),
            None => name.to_string(),
        };
        self.out.push(Unit { member: Member { file: self.file.clone(), name }, line: line(sig.ident.span()), exact, renamed });
    }
    fn push_table(&mut self, name: &syn::Ident, ty: &syn::Type, expr: &syn::Expr) {
        let syn::Expr::Array(a) = expr else { return };
        if a.elems.len() < 8 {
            return;
        }
        let mut ts = ty.to_token_stream();
        expr.to_tokens(&mut ts);
        self.out.push(Unit { member: Member { file: self.file.clone(), name: name.to_string() }, line: line(name.span()), exact: flat(ts), renamed: None });
    }
}

impl<'a> Visit<'a> for Collect<'_> {
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        self.push_fn(&f.sig.ident, &f.sig, &f.block);
        syn::visit::visit_item_fn(self, f);
    }
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        self.push_fn(&f.sig.ident, &f.sig, &f.block);
        syn::visit::visit_impl_item_fn(self, f);
    }
    fn visit_trait_item_fn(&mut self, f: &'a syn::TraitItemFn) {
        if let Some(b) = &f.default {
            self.push_fn(&f.sig.ident, &f.sig, b);
        }
        syn::visit::visit_trait_item_fn(self, f);
    }
    fn visit_item_impl(&mut self, i: &'a syn::ItemImpl) {
        let t = i.self_ty.to_token_stream().to_string().replace(' ', "");
        let t = match &i.trait_ {
            Some((_, p, _)) => format!("<{t}as{}>", p.to_token_stream().to_string().replace(' ', "")),
            None => t,
        };
        self.ty.push(t);
        syn::visit::visit_item_impl(self, i);
        self.ty.pop();
    }
    fn visit_item_trait(&mut self, t: &'a syn::ItemTrait) {
        self.ty.push(t.ident.to_string());
        syn::visit::visit_item_trait(self, t);
        self.ty.pop();
    }
    fn visit_item_const(&mut self, c: &'a syn::ItemConst) {
        self.push_table(&c.ident, &c.ty, &c.expr);
        syn::visit::visit_item_const(self, c);
    }
    fn visit_item_static(&mut self, s: &'a syn::ItemStatic) {
        self.push_table(&s.ident, &s.ty, &s.expr);
        syn::visit::visit_item_static(self, s);
    }
}

/// the tokens one by one (a group as its delimiters around its tokens), doc attributes left out
pub fn flat(ts: TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    flat_into(ts, &mut out);
    out
}

fn flat_into(ts: TokenStream, out: &mut Vec<String>) {
    let all: Vec<TokenTree> = ts.into_iter().collect();
    let mut k = 0;
    while let Some(t) = all.get(k) {
        k = k.saturating_add(1);
        match t {
            TokenTree::Punct(p) if p.as_char() == '#' => {
                // `#[doc = ".."]` / `#![doc = ".."]`: a comment, not code
                let (bang, at) = match all.get(k) {
                    Some(TokenTree::Punct(b)) if b.as_char() == '!' => (true, k.saturating_add(1)),
                    _ => (false, k),
                };
                let doc = match all.get(at) {
                    Some(TokenTree::Group(g)) => g.delimiter() == Delimiter::Bracket && g.stream().into_iter().next().is_some_and(|f| f.to_string() == "doc"),
                    _ => false,
                };
                if doc {
                    k = at.saturating_add(1);
                    continue;
                }
                out.push("#".into());
                if bang {
                    out.push("!".into());
                    k = at;
                }
            }
            TokenTree::Group(g) => {
                let (o, c) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                if !o.is_empty() {
                    out.push(o.into());
                }
                flat_into(g.stream(), out);
                if !c.is_empty() {
                    out.push(c.into());
                }
            }
            TokenTree::Ident(i) => out.push(i.to_string()),
            TokenTree::Punct(p) => out.push(p.as_char().to_string()),
            TokenTree::Literal(l) => out.push(l.to_string()),
        }
    }
}

/// `toks` with local names numbered by first use: a lower-case identifier that is not a keyword,
/// not called (`f(`), not a macro (`m!`), not after `.` (a field or method), not beside `::`
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "bool", "char", "str",
];

pub fn renamed(toks: &[String]) -> Vec<String> {
    let mut names: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(toks.len());
    for (k, t) in toks.iter().enumerate() {
        let prev = k.checked_sub(1).and_then(|p| toks.get(p)).map(String::as_str);
        let prev2 = k.checked_sub(2).and_then(|p| toks.get(p)).map(String::as_str);
        let next = toks.get(k.saturating_add(1)).map(String::as_str);
        let next2 = toks.get(k.saturating_add(2)).map(String::as_str);
        let local = t.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !KEYWORDS.contains(&t.as_str())
            && !matches!(next, Some("(" | "!"))
            && prev != Some(".")
            && !(prev == Some(":") && prev2 == Some(":"))
            && !(next == Some(":") && next2 == Some(":"));
        if local {
            let n = names.len();
            let n = *names.entry(t.as_str()).or_insert(n);
            out.push(format!("${n}"));
        } else {
            out.push(t.clone());
        }
    }
    out
}

/// a known group: its kind and members, as dup-sites.tsv writes them
pub type Known = (Kind, Vec<Member>);

/// dup-sites.tsv: `kind<TAB>member<TAB>member..<TAB># why` per line; `#` lines and blank lines
/// are comments
pub fn load_known(text: &str) -> Vec<Known> {
    let mut out = Vec::new();
    for l in text.lines() {
        if l.trim().is_empty() || l.starts_with('#') {
            continue;
        }
        let mut parts = l.split('\t');
        let Some(kind) = parts.next().and_then(Kind::of_id) else { continue };
        let mut members: Vec<Member> = parts
            .take_while(|p| !p.starts_with('#'))
            .filter_map(|p| p.split_once('#').map(|(f, n)| Member { file: f.to_owned(), name: n.to_owned() }))
            .collect();
        members.sort();
        out.push((kind, members));
    }
    out
}

/// the groups the known list does not have (a known group still matches when members left it)
pub fn fresh<'g>(found: &'g [Group], known: &[Known]) -> Vec<&'g Group> {
    found
        .iter()
        .filter(|g| !known.iter().any(|(k, ms)| *k == g.kind && g.members.iter().all(|m| ms.contains(m))))
        .collect()
}

/// a group's line for dup-sites.tsv (the why left for the person adding it)
pub fn line_of(g: &Group, why: &str) -> String {
    let ms: Vec<String> = g.members.iter().map(Member::to_string).collect();
    format!("{}\t{}\t# {why}", g.kind.id(), ms.join("\t"))
}

/// the known list with the lines of gone groups left out (a group is gone when fewer than two
/// of its members are still a group of that kind); comments and other lines stay
pub fn updated(text: &str, found: &[Group]) -> String {
    text.lines()
        .filter(|l| match load_known(l).first() {
            None => true,
            Some((k, ms)) => found.iter().any(|g| g.kind == *k && g.members.iter().filter(|m| ms.contains(m)).count() >= 2),
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

/// the findings as the lint prints them: what is copied where, and what to do
pub fn report(new: &[&Group], sites: &str) -> String {
    let mut out = String::new();
    for g in new {
        let at: Vec<String> = g.members.iter().zip(&g.lines).map(|(m, l)| format!("{}:{l} ({})", m.file, m.name)).collect();
        out.push_str(&format!(
            "dup: {} copy, {} tokens:\n    {}\n    (keep one and call it from the others; an intended copy goes in {sites} as this line, its why filled in:)\n    {}\n",
            g.kind.id(),
            g.tokens,
            at.join("\n    "),
            line_of(g, "why")
        ));
    }
    out
}

/// every group, known or not (`--list`)
pub fn list(found: &[Group]) -> String {
    let mut out = String::new();
    for g in found {
        let at: Vec<String> = g.members.iter().zip(&g.lines).map(|(m, l)| format!("{}:{l} {}", m.file, m.name)).collect();
        out.push_str(&format!("{} {} tokens\n    {}\n", g.kind.id(), g.tokens, at.join("\n    ")));
    }
    out
}

#[cfg(test)]
#[path = "dup_tests.rs"]
mod tests;
