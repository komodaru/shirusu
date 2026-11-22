//! ルール評価の検証。合成 Trace に対して各ルールの pass/fail と、
//! CLI の終了コードを確認する。

use shirusu_test::{evaluate, TestSpec};
use shirusu_trace::compact::encode_values;
use shirusu_trace::model::PayloadLossReason;
use shirusu_trace::reader::ReadTrace;
use shirusu_trace::schema::{Ty, TypedValue};
use shirusu_trace::writer::FinalizationInfo;
use shirusu_trace::PayloadRef;

mod common;
use common::{cv, B};

fn clean_trace() -> ReadTrace {
    let mut b = B::new();
    b.create(0x100, "C:\\game\\save.dat");
    b.read(0x100, 4, PayloadRef::None);
    b.close(0x100);
    b.finish(common::session("test-crate"), FinalizationInfo::default())
}

#[test]
fn recovered_trace_cannot_pass_no_integrity_loss() {
    let mut trace = clean_trace();
    trace
        .issues
        .push(shirusu_trace::reader::RecoveryIssue::CorruptBlock {
            offset: 100,
            reason: "crc mismatch".into(),
        });
    let s = spec(r#"{"rules":[{"type":"no-integrity-loss"}]}"#);
    assert!(!evaluate(&s, &trace).all_passed());
    trace.issues.clear();
    trace.finalized_cleanly = false;
    assert!(!evaluate(&s, &trace).all_passed());
}

#[test]
fn failed_close_is_not_a_closed_object() {
    let mut trace = clean_trace();
    for batch in &mut trace.batches {
        for e in &mut batch.events {
            if e.schema_id == 8 {
                e.args = Some(
                    encode_values(
                        &[Ty::I32, Ty::U32],
                        &[cv(TypedValue::I32(0)), cv(TypedValue::U32(6))],
                    )
                    .unwrap(),
                );
            }
        }
    }
    let s = spec(r#"{"rules":[{"type":"object-closed","created_by":"kernel32!CreateFileW"}]}"#);
    assert!(!evaluate(&s, &trace).all_passed());
}

fn spec(json: &str) -> TestSpec {
    TestSpec::parse(json).unwrap()
}

#[test]
fn api_called_variants() {
    let trace = clean_trace();
    let s = spec(
        r#"{"name":"t","rules":[
            {"type":"api-called","api":"kernel32!CreateFileW"},
            {"type":"api-called","api":"kernel32!ReadFile","field":"nNumberOfBytesToRead","equals":4},
            {"type":"api-called","api":"kernel32!CreateFileW","field":"lpFileName","contains":"save.dat"},
            {"type":"api-called","api":"kernel32!WriteFile"}
        ]}"#,
    );
    let report = evaluate(&s, &trace);
    assert!(report.results[0].passed, "{:?}", report.results[0]);
    assert!(report.results[1].passed, "{:?}", report.results[1]);
    assert!(report.results[2].passed, "{:?}", report.results[2]);
    assert!(!report.results[3].passed, "{:?}", report.results[3]);
    assert!(!report.all_passed());
}

#[test]
fn api_called_field_mismatch_reports_observed() {
    let trace = clean_trace();
    let s = spec(
        r#"{"rules":[{"type":"api-called","api":"kernel32!ReadFile","field":"nNumberOfBytesToRead","equals":99}]}"#,
    );
    let report = evaluate(&s, &trace);
    assert!(!report.results[0].passed);
    assert!(
        report.results[0].detail.contains("observed: 4"),
        "{}",
        report.results[0].detail
    );
}

