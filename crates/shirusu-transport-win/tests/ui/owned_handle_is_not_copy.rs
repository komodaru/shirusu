// 所有するハンドルは複製できない (二重に閉じない)。
use shirusu_transport_win::OwnedHandle;

fn close(_: OwnedHandle) {}

fn main() {
    let handle = unsafe { OwnedHandle::from_raw(4) }.unwrap();
    close(handle);
    close(handle);
}
