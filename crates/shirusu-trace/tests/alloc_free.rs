//! d4fc3c0 レビュー 4 の性能回帰: 値列の検証 (`validate_values` / `validate_values_completeness`) は
//! ヒープに割り当てない。Blob 参照の列挙 (`blob_refs`) は BlobRef を含まない大きな引数を複製しない
//! (以前は 1 MiB の Bytes で 2 回・1,048,672 バイトを要求した)。大域アロケータで要求を数える。
//! libtest を使わない (harness = false): 測っている間に別のスレッドが割り当てない。
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use shirusu_trace::compact::{encode_values, validate_values, validate_values_completeness};
use shirusu_trace::model::{PayloadRef, TraceEvent};
use shirusu_trace::schema::{
    make_schema, CaptureTiming, CapturedValue, EventCapture, LastErrorMeaning, SchemaRegistry, Ty, TypedValue,
    ValueCapture, ValueKind,
};

struct Counting;

static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: System へそのまま渡し、数えるだけ
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: 呼び出し側の約束をそのまま渡す
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: 同上
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: 同上
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// `f` の間の (回数, バイト数)
fn measure(f: impl FnOnce()) -> (u64, u64) {
    let (c, b) = (CALLS.load(Ordering::SeqCst), BYTES.load(Ordering::SeqCst));
    f();
    (CALLS.load(Ordering::SeqCst) - c, BYTES.load(Ordering::SeqCst) - b)
}

fn main() {
    let after = CaptureTiming::After.flags() | ValueKind::Scalar.flags();
    let mut r = SchemaRegistry::new();
    r.register(make_schema(
        1,
        "k32!R/leave",
        vec![
            ("return".into(), Ty::I32, after),
            ("data".into(), Ty::Bytes, after),
            ("last_error".into(), Ty::U32, after | LastErrorMeaning::WhenReturnIsFalse.flags()),
        ],
    ))
    .expect("schema");
    let types = [Ty::I32, Ty::Bytes, Ty::U32];
    let big = vec![0xa5u8; 1 << 20];
    let args = encode_values(
        &types,
        &[
            CapturedValue::captured(TypedValue::I32(1)),
            CapturedValue::captured(TypedValue::Bytes(big)),
            CapturedValue { capture: ValueCapture::Stale, value: TypedValue::U32(5) },
        ],
    )
    .expect("encode");
    let shapes = r.shape_ref(1).expect("shape");

    let (n, bytes) = measure(|| validate_values(&types, &args).expect("valid"));
    assert_eq!((n, bytes), (0, 0), "validate_values allocates");
    let (n, bytes) = measure(|| assert!(validate_values_completeness(shapes, &args).expect("valid")));
    assert_eq!((n, bytes), (0, 0), "validate_values_completeness allocates");

    let e = TraceEvent {
        kind: 2,
        sequence: 1,
        timestamp_qpc: 1,
        call_id: 1,
        parent_call_id: 0,
        api_id: 1,
        schema_id: 1,
        capture: EventCapture::Captured,
        payload: PayloadRef::None,
        args: Some(args),
        global_order: None,
    };
    let (n, bytes) = measure(|| {
        let refs = e.blob_refs(&r);
        assert!(refs.known().is_empty());
    });
    // 参照がないので結果の Vec も伸びない: 割り当ては 0
    assert_eq!((n, bytes), (0, 0), "blob_refs copies the arguments");
    println!("alloc_free: ok (validate 0, completeness 0, blob_refs 0)");
}
