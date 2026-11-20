//! The same file is masu-lint's, shirusu-lint's and kikizake-lint's (each repository checks
//! its own tree in its own tests).
use super::*;

/// a function long enough to count (`Kind::Exact.min_tokens`), its locals named by `x`, `y`
fn body(name: &str, x: &str, y: &str) -> String {
    format!(
        "pub fn {name}(src: &[u8], at: usize) -> Option<u32> {{\n    let {x} = src.get(at..at.checked_add(4)?)?;\n    let mut {y} = 0u32;\n    for (k, b) in {x}.iter().enumerate() {{\n        {y} |= u32::from(*b) << (8 * k);\n    }}\n    if {y} == 0xffff_ffff {{ return None; }}\n    let s = {x}.len().checked_mul(2)?.checked_add(usize::from({x}[0]))?;\n    Some({y}.wrapping_add(u32::try_from(s).ok()?).rotate_left(3))\n}}\n"
    )
}

fn groups(fs: &[(&str, &str)]) -> Vec<Group> {
    find(&fs.iter().map(|(p, t)| ((*p).to_owned(), (*t).to_owned())).collect::<Vec<_>>())
}

#[test]
fn the_body_is_long_enough_to_count() {
    let f: syn::ItemFn = syn::parse_str(&body("a", "w", "v")).unwrap();
    let mut ts = f.sig.to_token_stream();
    f.block.to_tokens(&mut ts);
    let n = flat(ts).len();
    assert!(n >= Kind::Renamed.min_tokens(), "{n}");
}

#[test]
fn a_copy_under_another_name_in_another_crate_is_found() {
    let g = groups(&[("crates/a/src/lib.rs", &body("le32", "w", "v")), ("crates/b/src/x.rs", &format!("/// docs differ\n{}", body("read32", "w", "v")))]);
    assert_eq!(g.len(), 1, "{g:?}");
    let g = &g[0];
    assert_eq!(g.kind, Kind::Exact);
    assert_eq!(g.members, vec![Member { file: "crates/a/src/lib.rs".into(), name: "le32".into() }, Member { file: "crates/b/src/x.rs".into(), name: "read32".into() }]);
    assert_eq!(g.lines, vec![1, 2]);
}

#[test]
fn a_copy_with_renamed_locals_is_found_as_renamed() {
    let g = groups(&[("a.rs", &body("le32", "w", "v")), ("b.rs", &body("le32", "word", "acc"))]);
    assert_eq!(g.iter().map(|g| g.kind).collect::<Vec<_>>(), vec![Kind::Renamed], "{g:?}");
}

#[test]
fn a_different_call_or_type_is_not_a_copy() {
    let other = body("le32", "w", "v").replace("rotate_left", "rotate_right");
    assert!(groups(&[("a.rs", &body("le32", "w", "v")), ("b.rs", &other)]).is_empty());
    let other = body("le32", "w", "v").replace("u32::from", "u64::from");
    assert!(groups(&[("a.rs", &body("le32", "w", "v")), ("b.rs", &other)]).is_empty());
}

#[test]
fn methods_carry_their_type_and_short_functions_do_not_count() {
    let m = format!("struct S;\nimpl S {{\n{}}}\n", body("get", "w", "v"));
    let g = groups(&[("a.rs", &m), ("b.rs", &body("free", "w", "v"))]);
    assert_eq!(g[0].members[0].name, "S::get");
    let short = "fn f(a: u32) -> u32 { a.wrapping_add(1) }\n";
    assert!(groups(&[("a.rs", short), ("b.rs", short)]).is_empty());
}

#[test]
fn tables_are_compared_whole() {
    let t: String = (0..40).map(|k| format!("{k}, ")).collect();
    let a = format!("pub const T: [u8; 40] = [{t}];\n");
    let b = format!("static TABLE: [u8; 40] = [{t}];\n");
    let g = groups(&[("a.rs", &a), ("b.rs", &b)]);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].kind, Kind::Table);
    let c = a.replace("39, ", "38, ");
    assert!(groups(&[("a.rs", &a), ("b.rs", &c)]).is_empty());
}

#[test]
fn the_known_list_lets_its_group_be_and_nothing_else() {
    let a = ("crates/a/src/lib.rs", body("le32", "w", "v"));
    let b = ("crates/b/src/x.rs", body("read32", "w", "v"));
    let c = ("crates/c/src/y.rs", body("third", "w", "v"));
    let found = groups(&[(a.0, &a.1), (b.0, &b.1)]);
    let line = line_of(&found[0], "the test oracle");
    assert_eq!(line, "exact\tcrates/a/src/lib.rs#le32\tcrates/b/src/x.rs#read32\t# the test oracle");
    let known = load_known(&format!("# comment\n\n{line}\n"));
    assert!(fresh(&found, &known).is_empty());
    // (a third copy is new: the known pair does not cover it)
    let found3 = groups(&[(a.0, &a.1), (b.0, &b.1), (c.0, &c.1)]);
    assert_eq!(fresh(&found3, &known).len(), 1);
    // (a known exact group does not cover a renamed copy)
    let known_renamed = load_known(&line.replace("exact", "renamed"));
    assert_eq!(fresh(&found, &known_renamed).len(), 1);
}

#[test]
fn doc_comments_inside_bodies_do_not_count() {
    let a = body("f", "w", "v");
    let b = body("f", "w", "v").replace("let mut v", "/// a word\n    #[allow(unused_mut)]\n    let mut v");
    let g = groups(&[("a.rs", &a), ("b.rs", &b)]);
    assert!(g.is_empty(), "an allow is code, a doc comment is not: {g:?}");
    let c = body("f", "w", "v").replace("let mut v", "/// a word\n    let mut v");
    assert_eq!(groups(&[("a.rs", &a), ("c.rs", &c)]).len(), 1);
}

#[test]
fn update_drops_gone_groups_only() {
    let a = ("crates/a/src/lib.rs", body("le32", "w", "v"));
    let b = ("crates/b/src/x.rs", body("read32", "w", "v"));
    let found = groups(&[(a.0, &a.1), (b.0, &b.1)]);
    let keep = line_of(&found[0], "kept");
    let gone = "exact\tcrates/z/src/z.rs#gone\tcrates/y/src/y.rs#gone\t# its copies were merged";
    let text = format!("# header\n{keep}\n{gone}\n");
    assert_eq!(updated(&text, &found), format!("# header\n{keep}\n"));
    assert!(report(&fresh(&found, &[]), "sites.tsv").contains("crates/a/src/lib.rs:1 (le32)"));
}
