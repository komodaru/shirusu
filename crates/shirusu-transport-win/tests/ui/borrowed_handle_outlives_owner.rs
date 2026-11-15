// 借りたハンドルは持ち主より長く生きない (閉じた後に使わない)。
use shirusu_transport_win::{BorrowedHandle, OwnedHandle};

fn main() {
    let borrowed: BorrowedHandle<'_>;
    {
        let owner = unsafe { OwnedHandle::from_raw(4) }.unwrap();
        borrowed = owner.borrow();
    }
    let _ = borrowed.raw();
}
