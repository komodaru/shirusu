//! ReadFile / WriteFile / GetQueuedCompletionStatus の結果の種類と、記録した値の意味が、取得
//! (shirusu-hook) → 符号化 → Trace の書き出しと読み戻し → 完全性の集計 (shirusu-analysis) →
//! conformance のどこでも同じ答えになるか (7c5a553 レビュー P1-3 / P2-1)。
//!
//! - 部分読み取り (FALSE + ERROR_MORE_DATA) では、読めたバイト数とその内容を残す。
//! - 取得側が NotApplicable と書いたフィールドは、Schema の出力条件と実際の結果で検査器が
//!   受理する (取得側と検査器の分類が食い違えば、正しい記録が不完全と数えられる)。
//! - 検査器は、実際の結果では値があるのに NotApplicable / Stale と書いた記録を拒む。
use proptest::prelude::*;
use shirusu_hook::api::{api_by_id, API_GET_QUEUED_COMPLETION_STATUS};
use shirusu_hook::capture::*;
use shirusu_trace::compact::validate_values_completeness;
use shirusu_trace::model::{EventBatch, PayloadRef, TraceEvent};
use shirusu_trace::proto::{CaptureEnvironment, SessionMetadata};
use shirusu_trace::schema::{CapturedValue, EventCapture, SchemaRegistry, TypedValue, ValueCapture};
use shirusu_conformance::CheckStatus;
use shirusu_trace::{FinalizationInfo, TraceWriter};

const READ_FILE: u32 = 2;
const WRITE_FILE: u32 = 3;

fn registry(api_id: u32) -> SchemaRegistry {
    let mut r = SchemaRegistry::new();
    let api = api_by_id(api_id).unwrap();
    r.register(api.leave_schema()).unwrap();
    r
}

/// 取得した leave の値列が、検査器の目で完全か
fn complete(api_id: u32, values: &[CapturedValue]) -> bool {
    let api = api_by_id(api_id).unwrap();
    let bytes = encode_leave(api, values).unwrap();
    let r = registry(api_id);
    validate_values_completeness(r.shape_ref(api.leave_schema_id()).unwrap(), &bytes).unwrap()
}

/// leave 1 件だけの Trace を書いて読み戻し、完全性の集計とconformance の値検査に通す
fn through_trace(api_id: u32, values: &[CapturedValue]) -> (bool, bool) {
    let api = api_by_id(api_id).unwrap();
    let env = CaptureEnvironment {
        session: SessionMetadata {
            session_id: 3,
            created_by: "io-outcome".into(),
            qpc_frequency: 1000,
        },
        ..Default::default()
    };
    let mut w = TraceWriter::new(Vec::new(), env).unwrap();
    w.register_schema(api.leave_schema()).unwrap();
    w.write_batch(&EventBatch {
        session_id: 3,
        process_id: 1,
        raw_pid: 1,
        thread_id: 1,
        qpc_frequency: 1000,
        events: vec![TraceEvent {
            kind: 2,
            sequence: 1,
            timestamp_qpc: 1,
            call_id: 1,
            parent_call_id: 0,
            api_id,
            schema_id: api.leave_schema_id(),
            capture: EventCapture::Captured,
            payload: PayloadRef::None,
            args: Some(encode_leave(api, values).unwrap()),
            global_order: None,
        }],
    })
    .unwrap();
    let bytes = w.finish(FinalizationInfo::default()).unwrap();
    let t = shirusu_trace::read_trace(&bytes).unwrap();
    let summary = shirusu_analysis::IntegritySummary::of(&t);
    let view = shirusu_analysis::TraceView::build(&t);
    let fields_complete = view.events.iter().all(|e| e.fields.iter().all(|f| f.is_complete()));
    (summary.degraded_events == 0, fields_complete)
}

