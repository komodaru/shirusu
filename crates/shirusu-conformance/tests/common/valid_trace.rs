//! cli.rs と coverage_gaps.rs が CLI に渡す、適合判定を通る最小の Trace。

use shirusu_trace::model::{EventBatch, PayloadRef, TraceEvent};
use shirusu_trace::proto::{CaptureEnvironment, SessionMetadata};
use shirusu_trace::schema::{make_schema, EventCapture, Ty};
use shirusu_trace::writer::{FinalizationInfo, TraceWriter};

pub fn valid_trace_bytes() -> Vec<u8> {
    let env = CaptureEnvironment {
        session: SessionMetadata {
            session_id: 3,
            created_by: "cli-test writer".into(),
            qpc_frequency: 10_000_000,
        },
        ..Default::default()
    };
    let mut w = TraceWriter::new(Vec::new(), env).unwrap();
    w.register_schema(make_schema(1, "x!Y/enter", vec![("h".into(), Ty::U64, 0)]))
        .unwrap();
    w.register_schema(make_schema(
        2,
        "x!Y/leave",
        vec![
            ("return".into(), Ty::U64, 0),
            ("last_error".into(), Ty::U32, 0),
        ],
    ))
    .unwrap();
    let ev = |kind, sequence| TraceEvent {
        kind,
        sequence,
        timestamp_qpc: sequence * 10,
        call_id: 1,
        parent_call_id: 0,
        api_id: 1,
        schema_id: if kind == 1 { 1 } else { 2 },
        capture: EventCapture::Captured,
        payload: PayloadRef::None,
        args: None,
        global_order: None,
    };
    w.write_batch(&EventBatch {
        session_id: 3,
        process_id: 1,
        raw_pid: 10,
        thread_id: 1,
        qpc_frequency: 10_000_000,
        events: vec![ev(1, 1), ev(2, 2)],
    })
    .unwrap();
    w.finish(FinalizationInfo::default()).unwrap()
}
