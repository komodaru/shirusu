//! 逃げのコメントの禁止 (ユーザーの決まり 2026-11-20): 正しい動作を実装する代わりに、
//! コメントへ近道を書いて済ませない。to-do の印、"for now"、簡略化、"not implemented"、
//! 何でも答えるスタブ、近似、best effort (英語と日本語) をコメントに書かない。
//! "not supported" / "unsupported" / 近似 / 無視する引数は、同じコメントに Windows の
//! 根拠 (実測、golden、winref、仕様、Wine のテスト) があるときだけ通す (それは Windows の
//! 答えで、抜けではない)。Rust のテストでないコードでは文字列リテラル (ログ) も見る。
//!
//! 既にある箇所は scripts/ci/escape-known.txt に「直すもの」として並べ、減る方向にだけ動く。

use crate::{Hits, Tree};

pub(crate) const KNOWN: &str = "scripts/ci/escape-known.txt";

/// どのコメントにあっても逃げの語 (大文字小文字をそのまま、単語として)
const ALWAYS_EXACT: [&str; 7] = [
    concat!("TO", "DO"),
    concat!("FIX", "ME"),
    concat!("XX", "X"),
    concat!("HA", "CK"),
    "simplified",
    "simplification",
    "simplify",
];

/// どのコメントにあっても逃げの語 (小文字にし、空白を 1 つにして、単語として)
const ALWAYS_FOLDED: [&str; 25] = [
    "for now",
    "for the moment",
    "in the meantime",
    "not implemented",
    "not yet implemented",
    "unimplemented",
    "not ported",
    "not modeled",
    "not modelled",
    "not modeled yet",
    "not modelled yet",
    "best effort",
    "best-effort",
    "good enough",
    "close enough",
    "stubbed",
    "logged stub",
    "stub returning",
    "placeholder value",
    "placeholder answer",
    "workaround",
    "hacky",
    "fake success",
    "fake answer",
    "simpler version",
];

/// どのコメントにあっても逃げの語 (日本語: 語の切れ目を見ない)
const ALWAYS_JA: [&str; 11] = [
    "とりあえず",
    "簡略",
    "未実装",
    "未対応",
    "暫定",
    "ひとまず",
    "手抜き",
    "近似",
    "仮実装",
    "後で実装",
    "ダミー実装",
];

/// Windows の根拠が同じコメントに無ければ逃げの語 (大文字小文字をそのまま)
const UNLESS_EXACT: [&str; 4] = [
    "not supported",
    "not handled",
    "unsupported",
    "ignored flags",
];

/// Windows の根拠が同じコメントに無ければ逃げの語 (小文字にして、語の始まりとして)
const UNLESS_PREFIX: [&str; 1] = ["approximat"];

/// それを Windows の答えにする根拠
const EVIDENCE: [&str; 17] = [
    "Windows",
    "winref",
    "golden",
    "MEASURED",
    "measured",
    "Wine's test",
    "as on ",
    "the spec",
    "documented",
    "MSDN",
    "learn.microsoft",
    "x86",
    "x87",
    "CPU",
    "実測",
    "仕様",
    "Wine のテスト",
];

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `s` に `w` が単語として (前後が英数字でなく) あるか; `prefix` なら後ろは見ない
fn has_word(s: &str, w: &str, prefix: bool) -> bool {
    let mut from = 0;
    while let Some(k) = s.get(from..).and_then(|r| r.find(w)) {
        let at = from.saturating_add(k);
        let end = at.saturating_add(w.len());
        let before = s.get(..at).and_then(|b| b.chars().next_back());
        let after = s.get(end..).and_then(|a| a.chars().next());
        let starts = w.chars().next().is_none_or(|c| !is_word_char(c))
            || before.is_none_or(|c| !is_word_char(c));
        let ends = prefix
            || w.chars().next_back().is_none_or(|c| !is_word_char(c))
            || after.is_none_or(|c| !is_word_char(c));
        if starts && ends {
            return true;
        }
        from = end;
    }
    false
}

