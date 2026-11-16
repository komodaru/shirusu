// レビュー 2026-11-16 P2: 閉じる先のプロセスは値が持つ。別の pid は渡せない。
fn close(h: shirusu_transport_win::RemoteHandle) {
    let _ = shirusu_transport_win::close_remote_handle(1234, h);
}

fn main() {}
