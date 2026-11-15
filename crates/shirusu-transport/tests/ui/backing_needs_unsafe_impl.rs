// 任意のポインタを Shm にするには、unsafe impl で領域の有効性を約束する。
use shirusu_transport::Backing;
use std::ptr::NonNull;

struct Raw(usize);
impl Backing for Raw {
    fn base(&self) -> NonNull<u8> {
        NonNull::new(self.0 as *mut u8).unwrap()
    }
    fn byte_len(&self) -> usize {
        4096
    }
}

fn main() {}
