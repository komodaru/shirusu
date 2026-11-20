//! The source check guards registration. The CI log additionally proves that
//! Cargo discovered and ran each target, including platform-gated modules.
use super::{line, Hits, Tree};
use std::collections::BTreeSet;
use std::process::ExitCode;

pub(super) const EMITTER: &str = "crates/shirusu-emit/src/emitter.rs";
pub(super) const DURABILITY: &str = "crates/shirusu-trace/tests/durability.rs";

const HANDLES: &[(&str, &str, &str, usize)] = &[
    (EMITTER, "CallToken", "emitter::CallToken", 1),
    (
        "crates/shirusu-hook/src/runtime.rs",
        "ReentrancyGuard",
        "runtime::ReentrancyGuard",
        2,
    ),
    (
        "crates/shirusu-transport/src/context.rs",
        "PublishGuard",
        "context::PublishGuard",
        3,
    ),
    (
        "crates/shirusu-transport/src/shm.rs",
        "SlotRegistration",
        "shm::SlotRegistration",
        2,
    ),
];

// Source file, Cargo test binary, and full libtest name.
const TESTS: &[(&str, &str, &str)] = &[
    (
        "crates/shirusu-cli/src/repair.rs",
        "shirusu",
        "repair::pbt::middle_chunk_loss_survives_repair_and_index_damage_alone_preserves_events",
    ),
    (
        DURABILITY,
        "durability",
        "partial_batches_report_the_exact_retry_suffix",
    ),
    (
        "crates/shirusu-emit/tests/coverage_gaps.rs",
        "coverage_gaps",
        "flush_retry_keeps_only_the_unwritten_events",
    ),
    (
        "crates/shirusu-emit/tests/coverage_gaps.rs",
        "coverage_gaps",
        "foreign_token_cannot_close_or_abandon_local_call",
    ),
    (
        "crates/shirusu-conformance/src/check.rs",
        "shirusu_conformance",
        "check::tests::mismatched_leave_api_fails_pairing",
    ),
    (
        "crates/shirusu-compare/tests/scenarios.rs",
        "scenarios",
        "unrelated_leave_does_not_match_a_valid_call",
    ),
    (
        "crates/shirusu-conformance/src/check.rs",
        "shirusu_conformance",
        "check::tests::unreadable_required_value_contradicts_complete",
    ),
    (
        "crates/shirusu-test/tests/rules.rs",
        "rules",
        "uncaptured_fields_control_integrity_and_cannot_satisfy_value_rules",
    ),
    (
        "crates/shirusu-trace/src/schema.rs",
        "shirusu_trace",
        "schema::tests::undefined_value_claims_need_a_schema_declaration",
    ),
    (
        "crates/shirusu-conformance/src/check.rs",
        "shirusu_conformance",
        "check::tests::undefined_claims_on_fields_that_cannot_be_undefined_contradict_complete",
    ),
    (
        "crates/shirusu-hook/src/capture.rs",
        "shirusu_hook",
        "capture::tests::completion_byte_count_is_read_only_for_dequeued_packets",
    ),
    (
        "crates/shirusu-agent/tests/bootstrap_mapping.rs",
        "bootstrap_mapping",
        "duplicate_registration_preserves_events_in_the_registered_mapping",
    ),
    (
        "crates/shirusu-loom-tests/src/lib.rs",
        "shirusu_loom_tests",
        "tests::loom_cancel_publishes_free_after_clearing_owner",
    ),
    (
        "crates/shirusu-trace/tests/stream_reader.rs",
        "stream_reader",
        "long_chain_loads_with_bounded_work",
    ),
    (
        "crates/shirusu-host/src/background_drain.rs",
        "shirusu_host",
        "background_drain::tests::ready_reader_keeps_draining_while_child_setup_waits",
    ),
    (
        "crates/shirusu-agent/src/writer_cell.rs",
        "shirusu_agent",
        "writer_cell::tests::racing_claimers_get_exactly_one_owner",
    ),
    (
        "crates/shirusu-hook/src/hazard.rs",
        "shirusu_hook",
        "hazard::tests::terminated_owner_hazard_is_withdrawn",
    ),
];

pub(super) fn opaque_handles(tree: &Tree) -> Hits {
    let mut hits = Hits::new();
    for &(file, name, _, _) in HANDLES {
        if !tree.has_or_real(file) {
            continue;
        }
        let ast = syn::parse_file(&tree.text(file));
        let definition = ast.as_ref().ok().and_then(|ast| {
            ast.items.iter().find_map(|item| match item {
                syn::Item::Struct(s) if s.ident == name => Some(s),
                _ => None,
            })
        });
        match definition {
            Some(s)
                if !s.fields.is_empty()
                    && s.fields
                        .iter()
                        .all(|f| !matches!(f.vis, syn::Visibility::Public(_))) => {}
            s => hits.push((
                file.into(),
                s.map_or(1, |s| line(s.ident.span())),
                format!("{name} は公開フィールドのない非空の構造体にする"),
            )),
        }
    }
    hits
}