/// enter と leave 1 組の Trace を書いて読み戻し、conformance の `last-error-inputs` を返す
fn last_error_inputs(api_id: u32, enter: &[CapturedValue], leave: &[CapturedValue]) -> CheckStatus {
    let api = api_by_id(api_id).unwrap();
    let env = CaptureEnvironment {
        session: SessionMetadata {
            session_id: 3,
            created_by: "io-outcome".into(),
            qpc_frequency: 1000,
        },
        ..Default::default()
    };
    let mut w = TraceWriter::new(Vec::new(), env).unwrap();
    w.register_schema(api.enter_schema()).unwrap();
    w.register_schema(api.leave_schema()).unwrap();
    let event = |kind: u16, sequence: u64, schema_id: u32, args: Vec<u8>| TraceEvent {
        kind,
        sequence,
        timestamp_qpc: sequence,
        call_id: 1,
        parent_call_id: 0,
        api_id,
        schema_id,
        capture: EventCapture::Captured,
        payload: PayloadRef::None,
        args: Some(args),
        global_order: None,
    };
    w.write_batch(&EventBatch {
        session_id: 3,
        process_id: 1,
        raw_pid: 1,
        thread_id: 1,
        qpc_frequency: 1000,
        events: vec![
            event(1, 1, api.enter_schema_id(), encode_enter(api, enter).unwrap()),
            event(2, 2, api.leave_schema_id(), encode_leave(api, leave).unwrap()),
        ],
    })
    .unwrap();
    let bytes = w.finish(FinalizationInfo::default()).unwrap();
    let t = shirusu_trace::read_trace(&bytes).unwrap();
    let report = shirusu_conformance::check_trace(&t);
    report.finding("last-error-inputs").unwrap().status
}

fn memory(data: &[u8], count: u32) -> MapMemory {
    let mut m = MapMemory::new();
    m.write(0x4000, data);
    m.write_u32(0x5000, count);
    m
}

#[test]
fn a_partial_message_read_keeps_the_bytes_it_read() {
    // メッセージモードのパイプ: バッファ 4 バイト、FALSE / ERROR_MORE_DATA、読めた 4 バイトは有効
    let m = memory(b"abcd", 4);
    let v = read_file_leave(&m, 0, 0x4000, 4, 0x5000, false, ERROR_MORE_DATA);
    assert_eq!(v[1], CapturedValue::captured(TypedValue::U32(4)));
    assert_eq!(v[2], CapturedValue::captured(TypedValue::Bytes(b"abcd".to_vec())));
    assert_eq!(v[3], CapturedValue::captured(TypedValue::U32(ERROR_MORE_DATA)));
    assert!(complete(READ_FILE, &v));
    assert_eq!(through_trace(READ_FILE, &v), (true, true));
    // 内容を空にした記録 (レビューの再現) は、読めたバイト数と食い違うので完全ではない
    let mut emptied = v.clone();
    emptied[2] = CapturedValue {
        capture: ValueCapture::NotApplicable,
        value: TypedValue::Bytes(Vec::new()),
    };
    assert!(!complete(READ_FILE, &emptied));
    assert_eq!(through_trace(READ_FILE, &emptied), (false, false));
}

#[test]
fn close_handle_stale_and_not_applicable_are_checked_against_the_return() {
    // 7c5a553 レビュー P2-1 の 2 例 (組み込みの CloseHandle Schema)
    let close = 4;
    let stale_on_failure = vec![
        CapturedValue::captured(TypedValue::I32(0)),
        CapturedValue { capture: ValueCapture::Stale, value: TypedValue::U32(6) },
    ];
    assert!(!complete(close, &stale_on_failure));
    assert_eq!(through_trace(close, &stale_on_failure), (false, false));
    let not_applicable_return = vec![
        CapturedValue { capture: ValueCapture::NotApplicable, value: TypedValue::I32(1) },
        CapturedValue { capture: ValueCapture::Stale, value: TypedValue::U32(6) },
    ];
    assert!(!complete(close, &not_applicable_return));
    // 本物の取得は成功時の LastError だけを Stale にし、それは受理される
    assert!(complete(close, &close_handle_leave(1, 6)));
    assert!(complete(close, &close_handle_leave(0, 6)));
}

