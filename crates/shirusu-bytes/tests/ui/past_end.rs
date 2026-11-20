// A fixed read that reaches past its record (4 bytes at 13 of 16) is refused when it is built.
// (In a const item, so `cargo check`, which trybuild runs, evaluates it too; in a function body
// the assertion fires when the function is compiled to code, i.e. in `cargo build`/`cargo test`.)
const PAST: u32 = shirusu_bytes::u32_at::<13, 16>(&[0u8; 16]);
fn main() {
    let _ = PAST;
}
