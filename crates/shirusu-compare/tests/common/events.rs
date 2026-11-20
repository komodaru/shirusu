//! invalid_handle_width.rs と value_kinds.rs が共有する、引数付きイベントの列（どちらも
//! ValueKind の印を付けた自前のスキーマで Trace を組み、イベントの積み方だけが同じ）。

use shirusu_trace::compact::encode_values;
use shirusu_trace::model::TraceEvent;
use shirusu_trace::schema::{CapturedValue, EventCapture, Ty};
use shirusu_trace::PayloadRef;

pub struct Events {
    pub events: Vec<TraceEvent>,
    seq: u64,
}

impl Events {
    pub fn new() -> Self {
        Events {
            events: Vec::new(),
            seq: 0,
        }
    }
    /// sequence を 1 から振り、timestamp は 1000 + 10 * sequence。
    pub fn push(
        &mut self,
        kind: u16,
        call: u64,
        api_id: u32,
        schema_id: u32,
        types: &[Ty],
        values: Vec<CapturedValue>,
    ) {
        self.seq += 1;
        self.events.push(TraceEvent {
            kind,
            sequence: self.seq,
            timestamp_qpc: 1000 + self.seq * 10,
            call_id: call,
            parent_call_id: 0,
            api_id,
            schema_id,
            capture: EventCapture::Captured,
            payload: PayloadRef::None,
            args: Some(encode_values(types, &values).unwrap()),
            global_order: None,
        });
    }
}
