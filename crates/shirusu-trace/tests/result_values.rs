//! d4fc3c0 レビューの値の読み方に関する回帰:
//! - 2.1: 同じ意味の非 NULL 戻り値なら、U64 / Ptr32 / Ptr64 で LastError の判定が食い違わない。
//!   復号済みの値 (`ResultFacts::of_values`) と値列の直接走査 (`validate_values_completeness`) が
//!   同じ答えになる。
//! - 1.3: 取得状態が NotApplicable / Unreadable / Unsupported の BlobRef はダミーで参照ではない。
//!   Writer は受理し、`blob_refs` は数えない。Truncated / LimitExceeded / Stale / Captured は参照。
//! - 4: 借りて読む値列 (`values_ref`) は所有権付きの復号と同じ値・同じ受理。
use proptest::prelude::*;
use shirusu_trace::compact::{decode_values, encode_values, validate_values, validate_values_completeness, values_ref};
use shirusu_trace::model::{EventBatch, PayloadRef, TraceEvent};
use shirusu_trace::proto::{CaptureEnvironment, SessionMetadata};
use shirusu_trace::refs::EventBlobRefs;
use shirusu_trace::schema::{
    make_schema, CaptureTiming, CapturedValue, EventCapture, FieldRoles, LastErrorMeaning, OutputCondition,
    ResultFacts, SchemaRegistry, Ty, TypedValue, ValueCapture, ValueKind,
};
use shirusu_trace::{BlobId, FinalizationInfo, TraceWriter};

fn after() -> u32 {
    CaptureTiming::After.flags() | ValueKind::Scalar.flags()
}

/// leave `[return: ty, last_error: U32]`、last_error は WhenReturnIsNull
fn registry(ty: Ty) -> SchemaRegistry {
    let mut r = SchemaRegistry::new();
    r.register(make_schema(
        7,
        "k32!F/leave",
        vec![
            ("return".into(), ty, after()),
            ("last_error".into(), Ty::U32, after() | LastErrorMeaning::WhenReturnIsNull.flags()),
        ],
    ))
    .unwrap();
    r
}

fn ret_value(ty: Ty, bits: u64) -> TypedValue {
    match ty {
        Ty::U64 => TypedValue::U64(bits),
        Ty::Ptr32 => TypedValue::Ptr32(bits as u32),
        Ty::Ptr64 => TypedValue::Ptr64(bits),
        other => panic!("not a pointer-sized return: {other:?}"),
    }
}

/// 期待値: WhenReturnIsNull の Stale は、戻り値が非 NULL なら正しい
fn judged(ty: Ty, bits: u64, last_error: ValueCapture) -> (bool, bool) {
    let r = registry(ty);
    let values = [
        CapturedValue::captured(ret_value(ty, bits)),
        CapturedValue { capture: last_error, value: TypedValue::U32(5) },
    ];
    let bytes = encode_values(&[ty, Ty::U32], &values).unwrap();
    let scanned = validate_values_completeness(r.shape_ref(7).unwrap(), &bytes).unwrap();
    let def = r.get(7).unwrap();
    let decoded = decode_values(&[ty, Ty::U32], &bytes).unwrap();
    let facts = ResultFacts::of_values(FieldRoles::of(&def.fields), &decoded);
    let by_value = decoded
        .iter()
        .zip(&def.fields)
        .all(|(v, f)| v.capture.is_complete_given(f.flags, &facts));
    (scanned, by_value)
}

#[test]
fn pointer_returns_decide_last_error_like_integers() {
    // レビューの再現: 非 NULL の戻り値と正しい Stale
    for ty in [Ty::U64, Ty::Ptr32, Ty::Ptr64] {
        assert_eq!(judged(ty, 0x1000, ValueCapture::Stale), (true, true), "{ty:?}");
        // NULL を返したのに Stale は誤り
        assert_eq!(judged(ty, 0, ValueCapture::Stale), (false, false), "{ty:?}");
    }
}

