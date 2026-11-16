// レビュー 2026-11-16 P2: RemoteHandle は複製した API だけが作る。数値と
// プロセス ID を外から組み合わせて作ることはできない。
use shirusu_transport_win::RemoteHandle;

fn main() {
    let _ = RemoteHandle {
        raw: 4,
        target_pid: 1,
    };
}