#[test]
fn queued_completion_not_applicable_needs_an_empty_dequeue() {
    let gqcs = API_GET_QUEUED_COMPLETION_STATUS;
    let none = QueuedCompletion::observe(0, 0, || None).leave_values(258);
    assert_eq!(none[1].capture, ValueCapture::NotApplicable);
    assert!(complete(gqcs, &none));
    assert_eq!(through_trace(gqcs, &none), (true, true));
    // 失敗した I/O のパケット (FALSE だが OVERLAPPED を取り出した) にはバイト数がある
    let mut failed_io = QueuedCompletion::observe(0, 0x1000, || Some(3)).leave_values(38);
    assert!(complete(gqcs, &failed_io));
    failed_io[1] = CapturedValue { capture: ValueCapture::NotApplicable, value: TypedValue::U32(0) };
    assert!(!complete(gqcs, &failed_io));
}

proptest! {
    /// ReadFile の leave: どの結果でも、取得側の書き方を検査器が同じく分類する
    #[test]
    fn read_file_leave_agrees_with_the_checker(
        ret in prop_oneof![Just(0i32), Just(1), Just(-1), any::<i32>()],
        last_error in prop_oneof![Just(0u32), Just(5), Just(ERROR_MORE_DATA), Just(ERROR_IO_PENDING), any::<u32>()],
        asynchronous in any::<bool>(),
        data in proptest::collection::vec(any::<u8>(), 0..24),
        count_ok in any::<bool>(),
    ) {
        let count = data.len() as u32;
        let m = memory(&data, count);
        let count_addr = if count_ok { 0x5000 } else { 0 };
        let v = read_file_leave(&m, ret, 0x4000, 24, count_addr, asynchronous, last_error);
        let outcome = IoOutcome::classify(ret, last_error);
        // 検査器の答えは、取得側が値を取れたか (Unsupported を書かなかったか) と一致する
        let wrote_unsupported = v.iter().any(|x| x.capture == ValueCapture::Unsupported);
        prop_assert_eq!(complete(READ_FILE, &v), !wrote_unsupported, "{:?} {:?}", outcome, v);
        prop_assert_eq!(through_trace(READ_FILE, &v), (!wrote_unsupported, !wrote_unsupported));
        match outcome {
            IoOutcome::Completed | IoOutcome::MoreData if !asynchronous && count_ok => {
                prop_assert_eq!(&v[1], &CapturedValue::captured(TypedValue::U32(count)));
                prop_assert_eq!(&v[2], &CapturedValue::captured(TypedValue::Bytes(data.clone())));
            }
            IoOutcome::Pending => {
                prop_assert_eq!(v[1].capture, ValueCapture::NotApplicable);
                prop_assert_eq!(v[2].capture, ValueCapture::NotApplicable);
            }
            IoOutcome::Failed => prop_assert_eq!(v[2].capture, ValueCapture::NotApplicable),
            IoOutcome::Completed | IoOutcome::MoreData => {}
        }
    }

    /// WriteFile の leave も同じ
    #[test]
    fn write_file_leave_agrees_with_the_checker(
        ret in prop_oneof![Just(0i32), Just(1), any::<i32>()],
        last_error in prop_oneof![Just(0u32), Just(5), Just(ERROR_IO_PENDING), any::<u32>()],
        asynchronous in any::<bool>(),
        count in any::<u32>(),
    ) {
        let m = memory(&[], count);
        let v = write_file_leave(&m, ret, 0x5000, asynchronous, last_error);
        let wrote_unsupported = v.iter().any(|x| x.capture == ValueCapture::Unsupported);
        prop_assert_eq!(complete(WRITE_FILE, &v), !wrote_unsupported, "{:?}", v);
    }

    /// GetQueuedCompletionStatus の leave
    #[test]
    fn queued_completion_leave_agrees_with_the_checker(
        ret in prop_oneof![Just(0i32), Just(1), any::<i32>()],
        dequeued in prop_oneof![Just(0u64), any::<u64>()],
        transferred in proptest::option::of(any::<u32>()),
        last_error in any::<u32>(),
    ) {
        let v = QueuedCompletion::observe(ret, dequeued, || transferred).leave_values(last_error);
        let bad = v.iter().any(|x| matches!(x.capture, ValueCapture::Unsupported | ValueCapture::Unreadable));
        prop_assert_eq!(complete(API_GET_QUEUED_COMPLETION_STATUS, &v), !bad, "{:?}", v);
    }

    /// 完了の観測での ReadFile leave
    #[test]
    fn read_completion_agrees_with_the_checker(
        success in any::<bool>(),
        last_error in prop_oneof![Just(0u32), Just(5), Just(ERROR_MORE_DATA), any::<u32>()],
        data in proptest::collection::vec(any::<u8>(), 0..24),
        known in any::<bool>(),
    ) {
        let m = memory(&data, 0);
        let transferred = known.then_some(data.len() as u32);
        let outcome = IoOutcome::of_completion(success, last_error);
        let v = complete_read_file_leave(&m, 0x4000, 24, transferred, outcome, last_error);
        let wrote_unsupported = v.iter().any(|x| x.capture == ValueCapture::Unsupported);
        prop_assert_eq!(complete(READ_FILE, &v), !wrote_unsupported, "{:?} {:?}", outcome, v);
        if outcome.has_output() && known {
            prop_assert_eq!(&v[2], &CapturedValue::captured(TypedValue::Bytes(data.clone())));
        }
    }

    /// CreateFileW (入力で LastError の意味が決まる): 取得側が Stale と書くのは、conformance が
    /// enter の dwCreationDisposition と戻り値で残留値と認める呼び出しとちょうど同じ。取得側の
    /// 記録を Stale に書き換えると、そうでない呼び出しでは conformance が落とす
    #[test]
    fn create_file_stale_agrees_with_conformance(
        disposition in prop_oneof![1u32..=5, any::<u32>()],
        ret in prop_oneof![Just(u64::MAX), Just(0xffff_ffffu64), Just(0x1234u64), any::<u64>()],
        last_error in prop_oneof![Just(0u32), Just(183), any::<u32>()],
    ) {
        let m = MapMemory::new();
        let enter = create_file_w_enter(&m, 0, 0x8000_0000, 0, 0, disposition, 0x80, 0);
        let leave = create_file_w_leave(ret, last_error, disposition);
        let hook_says_stale = leave[1].capture == ValueCapture::Stale;
        prop_assert_eq!(last_error_inputs(1, &enter, &leave), CheckStatus::Pass);
        let mut forced = leave.clone();
        forced[1].capture = ValueCapture::Stale;
        let want = if hook_says_stale { CheckStatus::Pass } else { CheckStatus::Fail };
        prop_assert_eq!(last_error_inputs(1, &enter, &forced), want, "{:?}", leave);
        // 戻り値だけの必要条件 (失敗した呼び出しの Stale は不完全) も同じ向き
        if !hook_says_stale && (ret == 0 || ret == u64::MAX || ret == 0xffff_ffff) {
            prop_assert!(!complete(1, &forced));
        }
    }
}

#[test]
fn stale_last_error_of_create_always_is_rejected() {
    // CREATE_ALWAYS の成功は ERROR_ALREADY_EXISTS を設定する: Stale と書いた記録は嘘
    let m = MapMemory::new();
    let enter = create_file_w_enter(&m, 0, 0x4000_0000, 0, 0, 2, 0x80, 0);
    let mut leave = create_file_w_leave(0x1234, 183, 2);
    assert_eq!(leave[1].capture, ValueCapture::Captured);
    leave[1].capture = ValueCapture::Stale;
    assert_eq!(last_error_inputs(1, &enter, &leave), CheckStatus::Fail);
    // 戻り値だけでは分からないので、leave 単体の検査は通る (入力の照合は conformance が担う)
    assert!(complete(1, &leave));
    // CREATE_NEW の成功の Stale は正しい
    let enter = create_file_w_enter(&m, 0, 0x4000_0000, 0, 0, 1, 0x80, 0);
    assert_eq!(last_error_inputs(1, &enter, &create_file_w_leave(0x1234, 0, 1)), CheckStatus::Pass);
}