#[test]
fn no_integrity_loss_pass_and_fail() {
    let s = spec(r#"{"rules":[{"type":"no-integrity-loss"}]}"#);
    assert!(evaluate(&s, &clean_trace()).all_passed());

    // Payload が落ちた Trace は失敗する。
    let mut b = B::new();
    b.create(0x100, "x");
    b.read(
        0x100,
        4,
        PayloadRef::Dropped {
            original_len: 4096,
            reason: PayloadLossReason::Overflow,
        },
    );
    b.close(0x100);
    let dropped = b.finish(common::session("test-crate"), FinalizationInfo::default());
    let report = evaluate(&s, &dropped);
    assert!(!report.all_passed());
    assert!(
        report.results[0].detail.contains("dropped_payloads"),
        "{}",
        report.results[0].detail
    );

    // Finalization が Degraded を宣言する Trace も失敗する。
    let degraded = clean_trace_degraded();
    let report = evaluate(&s, &degraded);
    assert!(!report.all_passed());
}

fn clean_trace_degraded() -> ReadTrace {
    let mut b = B::new();
    b.create(0x100, "x");
    b.close(0x100);
    b.finish(common::session("test-crate"), FinalizationInfo {
        event_integrity: 1,
        ..Default::default()
    })
}

#[test]
fn object_closed_variants() {
    let s = spec(r#"{"rules":[{"type":"object-closed","created_by":"kernel32!CreateFileW"}]}"#);
    assert!(evaluate(&s, &clean_trace()).all_passed());

    let mut b = B::new();
    b.create(0x100, "x");
    b.read(0x100, 4, PayloadRef::None);
    let unclosed = b.finish(common::session("test-crate"), FinalizationInfo::default());
    let report = evaluate(&s, &unclosed);
    assert!(!report.all_passed());
    assert!(
        report.results[0].detail.contains("unclosed"),
        "{}",
        report.results[0].detail
    );

    // 生成自体が無い場合は空虚な真にせず失敗とする。
    let empty = B::new().finish(common::session("test-crate"), FinalizationInfo::default());
    let report = evaluate(&s, &empty);
    assert!(!report.all_passed());
    assert!(
        report.results[0].detail.contains("no object"),
        "{}",
        report.results[0].detail
    );
}

#[test]
fn event_count_bounds() {
    let trace = clean_trace();
    let s = spec(r#"{"rules":[{"type":"event-count","min":6,"max":6}]}"#);
    assert!(evaluate(&s, &trace).all_passed());
    let s = spec(r#"{"rules":[{"type":"event-count","min":7}]}"#);
    assert!(!evaluate(&s, &trace).all_passed());
}

#[test]
fn uncaptured_fields_control_integrity_and_cannot_satisfy_value_rules() {
    use shirusu_trace::{compact::decode_values, schema::ValueCapture};
    for capture in [
        ValueCapture::Unreadable,
        ValueCapture::Unsupported,
        ValueCapture::Truncated,
        ValueCapture::LimitExceeded,
        // 必須の入力 (lpFileName) は結果によらず定義されているので、
        // 「古い」「定義されない」と書いても取得できたことにはならない。
        ValueCapture::Stale,
        ValueCapture::NotApplicable,
    ] {
        let mut trace = clean_trace();
        let event = &mut trace.batches[0].events[0];
        let types = [Ty::Str16, Ty::U32];
        let mut values = decode_values(&types, event.args.as_ref().unwrap()).unwrap();
        values[0].capture = capture;
        values[0].value = TypedValue::Str16("dummy".encode_utf16().collect());
        event.args = Some(encode_values(&types, &values).unwrap());
        let integrity = spec(r#"{"rules":[{"type":"no-integrity-loss"}]}"#);
        assert!(!evaluate(&integrity, &trace).all_passed(), "{capture:?}");
        for condition in [r#""equals":"dummy""#, r#""contains":"dum""#] {
            let rule = spec(&format!(
                r#"{{"rules":[{{"type":"api-called","api":"kernel32!CreateFileW","field":"lpFileName",{condition}}}]}}"#
            ));
            assert!(!evaluate(&rule, &trace).all_passed(), "{capture:?}");
        }
    }
}

#[test]
fn integrity_rule_rejects_reader_recovery_even_with_complete_finalization() {
    let rule = spec(r#"{"rules":[{"type":"no-integrity-loss"}]}"#);
    let mut trace = clean_trace();
    trace
        .issues
        .push(shirusu_trace::reader::RecoveryIssue::TruncatedTail { offset: 7 });
    assert!(!evaluate(&rule, &trace).all_passed());
    // 規則は IntegritySummary::incompleteness と同じ答え (d4fc3c0 レビュー 1.6: 項目を手で並べた
    // 写しは別の答えを出した)。Index の不一致だけは最適化の Index の問題で、イベントは走査から
    // 読めている: inspect / compare と同じく欠けに数えない
    let mut index_only = clean_trace();
    index_only
        .issues
        .push(shirusu_trace::reader::RecoveryIssue::IndexMismatch);
    assert_eq!(
        evaluate(&rule, &index_only).all_passed(),
        shirusu_analysis::IntegritySummary::of(&index_only).is_complete()
    );
}
