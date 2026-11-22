//! 実在する API 定義 (生成表・広域表) の取得結果を、取得 (generic capture helper) → 符号化 → Trace の
//! 書き出しと読み戻し → 完全性の集計 (shirusu-analysis) → no-integrity-loss → conformance まで通し、
//! 「取得側が取得すべき値を取得できたと書いた記録」を、どの利用側も完全と判定するか
//! (d4fc3c0 レビュー P1: 戻り値のない Sleep の Stale を、完全性の判定が不完全と数えた。条件の判定
//! 関数だけのテストでは、期待値側が同じ誤りを写して見逃した)。
//!
//! 期待値は取得側の出力そのもの: Captured / Stale だけで書けた leave は完全、Unsupported を含む
//! leave は不完全。判定の式を期待値側に書かない。
use shirusu_analysis::{IntegritySummary, TraceView};
use shirusu_hook::api::{GenericApi, BROAD_API_TABLE, GENERATED_API_TABLE};
use shirusu_hook::api::generic_api;
use shirusu_hook::capture::{encode_generic_enter, encode_generic_leave_with_inputs};
use shirusu_trace::compact::decode_values;
use shirusu_trace::model::{EventBatch, PayloadRef, TraceEvent};
use shirusu_trace::proto::{CaptureEnvironment, SessionMetadata};
use shirusu_trace::schema::{EventCapture, ValueCapture};
use shirusu_trace::{FinalizationInfo, TraceWriter};

/// 戻り値の代表 (成功・失敗・ポインタ・INVALID_HANDLE_VALUE の 32/64bit 形)
const RETURNS: [u64; 6] = [0, 1, 0x1000, 0x7fff_0000_1234, 0xffff_ffff, u64::MAX];

fn apis() -> Vec<GenericApi> {
    GENERATED_API_TABLE
        .iter()
        .chain(BROAD_API_TABLE)
        .filter_map(|d| generic_api(d.api_id))
        .collect()
}

/// 1 API・1 戻り値の enter と leave。leave の取得状態も返す
fn call(api: GenericApi, ret: u64, call_id: u64) -> ([TraceEvent; 2], Vec<ValueCapture>) {
    let args = vec![0u64; api.arity()];
    let enter = encode_generic_enter::<12>(api, &args);
    let leave = encode_generic_leave_with_inputs(api, ret, 5, &args);
    let def = api.def();
    let types: Vec<_> = def.leave_schema().fields.iter().map(|f| shirusu_trace::schema::Ty::from_tag(f.ty)).collect::<Option<_>>().unwrap();
    let captures = decode_values(&types, leave.as_bytes()).unwrap().iter().map(|v| v.capture).collect();
    let event = |kind: u16, sequence: u64, schema_id: u32, args: &[u8]| TraceEvent {
        kind,
        sequence,
        timestamp_qpc: sequence,
        call_id,
        parent_call_id: 0,
        api_id: def.api_id,
        schema_id,
        capture: EventCapture::Captured,
        payload: PayloadRef::None,
        args: Some(args.to_vec()),
        global_order: None,
    };
    (
        [
            event(1, call_id * 2, def.enter_schema_id(), enter.as_bytes()),
            event(2, call_id * 2 + 1, def.leave_schema_id(), leave.as_bytes()),
        ],
        captures,
    )
}

/// 呼び出し列を Trace に書いて読み戻す
fn trace_of(apis: &[GenericApi], calls: Vec<[TraceEvent; 2]>) -> shirusu_trace::reader::ReadTrace {
    let env = CaptureEnvironment {
        session: SessionMetadata {
            session_id: 9,
            created_by: "catalog-results".into(),
            qpc_frequency: 1000,
        },
        ..Default::default()
    };
    let mut w = TraceWriter::new(Vec::new(), env).unwrap();
    for api in apis {
        w.register_schema(api.def().enter_schema()).unwrap();
        w.register_schema(api.def().leave_schema()).unwrap();
    }
    w.write_batch(&EventBatch {
        session_id: 9,
        process_id: 1,
        raw_pid: 1,
        thread_id: 1,
        qpc_frequency: 1000,
        events: calls.into_iter().flatten().collect(),
    })
    .unwrap();
    let bytes = w.finish(FinalizationInfo::default()).unwrap();
    shirusu_trace::read_trace(&bytes).unwrap()
}

