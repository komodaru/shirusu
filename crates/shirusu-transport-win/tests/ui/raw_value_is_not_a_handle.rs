// 数値は閉じる対象にならない (旧 close_handle_value(usize) の置き換え)。
use shirusu_transport_win::OwnedHandle;

fn main() {
    let handle: OwnedHandle = 4usize;
    drop(handle);
}
