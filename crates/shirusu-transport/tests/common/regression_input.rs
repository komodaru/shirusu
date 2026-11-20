//! regression_clock_split.rs と regression_reader_hardening.rs が共有する 1 イベントの入力。

use shirusu_transport::ring::EventInput;
use shirusu_transport::{CaptureState, EventKind};

pub fn input(call: u64, payload: &[u8]) -> EventInput<'_> {
    EventInput {
        kind: EventKind::CallEnter,
        api_id: 10,
        schema_id: 20,
        call_id: call,
        parent_call_id: 0,
        payload,
        capture_state: CaptureState::Captured,
        global_order: 0,
    }
}
