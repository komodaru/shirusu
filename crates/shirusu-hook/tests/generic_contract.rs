//! 外部レビュー 2026-11-16 P2: 生成 Detour (engine_generated.inc) と API 表の
//! 対応を、生成物そのものから確かめる。表の形 (型と leave) はビルド時の
//! `GENERIC_CONTRACT` が確かめ、ここは Detour ごとの引数の数と、全ての汎用
//! API に Detour があることを確かめる (生成器の出力の golden 検査)。

use shirusu_hook::api::{generic_api, BROAD_API_TABLE, GENERATED_API_TABLE};
use std::collections::BTreeMap;

/// `generated_detour(ID, &[a0 as u64, ...], invoke)` の ID と引数の数。
fn generated_detours() -> BTreeMap<u32, usize> {
    let text = include_str!("../src/engine_generated.inc");
    let mut found = BTreeMap::new();
    for (_, rest) in text
        .match_indices("generated_detour(")
        .map(|(i, _)| ((), &text[i..]))
    {
        let rest = &rest["generated_detour(".len()..];
        let (id, after) = rest.split_once(", &[").expect("generated_detour(ID, &[...");
        let args = &after[..after.find(']').expect("argument list")];
        let arity = if args.trim().is_empty() {
            0
        } else {
            args.split(',').count()
        };
        let id: u32 = id.trim().parse().expect("numeric API ID");
        assert!(
            found.insert(id, arity).is_none(),
            "API {id} has two Detours"
        );
    }
    found
}

#[test]
fn every_generated_detour_matches_its_row_arity() {
    let detours = generated_detours();
    assert!(!detours.is_empty());
    for (&id, &arity) in &detours {
        let api = generic_api(id).unwrap_or_else(|| panic!("Detour for non-generic API {id}"));
        assert_eq!(api.arity(), arity, "{}", api.def().function);
    }
    for api in GENERATED_API_TABLE.iter().chain(BROAD_API_TABLE) {
        assert!(
            detours.contains_key(&api.api_id),
            "{} has no Detour",
            api.function
        );
    }
}