fn folded(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 根拠を問わず逃げの語があるか
pub(crate) fn is_escape_always(s: &str) -> bool {
    let f = folded(s);
    ALWAYS_EXACT.iter().any(|w| has_word(s, w, false))
        || ALWAYS_FOLDED.iter().any(|w| has_word(&f, w, false))
        || ALWAYS_JA.iter().any(|w| s.contains(w))
}

/// コメント 1 行 `s` が逃げか (`block`: それを含むコメントのかたまり、根拠を探す所)
pub(crate) fn is_escape(s: &str, block: &str) -> bool {
    let f = folded(s);
    let always = is_escape_always(s);
    let unless = UNLESS_EXACT.iter().any(|w| has_word(s, w, false))
        || UNLESS_PREFIX.iter().any(|w| has_word(&f, w, true));
    always || (unless && !EVIDENCE.iter().any(|e| block.contains(e)))
}

const SLASH: [&str; 7] = [".rs", ".c", ".h", ".cpp", ".js", ".ts", ".mjs"];
const HASH: [&str; 6] = [".py", ".sh", ".ps1", ".yml", ".yaml", ".toml"];

/// 行ごとのコメント (無い行は空)、と Rust では文字列リテラル
pub(crate) fn comments(path: &str, text: &str) -> (Vec<String>, Vec<String>) {
    let mut cs = Vec::new();
    let mut ss = Vec::new();
    if SLASH.iter().any(|e| path.ends_with(e)) {
        let (mut in_block, mut in_str) = (false, false);
        for l in text.split('\n') {
            let (mut c, mut s) = (String::new(), String::new());
            let ch: Vec<char> = l.chars().collect();
            let mut i = 0;
            while let Some(&x) = ch.get(i) {
                let next = ch.get(i.saturating_add(1)).copied();
                if in_block {
                    if x == '*' && next == Some('/') {
                        in_block = false;
                        i = i.saturating_add(2);
                        continue;
                    }
                    c.push(x);
                } else if in_str {
                    if x == '\\' {
                        s.push(x);
                        if let Some(n) = next {
                            s.push(n);
                        }
                        i = i.saturating_add(2);
                        continue;
                    }
                    if x == '"' {
                        in_str = false;
                    }
                    s.push(x);
                } else if x == '/' && next == Some('/') {
                    c.extend(ch.get(i..).unwrap_or_default());
                    break;
                } else if x == '/' && next == Some('*') {
                    in_block = true;
                    c.push_str("/*");
                    i = i.saturating_add(2);
                    continue;
                } else if x == '"' && !(i > 0 && ch.get(i.saturating_sub(1)) == Some(&'r')) {
                    in_str = true;
                    s.push(x);
                } else if x == '\'' && ch.get(i.saturating_add(2)) == Some(&'\'') {
                    // ('"' のような文字リテラル)
                    i = i.saturating_add(3);
                    continue;
                }
                i = i.saturating_add(1);
            }
            cs.push(c.trim().to_owned());
            ss.push(s.trim().to_owned());
        }
    } else if HASH.iter().any(|e| path.ends_with(e)) {
        for l in text.split('\n') {
            cs.push(hash_comment(l).unwrap_or_default().trim().to_owned());
            ss.push(String::new());
        }
    }
    (cs, ss)
}

/// 引用符の外の `#` コメント (`#!` と `$#`、`${#x}` は別)
fn hash_comment(l: &str) -> Option<&str> {
    let (mut q, mut prev) = (None::<char>, ' ');
    for (i, ch) in l.char_indices() {
        match q {
            Some(o) if ch == o && prev != '\\' => q = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => q = Some(ch),
            None if ch == '#'
                && (prev.is_whitespace() || i == 0)
                && !l.get(i..).is_some_and(|r| r.starts_with("#!")) =>
            {
                return l.get(i..)
            }
            None => {}
        }
        prev = ch;
    }
    None
}

fn is_test_file(p: &str) -> bool {
    p.contains("/tests/")
        || p.starts_with("tests/")
        || p.ends_with("tests.rs")
        || p.ends_with("_test.rs")
}

/// 語彙そのものと、その自己テスト
fn exempt(p: &str) -> bool {
    p.starts_with("crates/shirusu-lint/") || p.starts_with("vendor/") || p.contains("/testdata/")
}

/// 既知の箇所: (ファイル, コメントの文字列) -> 何件
fn known(tree: &Tree) -> std::collections::BTreeMap<(String, String), usize> {
    let mut m = std::collections::BTreeMap::new();
    for l in tree
        .text(KNOWN)
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let mut c = l.splitn(3, '\t');
        if let (Some(f), Some(t)) = (c.next(), c.next()) {
            let e: &mut usize = m.entry((f.to_owned(), folded(t))).or_default();
            *e = e.saturating_add(1);
        }
    }
    m
}

pub(crate) fn escape_comment(tree: &Tree) -> Hits {
    let mut out = Hits::new();
    let mut left = known(tree);
    for p in tree.paths() {
        if exempt(&p) || !tree.has_or_real(&p) {
            continue;
        }
        let (cs, ss) = comments(&p, &tree.text(&p));
        let mut found: Vec<(usize, String)> = Vec::new();
        let mut k = 0;
        while k < cs.len() {
            let mut e = k;
            while cs.get(e).is_some_and(|l| !l.is_empty()) {
                e = e.saturating_add(1);
            }
            let block = cs.get(k..e).map_or_default(|b| b.join(" "));
            for (i, l) in cs.iter().enumerate().take(e).skip(k) {
                if is_escape(l, &block) {
                    found.push((i.saturating_add(1), l.clone()));
                }
            }
            k = e.saturating_add(1);
        }
        if p.ends_with(".rs") && !is_test_file(&p) {
            for (i, s) in ss.iter().enumerate() {
                // (文字列は逃げの語だけを見る: Unsupported は記録の値の種類の名前でもある)
                if !s.is_empty() && is_escape_always(s) {
                    found.push((i.saturating_add(1), s.clone()));
                }
            }
        }
        for (line, what) in found {
            let key = (p.clone(), folded(&what));
            match left.get_mut(&key) {
                Some(n) if *n > 0 => *n = n.saturating_sub(1),
                _ => out.push((p.clone(), line, what)),
            }
        }
    }
    out
}
