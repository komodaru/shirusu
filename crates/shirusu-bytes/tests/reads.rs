use proptest::prelude::*;
use shirusu_bytes::{array_at, read_array, read_u16, read_u32, read_u64, u16_at, u32_at, u64_at};

#[test]
fn fixed_places_read_little_endian() {
    let r: [u8; 16] = core::array::from_fn(|i| i as u8);
    assert_eq!(u16_at::<0, 16>(&r), 0x0100);
    assert_eq!(u32_at::<12, 16>(&r), 0x0f0e_0d0c);
    assert_eq!(u64_at::<8, 16>(&r), 0x0f0e_0d0c_0b0a_0908);
    assert_eq!(array_at::<15, 1, 16>(&r), [15]);
    assert_eq!(array_at::<16, 0, 16>(&r), [0u8; 0]);
}

proptest! {
    // Each read from data is the slice's bytes when they all lie in it, None otherwise (never a
    // panic, also for offsets near usize::MAX).
    #[test]
    fn reads_from_data_match_the_slice(bytes in proptest::collection::vec(any::<u8>(), 0..40), at in prop_oneof![0usize..48, (usize::MAX - 8)..=usize::MAX]) {
        let want = |l: usize| at.checked_add(l).filter(|&e| e <= bytes.len()).map(|e| bytes[at..e].to_vec());
        prop_assert_eq!(read_array::<3>(&bytes, at).map(|a| a.to_vec()), want(3));
        prop_assert_eq!(read_u16(&bytes, at).map(|v| v.to_le_bytes().to_vec()), want(2));
        prop_assert_eq!(read_u32(&bytes, at).map(|v| v.to_le_bytes().to_vec()), want(4));
        prop_assert_eq!(read_u64(&bytes, at).map(|v| v.to_le_bytes().to_vec()), want(8));
    }
}
