//! cli.rs と coverage_gaps.rs が probe に渡す Trace。

use super::cv;
use shirusu_emit::{file_io_api_set, Emitter, EventMeta, SessionConfig};
use shirusu_trace::schema::TypedValue;
use shirusu_trace::writer::FinalizationInfo;

/// CloseHandle まで記録するかだけが違う、最小の 2 呼び出し Trace。
pub fn trace(close: bool) -> Vec<u8> {
    let mut em = Emitter::new(
        Vec::new(),
        file_io_api_set(),
        SessionConfig::new(1, "cli-test"),
    )
    .unwrap();
    let create = em
        .begin_call(
            1,
            "kernel32!CreateFileW",
            &[
                cv(TypedValue::Str16("x".encode_utf16().collect())),
                cv(TypedValue::U32(0)),
                cv(TypedValue::U32(0)),
                cv(TypedValue::U64(0)),
                cv(TypedValue::U32(3)),
                cv(TypedValue::U32(0)),
                cv(TypedValue::U64(0)),
            ],
            1,
            EventMeta::default(),
        )
        .unwrap();
    em.end_call(
        create,
        &[cv(TypedValue::U64(0x10)), cv(TypedValue::U32(0))],
        2,
        EventMeta::default(),
    )
    .unwrap();
    if close {
        let c = em
            .begin_call(
                1,
                "kernel32!CloseHandle",
                &[cv(TypedValue::U64(0x10))],
                3,
                EventMeta::default(),
            )
            .unwrap();
        em.end_call(
            c,
            &[cv(TypedValue::I32(1)), cv(TypedValue::U32(0))],
            4,
            EventMeta::default(),
        )
        .unwrap();
    }
    em.finish(FinalizationInfo::default()).unwrap()
}