proptest! {
    #[test]
    fn pointer_and_integer_returns_agree(bits in prop_oneof![Just(0u64), 1u64..=u32::MAX as u64], stale in any::<bool>()) {
        let c = if stale { ValueCapture::Stale } else { ValueCapture::Captured };
        let want = judged(Ty::U64, bits, c);
        // 走査と復号は同じ答え
        prop_assert_eq!(want.0, want.1);
        prop_assert_eq!(judged(Ty::Ptr32, bits, c), want);
        prop_assert_eq!(judged(Ty::Ptr64, bits, c), want);
    }

    /// 借りて読む値列と所有権付きの復号は、同じ入力を受理し同じ値を返す (どんなバイト列でも)
    #[test]
    fn borrowed_reader_matches_decode(
        types in proptest::collection::vec(prop_oneof![
            Just(Ty::I32), Just(Ty::I64), Just(Ty::U32), Just(Ty::U64), Just(Ty::F32), Just(Ty::F64),
            Just(Ty::Bool), Just(Ty::Str), Just(Ty::Str16), Just(Ty::Bytes), Just(Ty::BlobRef),
            Just(Ty::Ptr32), Just(Ty::Ptr64),
        ], 0..5),
        data in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let owned = decode_values(&types, &data);
        let borrowed: Result<Vec<CapturedValue>, _> = values_ref(types.iter().copied(), &data)
            .map(|v| v.map(|v| CapturedValue { capture: v.capture, value: v.value.to_owned_value() }))
            .collect();
        prop_assert_eq!(&owned, &borrowed);
        prop_assert_eq!(owned.is_ok(), validate_values(&types, &data).is_ok());
    }
}

/// 成功時だけ定義される BlobRef 出力を持つ leave (`[return: I32, out: BlobRef]`)
fn blob_out_writer() -> TraceWriter<Vec<u8>> {
    let env = CaptureEnvironment {
        session: SessionMetadata { session_id: 4, created_by: "result-values".into(), qpc_frequency: 1000 },
        ..Default::default()
    };
    let mut w = TraceWriter::new(Vec::new(), env).unwrap();
    w.register_schema(make_schema(
        3,
        "k32!G/leave",
        vec![
            ("return".into(), Ty::I32, after()),
            ("out".into(), Ty::BlobRef, after() | OutputCondition::WhenReturnIsTrue.flags()),
        ],
    ))
    .unwrap();
    w
}

fn leave(ret: i32, out: CapturedValue) -> TraceEvent {
    TraceEvent {
        kind: 2,
        sequence: 1,
        timestamp_qpc: 1,
        call_id: 1,
        parent_call_id: 0,
        api_id: 1,
        schema_id: 3,
        capture: EventCapture::Captured,
        payload: PayloadRef::None,
        args: Some(encode_values(&[Ty::I32, Ty::BlobRef], &[CapturedValue::captured(TypedValue::I32(ret)), out]).unwrap()),
        global_order: None,
    }
}

fn batch(e: TraceEvent) -> EventBatch {
    EventBatch { session_id: 4, process_id: 1, raw_pid: 1, thread_id: 1, qpc_frequency: 1000, events: vec![e] }
}

#[test]
fn dummy_blob_refs_are_not_references() {
    let zero = BlobId::from_bytes([0; 32]);
    for capture in [ValueCapture::NotApplicable, ValueCapture::Unreadable, ValueCapture::Unsupported] {
        let mut w = blob_out_writer();
        let e = leave(0, CapturedValue { capture, value: TypedValue::BlobRef(zero) });
        // レビューの再現: FALSE を返した leave の NotApplicable 出力。Writer は受理する
        w.write_batch(&batch(e)).unwrap_or_else(|err| panic!("{capture:?}: {err}"));
        let t = shirusu_trace::read_trace(&w.finish(FinalizationInfo::default()).unwrap()).unwrap();
        let e = &t.batches[0].events[0];
        assert_eq!(e.blob_refs(&t.registry), EventBlobRefs::Known(Vec::new()), "{capture:?}");
    }
}

#[test]
fn partial_blob_refs_still_refer() {
    for capture in [ValueCapture::Captured, ValueCapture::Truncated, ValueCapture::LimitExceeded, ValueCapture::Stale] {
        let mut w = blob_out_writer();
        let id = w.add_payload(b"partial").unwrap();
        w.write_batch(&batch(leave(1, CapturedValue { capture, value: TypedValue::BlobRef(id) }))).unwrap();
        let t = shirusu_trace::read_trace(&w.finish(FinalizationInfo::default()).unwrap()).unwrap();
        let refs = t.batches[0].events[0].blob_refs(&t.registry);
        assert_eq!(refs.known().iter().map(|u| u.id).collect::<Vec<_>>(), [id], "{capture:?}");
        // 参照先がなければ Writer は拒む (部分取得した内容を参照なしと扱わない)
        let mut w = blob_out_writer();
        let missing = BlobId::of_content(b"never added");
        assert!(w.write_batch(&batch(leave(1, CapturedValue { capture, value: TypedValue::BlobRef(missing) }))).is_err(), "{capture:?}");
    }
}
