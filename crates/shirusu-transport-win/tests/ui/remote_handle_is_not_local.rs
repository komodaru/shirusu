// 別プロセスのハンドル値は、このプロセスのハンドルとしては使えない。
use shirusu_transport_win::{BorrowedHandle, RemoteHandle};

fn wait(_: BorrowedHandle<'_>) {}

fn give(remote: RemoteHandle) {
    wait(remote);
}

fn main() {}
