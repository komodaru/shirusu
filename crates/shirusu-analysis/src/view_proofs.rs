//! Kani: what is not known stays not known (scripts/ci/kani.sh). The summary of any Trace,
//! every field symbolic (the lists and the loss map one of a few shapes: empty, the
//! optimisation-only `index_mismatch`, a real issue; no loss, a counted kind, the
//! UNKNOWN_LOSS_COUNT kind), against what [`IntegritySummary::incompleteness`] reports:
//!
//! * it reports nothing exactly when every part of the observation is whole (a Finalization
//!   written cleanly, the event count agreeing, no loss, no recovery issue but index_mismatch,
//!   no dropped payload, no missing Blob, no degraded event, no inconsistent call, each of the
//!   three declared levels Complete);
//! * each part that is not whole is reported as its own variant, with its count (none is
//!   folded into another or lost);
//! * a loss count is Exact only when no record says its count is unknown.
//!
//! Correspondence: the real fn, on the real struct. Bounds: collections of at most one entry,
//! from the shapes above (CBMC on symbolic collections does not end); the counts and levels
//! are any values.
use super::*;

fn any_level() -> Option<u32> {
    if kani::any() { Some(kani::any()) } else { None }
}

fn any_summary() -> IntegritySummary {
    let loss: u8 = kani::any();
    let mut loss_records_by_kind = BTreeMap::new();
    match loss {
        0 => {}
        1 => {
            loss_records_by_kind.insert(1u32, kani::any::<u64>());
        }
        _ => {
            loss_records_by_kind.insert(shirusu_trace::proto::loss_kind::UNKNOWN_LOSS_COUNT, kani::any::<u64>());
        }
    }
    let issue: u8 = kani::any();
    let recovery_issues = match issue {
        0 => Vec::new(),
        1 => vec!["index_mismatch".to_string()],
        _ => vec!["corrupt_block".to_string()],
    };
    IntegritySummary {
        finalized_cleanly: kani::any(),
        has_finalization: kani::any(),
        event_count_mismatch: kani::any(),
        capture_coverage: any_level(),
        event_integrity: any_level(),
        semantic_completeness: any_level(),
        finalization_notes: Vec::new(),
        loss_records_by_kind,
        total_loss_count: kani::any(),
        dropped_payload_events: kani::any(),
        dropped_payload_bytes: kani::any(),
        degraded_events: kani::any(),
        recovery_issues,
        missing_blob_refs: kani::any(),
        index_matches_scan: kani::any(),
        has_stored_index: kani::any(),
        inconsistent_calls: if kani::any() { vec!["CreateFileW".to_string()] } else { Vec::new() },
        checked_stale_inputs: kani::any(),
    }
}

#[kani::proof]
#[kani::unwind(4)]
fn nothing_unknown_is_reported_whole() {
    let s = any_summary();
    let gaps = s.incompleteness();
    let whole = s.has_finalization
        && s.finalized_cleanly
        && !s.event_count_mismatch
        && s.loss_records_by_kind.is_empty()
        && s.recovery_issues.iter().all(|i| i == "index_mismatch")
        && s.dropped_payload_events == 0
        && s.missing_blob_refs == 0
        && s.degraded_events == 0
        && s.inconsistent_calls.is_empty()
        && s.capture_coverage == Some(0)
        && s.event_integrity == Some(0)
        && s.semantic_completeness == Some(0);
    assert_eq!(gaps.is_empty(), whole);
    assert_eq!(s.is_complete(), whole);
    // (each part not whole has its own report)
    let has = |f: &dyn Fn(&Incompleteness) -> bool| gaps.iter().any(f);
    assert_eq!(has(&|i| *i == Incompleteness::NoFinalization), !s.has_finalization);
    assert_eq!(has(&|i| *i == Incompleteness::NotFinalizedCleanly), s.has_finalization && !s.finalized_cleanly);
    assert_eq!(has(&|i| *i == Incompleteness::EventCountMismatch), s.event_count_mismatch);
    assert_eq!(has(&|i| matches!(i, Incompleteness::RecoveryIssues(_))), s.recovery_issues.iter().any(|i| i != "index_mismatch"));
    assert_eq!(has(&|i| *i == Incompleteness::DroppedPayloads { events: s.dropped_payload_events }), s.dropped_payload_events > 0);
    assert_eq!(has(&|i| *i == Incompleteness::MissingBlobRefs { refs: s.missing_blob_refs }), s.missing_blob_refs > 0);
    assert_eq!(has(&|i| *i == Incompleteness::DegradedEvents { events: s.degraded_events }), s.degraded_events > 0);
    assert_eq!(has(&|i| matches!(i, Incompleteness::InconsistentCalls(_))), !s.inconsistent_calls.is_empty());
    for (aspect, level) in [
        (IntegrityAspect::CaptureCoverage, s.capture_coverage),
        (IntegrityAspect::EventIntegrity, s.event_integrity),
        (IntegrityAspect::SemanticCompleteness, s.semantic_completeness),
    ] {
        assert_eq!(has(&|i| *i == Incompleteness::Declared { aspect, level }), level != Some(0));
    }
    kani::cover!(whole, "a whole observation");
    kani::cover!(!whole && gaps.len() == 1, "one part missing");
    kani::cover!(whole && !s.recovery_issues.is_empty(), "whole with only an index mismatch");
}

#[kani::proof]
#[kani::unwind(4)]
fn a_loss_count_is_exact_only_when_known() {
    let s = any_summary();
    let gaps = s.incompleteness();
    let unknown = s.loss_records_by_kind.contains_key(&shirusu_trace::proto::loss_kind::UNKNOWN_LOSS_COUNT);
    let loss = gaps.iter().find_map(|i| match i {
        Incompleteness::LossRecords { records_by_kind, events } => Some((records_by_kind, *events)),
        _ => None,
    });
    match loss {
        None => assert!(s.loss_records_by_kind.is_empty()),
        Some((records, events)) => {
            assert_eq!(records, &s.loss_records_by_kind);
            assert_eq!(events, if unknown { LossCount::AtLeast(s.total_loss_count) } else { LossCount::Exact(s.total_loss_count) });
        }
    }
    kani::cover!(loss.is_some_and(|(_, e)| matches!(e, LossCount::AtLeast(_))), "an unknown count kept as at least");
    kani::cover!(loss.is_some_and(|(_, e)| matches!(e, LossCount::Exact(_))), "an exact count");
}