fn find_test<'a>(items: &'a [syn::Item], name: &str) -> Option<&'a syn::ItemFn> {
    if let Some((module, rest)) = name.split_once("::") {
        return items.iter().find_map(|item| match item {
            syn::Item::Mod(m) if m.ident == module => find_test(&m.content.as_ref()?.1, rest),
            _ => None,
        });
    }
    items.iter().find_map(|item| match item {
        syn::Item::Fn(f) if f.sig.ident == name => Some(f),
        _ => None,
    })
}

pub(super) fn registered_tests(tree: &Tree) -> Hits {
    let mut hits = Hits::new();
    let files: BTreeSet<_> = TESTS.iter().map(|row| row.0).collect();
    for file in files {
        if !tree.has_or_real(file) {
            continue;
        }
        let ast = match syn::parse_file(&tree.text(file)) {
            Ok(ast) => ast,
            Err(e) => {
                hits.push((file.into(), line(e.span()), e.to_string()));
                continue;
            }
        };
        let stem = std::path::Path::new(file)
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or(file);
        for &(_, _, name) in TESTS.iter().filter(|row| row.0 == file) {
            let local = name
                .strip_prefix(stem)
                .and_then(|s| s.strip_prefix("::"))
                .unwrap_or(name);
            let test = find_test(&ast.items, local);
            if !test.is_some_and(|f| {
                f.attrs.iter().any(|a| a.path().is_ident("test"))
                    && !f.attrs.iter().any(|a| a.path().is_ident("ignore"))
                    && !f.block.stmts.is_empty()
            }) {
                hits.push((
                    file.into(),
                    test.map_or(1, |f| line(f.sig.ident.span())),
                    format!("有効な #[test] {name} が必要"),
                ));
            }
        }
    }
    hits
}

fn missing_tests(log: &str) -> Vec<String> {
    let mut passed = BTreeSet::new();
    let mut doc_cases = BTreeSet::new();
    let mut binary = None;
    let mut docs = false;
    for raw in log.lines() {
        let text = raw.trim_start();
        if text.starts_with("Running ") {
            binary = text
                .strip_suffix(')')
                .and_then(|s| s.rsplit_once('('))
                .and_then(|(_, exe)| std::path::Path::new(exe).file_name()?.to_str())
                .and_then(|exe| exe.rsplit_once('-'))
                .map(|(target, _)| target);
            docs = false;
        } else if text.starts_with("Doc-tests ") {
            binary = None;
            docs = true;
        } else if let Some(name) = text
            .strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... ok"))
        {
            if let Some(target) = binary {
                passed.insert((target, name));
            }
            if docs {
                // Keep the source line to distinguish separate examples on one type.
                if let Some(case) = name
                    .strip_suffix(") - compile fail")
                    .and_then(|s| s.rsplit_once(" (line "))
                {
                    if let Ok(n) = case.1.parse::<usize>() {
                        doc_cases.insert((case.0, n));
                    }
                }
            }
        }
    }
    let mut missing: Vec<_> = TESTS
        .iter()
        .filter(|(_, target, name)| !passed.contains(&(*target, *name)))
        .map(|(file, _, name)| format!("{file}: {name}"))
        .collect();
    for &(file, _, item, count) in HANDLES {
        let key = format!("{file} - {item}");
        let ran = doc_cases.iter().filter(|(path, _)| *path == key).count();
        if ran < count {
            missing.push(format!("{key}: compile_fail {ran}/{count}"));
        }
    }
    missing
}

pub(super) fn check_log(log: &str) -> ExitCode {
    let missing = missing_tests(log);
    if missing.is_empty() {
        println!("shirusu-lint: review-tests: all required tests passed");
        ExitCode::SUCCESS
    } else {
        for name in missing {
            eprintln!("shirusu-lint: review-tests: 実行成功を確認できない: {name}");
        }
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_requires_a_pass_in_the_right_target_and_distinct_compile_fail_examples() {
        let mut log = String::new();
        for &(_, target, name) in TESTS {
            log.push_str(&format!(
                "     Running tests (target/debug/deps/{target}-abcdef)\ntest {name} ... ok\n"
            ));
        }
        log.push_str("   Doc-tests shirusu\n");
        for &(file, _, item, count) in HANDLES {
            for n in 1..=count {
                log.push_str(&format!(
                    "test {file} - {item} (line {n}) - compile fail ... ok\n"
                ));
            }
        }
        assert!(missing_tests(&log).is_empty());
        for altered in [
            log.replace("durability-abcdef", "other-abcdef"),
            log.replace("retry_suffix ... ok", "retry_suffix ... ignored"),
            log.replace("retry_suffix ... ok", "retry_suffix ... FAILED"),
            log.replace("PublishGuard (line 3)", "PublishGuard (line 2)"),
            log.replace(" - compile fail ... ok", " ... ok"),
        ] {
            assert!(!missing_tests(&altered).is_empty());
        }
    }
}
