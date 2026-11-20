//! Development benchmark of encoding/validation, not DLL injection or GUI performance.
//! cargo run -p shirusu-hook --release --example encoding-cost
use shirusu_hook::{api::api_by_name, capture};
use shirusu_trace::{
    compact::{decode_values, encode_values, validate_values},
    schema::{schema_types, SchemaRegistry},
};
use std::{hint::black_box, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api = api_by_name("HeapAlloc").ok_or("HeapAlloc is not in the API table")?;
    let generic =
        shirusu_hook::api::generic_api(api.api_id).ok_or("HeapAlloc has no generic table entry")?;
    let values = capture::generic_enter(generic, &[0x1234, 0, 64]);
    let iterations = 100_000;
    for round in 0..5 {
        let mut times = [0.0; 2];
        for mode in [round % 2, 1 - round % 2] {
            let start = Instant::now();
            for _ in 0..iterations {
                let bytes = if mode == 0 {
                    let schema = black_box(api).enter_schema();
                    let types = schema_types(&schema).ok_or("unknown field type")?;
                    encode_values(&types, black_box(&values))?
                } else {
                    capture::encode_enter(black_box(api), black_box(&values))?
                };
                black_box(bytes);
            }
            times[mode] = start.elapsed().as_nanos() as f64 / iterations as f64;
        }
        println!(
            "round={round} schema_per_call_ns={:.1} direct_fields_ns={:.1} ratio={:.3}",
            times[0],
            times[1],
            times[1] / times[0]
        );
    }
    let schema = api.enter_schema();
    let payload = capture::encode_enter(api, &values)?;
    let mut registry = SchemaRegistry::new();
    registry.register(schema.clone())?;
    for round in 0..5 {
        let mut times = [0.0; 2];
        for mode in [round % 2, 1 - round % 2] {
            let start = Instant::now();
            for _ in 0..iterations {
                if mode == 0 {
                    let types = schema_types(black_box(&schema)).ok_or("unknown field type")?;
                    black_box(decode_values(&types, black_box(&payload))?);
                } else {
                    let types = black_box(&registry)
                        .types_ref(schema.schema_id)
                        .ok_or("schema not registered")?;
                    validate_values(types, black_box(&payload))?;
                }
            }
            times[mode] = start.elapsed().as_nanos() as f64 / iterations as f64;
        }
        println!(
            "round={round} allocating_validation_ns={:.1} borrowed_validation_ns={:.1} ratio={:.3}",
            times[0],
            times[1],
            times[1] / times[0]
        );
    }
    let raw_args = [0x1234, 0, 64];
    for round in 0..5 {
        let mut times = [0.0; 2];
        for mode in [round % 2, 1 - round % 2] {
            let start = Instant::now();
            for _ in 0..iterations {
                if mode == 0 {
                    let values = capture::generic_enter(black_box(generic), black_box(&raw_args));
                    black_box(capture::encode_enter(api, &values)?);
                } else {
                    let bytes = capture::encode_generic_enter::<3>(
                        black_box(generic),
                        black_box(&raw_args),
                    );
                    black_box(bytes.as_bytes());
                }
            }
            times[mode] = start.elapsed().as_nanos() as f64 / iterations as f64;
        }
        println!(
            "round={round} allocating_capture_ns={:.1} stack_capture_ns={:.1} ratio={:.3}",
            times[0],
            times[1],
            times[1] / times[0]
        );
    }
    Ok(())
}