#[test]
fn every_catalog_capture_is_judged_as_the_capture_side_wrote_it() {
    let apis = apis();
    assert!(apis.len() > 100, "the catalog is loaded");
    let mut checked = 0;
    for api in &apis {
        for ret in RETURNS {
            let (events, captures) = call(*api, ret, 1);
            let want = captures.iter().all(|c| matches!(c, ValueCapture::Captured | ValueCapture::Stale));
            let t = trace_of(std::slice::from_ref(api), vec![events]);
            let summary = IntegritySummary::of(&t);
            let view = TraceView::build(&t);
            let fields = view.events.iter().all(|e| e.fields.iter().all(|f| f.is_complete()));
            let at = format!("{} ret={ret:#x} {captures:?}", api.def().function);
            assert_eq!(summary.degraded_events == 0, want, "degraded: {at}");
            assert_eq!(fields, want, "fields: {at}");
            assert!(summary.inconsistent_calls.is_empty(), "{at}: {:?}", summary.inconsistent_calls);
            if want {
                assert!(summary.is_complete(), "{at}: {:?}", summary.incompleteness());
                let report = shirusu_conformance::check_trace(&t);
                assert!(report.conformant(), "{at}: {:?}", report.findings);
            }
            checked += 1;
        }
    }
    assert!(checked >= apis.len() * RETURNS.len());
}

/// レビューの例そのもの: Sleep (戻り値なし、Never) / SetLastError / EnterCriticalSection
#[test]
fn void_apis_with_never_meaning_are_complete() {
    let apis = apis();
    for name in ["Sleep", "SetLastError", "EnterCriticalSection"] {
        let api = *apis.iter().find(|a| a.def().function == name).unwrap_or_else(|| panic!("{name} in the catalog"));
        assert!(api.ret().is_none(), "{name} returns nothing");
        let (events, captures) = call(api, 0, 1);
        assert_eq!(captures, [ValueCapture::Stale], "{name}");
        let t = trace_of(&[api], vec![events]);
        let summary = IntegritySummary::of(&t);
        assert_eq!(summary.degraded_events, 0, "{name}");
        assert!(summary.is_complete(), "{name}: {:?}", summary.incompleteness());
        // 自己比較も Identical (ObservedPrefixAgrees ではない)
        let report = shirusu_compare::compare_traces(&t, &t);
        assert!(report.verdict().is_identical(), "{name}: {:?}", report.verdict());
    }
}

/// F32 を返す API (戻り値は結果情報にならないが、Never なら判定は変わらない)
#[test]
fn float_returning_apis_follow_the_same_rule() {
    let apis = apis();
    let floats: Vec<_> = apis
        .iter()
        .filter(|a| matches!(a.ret(), Some(shirusu_hook::api::GenericScalar::F32 | shirusu_hook::api::GenericScalar::F64)))
        .collect();
    assert!(!floats.is_empty(), "the catalog has float-returning APIs");
    let calls: Vec<_> = floats.iter().enumerate().map(|(i, a)| call(**a, 0x3f80_0000, i as u64 + 1)).collect();
    let all_known = calls.iter().all(|(_, c)| c.iter().all(|c| matches!(c, ValueCapture::Captured | ValueCapture::Stale)));
    let t = trace_of(&floats.iter().map(|a| **a).collect::<Vec<_>>(), calls.into_iter().map(|(e, _)| e).collect());
    assert_eq!(IntegritySummary::of(&t).degraded_events == 0, all_known);
}
