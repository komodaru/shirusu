//! golden.rs と rules.rs が共有する合成 Trace ビルダ（kernel32 の CreateFileW / ReadFile /
//! CloseHandle を記録する）。prereq.rs のビルダは call_id の振り方とスキーマの値の種類が
//! 違う（親子呼び出しと値の種類を検査する）ので別に持つ。

use shirusu_trace::compact::encode_values;
use shirusu_trace::model::{EventBatch, TraceEvent};
use shirusu_trace::proto::{CaptureEnvironment, SessionMetadata};
use shirusu_trace::reader::ReadTrace;
use shirusu_trace::schema::{make_schema, CapturedValue, EventCapture, Ty, TypedValue};
use shirusu_trace::writer::FinalizationInfo;
use shirusu_trace::{PayloadRef, TraceWriter};

pub fn cv(v: TypedValue) -> CapturedValue {
    CapturedValue::captured(v)
}

pub struct B {
    pub events: Vec<TraceEvent>,
    seq: u64,
}

impl B {
    pub fn new() -> Self {
        B {
            events: Vec::new(),
            seq: 0,
        }
    }
    pub fn push(
        &mut self,
        kind: u16,
        api_id: u32,
        schema_id: u32,
        types: &[Ty],
        values: Vec<CapturedValue>,
        payload: PayloadRef,
    ) {
        self.seq += 1;
        self.events.push(TraceEvent {
            kind,
            sequence: self.seq,
            timestamp_qpc: 100 + self.seq * 10,
            call_id: if kind == 2 { self.seq - 1 } else { self.seq },
            parent_call_id: 0,
            api_id,
            schema_id,
            capture: EventCapture::Captured,
            payload,
            args: Some(encode_values(types, &values).unwrap()),
            global_order: None,
        });
    }
    pub fn create(&mut self, handle: u64, path: &str) {
        self.push(
            1,
            1,
            1,
            &[Ty::Str16, Ty::U32],
            vec![
                cv(TypedValue::Str16(path.encode_utf16().collect())),
                cv(TypedValue::U32(0x8000_0000)),
            ],
            PayloadRef::None,
        );
        self.push(
            2,
            1,
            2,
            &[Ty::U64, Ty::U32],
            vec![cv(TypedValue::U64(handle)), cv(TypedValue::U32(0))],
            PayloadRef::None,
        );
    }
    pub fn read(&mut self, handle: u64, want: u32, payload: PayloadRef) {
        self.push(
            1,
            2,
            3,
            &[Ty::U64, Ty::U32],
            vec![cv(TypedValue::U64(handle)), cv(TypedValue::U32(want))],
            payload,
        );
        self.push(
            2,
            2,
            4,
            &[Ty::I32, Ty::U32],
            vec![cv(TypedValue::I32(1)), cv(TypedValue::U32(want))],
            PayloadRef::None,
        );
    }
    pub fn close(&mut self, handle: u64) {
        self.push(
            1,
            4,
            7,
            &[Ty::U64],
            vec![cv(TypedValue::U64(handle))],
            PayloadRef::None,
        );
        self.push(
            2,
            4,
            8,
            &[Ty::I32, Ty::U32],
            vec![cv(TypedValue::I32(1)), cv(TypedValue::U32(0))],
            PayloadRef::None,
        );
    }
    /// the trace file: the six kernel32 schemas, the events in one batch, finalized with `fin`
    pub fn bytes(self, env: CaptureEnvironment, fin: FinalizationInfo) -> Vec<u8> {
        let mut w = TraceWriter::new(Vec::new(), env).unwrap();
        for def in [
            make_schema(
                1,
                "kernel32!CreateFileW/enter",
                vec![
                    ("lpFileName".into(), Ty::Str16, 0),
                    ("dwDesiredAccess".into(), Ty::U32, 0),
                ],
            ),
            make_schema(
                2,
                "kernel32!CreateFileW/leave",
                vec![
                    ("return".into(), Ty::U64, 0),
                    ("last_error".into(), Ty::U32, 0),
                ],
            ),
            make_schema(
                3,
                "kernel32!ReadFile/enter",
                vec![
                    ("hFile".into(), Ty::U64, 0),
                    ("nNumberOfBytesToRead".into(), Ty::U32, 0),
                ],
            ),
            make_schema(
                4,
                "kernel32!ReadFile/leave",
                vec![
                    ("return".into(), Ty::I32, 0),
                    ("lpNumberOfBytesRead".into(), Ty::U32, 0),
                ],
            ),
            make_schema(
                7,
                "kernel32!CloseHandle/enter",
                vec![("hObject".into(), Ty::U64, 0)],
            ),
            make_schema(
                8,
                "kernel32!CloseHandle/leave",
                vec![
                    ("return".into(), Ty::I32, 0),
                    ("last_error".into(), Ty::U32, 0),
                ],
            ),
        ] {
            w.register_schema(def).unwrap();
        }
        if !self.events.is_empty() {
            w.write_batch(&EventBatch {
                session_id: 1,
                process_id: 1,
                raw_pid: 10,
                thread_id: 1,
                qpc_frequency: 10_000_000,
                events: self.events,
            })
            .unwrap();
        }
        w.finish(fin).unwrap()
    }
    pub fn finish(self, env: CaptureEnvironment, fin: FinalizationInfo) -> ReadTrace {
        shirusu_trace::read_trace(&self.bytes(env, fin)).unwrap()
    }
}

/// a capture environment with only its session (`created_by` names the test)
pub fn session(created_by: &str) -> CaptureEnvironment {
    CaptureEnvironment {
        session: SessionMetadata {
            session_id: 1,
            created_by: created_by.into(),
            qpc_frequency: 10_000_000,
        },
        ..Default::default()
    }
}
