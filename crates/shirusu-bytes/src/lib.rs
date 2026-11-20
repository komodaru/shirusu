//! Little-endian reads from byte records without `try_into().unwrap()`.
//!
//! Two kinds of place:
//! - fixed (a record's layout, a constant offset into a `[u8; N]`): [`array_at`], [`u16_at`],
//!   [`u32_at`], [`u64_at`]. `AT + L <= N` is a compile-time assertion, so a read past the record
//!   does not build (`cargo build`; tests/ui/past_end.rs) and the read itself cannot fail.
//! - from data (an offset read from the record or a cursor): [`read_array`], [`read_u16`],
//!   [`read_u32`], [`read_u64`]. A place past the end is `None`, for the caller's own error.
#![no_std]

/// The `L` bytes at `AT` of a record of `N` bytes. `AT + L <= N` is checked when it is built.
#[inline(always)]
pub const fn array_at<const AT: usize, const L: usize, const N: usize>(
    record: &[u8; N],
) -> [u8; L] {
    const {
        assert!(
            AT <= N && L <= N - AT,
            "the read lies past the record's end"
        )
    };
    let mut out = [0u8; L];
    let mut i = 0;
    while i < L {
        out[i] = record[AT + i];
        i += 1;
    }
    out
}

/// The little-endian u16 at `AT` of a record of `N` bytes.
#[inline(always)]
pub const fn u16_at<const AT: usize, const N: usize>(record: &[u8; N]) -> u16 {
    u16::from_le_bytes(array_at::<AT, 2, N>(record))
}

/// The little-endian u32 at `AT` of a record of `N` bytes.
#[inline(always)]
pub const fn u32_at<const AT: usize, const N: usize>(record: &[u8; N]) -> u32 {
    u32::from_le_bytes(array_at::<AT, 4, N>(record))
}

/// The little-endian u64 at `AT` of a record of `N` bytes.
#[inline(always)]
pub const fn u64_at<const AT: usize, const N: usize>(record: &[u8; N]) -> u64 {
    u64::from_le_bytes(array_at::<AT, 8, N>(record))
}

/// The `L` bytes at `at` of `bytes`, or `None` when they do not all lie in it.
#[inline]
pub fn read_array<const L: usize>(bytes: &[u8], at: usize) -> Option<[u8; L]> {
    bytes.get(at..)?.first_chunk::<L>().copied()
}

/// The little-endian u16 at `at`, or `None` past the end.
#[inline]
pub fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    read_array(bytes, at).map(u16::from_le_bytes)
}

/// The little-endian u32 at `at`, or `None` past the end.
#[inline]
pub fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    read_array(bytes, at).map(u32::from_le_bytes)
}

/// The little-endian u64 at `at`, or `None` past the end.
#[inline]
pub fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    read_array(bytes, at).map(u64::from_le_bytes)
}
