//! 低水位時の非同期 Mapping 追加フロー (A70) の受け渡し口。
//!
//! 仕様 (spec-transport.md「生成」): 空きが低水位を下回ったら Agent は待たずに
//! Host へ知らせ、Host が非同期に新しい Mapping を作る。Mapping の Handle と
//! 情報を Agent へ渡し、Agent の管理スレッドが登録を完了して初めて使える。
//! この登録は Hook の中では行わない。Mapping の追加中に Host が止まっても、
//! Agent は既存の Ring で続ける。
//!
//! このモジュールは受け渡しの「口」だけを定義する:
//! - メッセージ型: [`ExpansionRequest`] (Agent → Host)、[`ExpansionDelivery`]
//!   (Host → Agent)。
//! - トレイト: [`ExpansionRequester`] と [`ExpansionDeliverer`]。プロセス内実装は
//!   `std::sync::mpsc::Sender`、Windows プロセス間では実 IPC チャネルをこの
//!   トレイトの背後に差し替える。Host は `target_pid` 付きの要求に対して
//!   `SharedMapping::duplicate_to_process` で作ったハンドル値を
//!   [`DeliveredMapping::Handle`] に載せて配送し、Agent は
//!   `SharedMapping::from_handle_value` で自分の写像を作る。
//! - 記録経路 (record / Hook 相当) はこのフローに触れない。登録は管理スレッド
//!   だけが行う (agent クレートの `ExpansionManager`、host クレートの
//!   `ExpansionService` を参照)。

use crate::shm::Shm;

/// Agent の管理スレッド → Host。新しい Mapping を要求する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpansionRequest {
    /// 追加する Slot 数。現行は 1 固定 (要求のたびに 1 Slot ずつ増やす)。
    pub slots: u32,
    /// Windows プロセス間の場合の配送先プロセス ID。同一プロセス
    /// (in-process) の場合は None で、配送は [`DeliveredMapping::InProcess`] になる。
    pub target_pid: Option<u32>,
}
impl ExpansionRequest {
    /// 同一プロセスの Host への要求 (1 Slot)。
    pub fn new() -> Self {
        Self::default()
    }
    /// 別プロセスの Host への要求 (1 Slot)。Host は Mapping を対象プロセスへ
    /// duplicate してハンドル値で配送する。
    pub fn for_process(pid: u32) -> Self {
        Self {
            slots: 1,
            target_pid: Some(pid),
        }
    }
}
impl Default for ExpansionRequest {
    fn default() -> Self {
        Self {
            slots: 1,
            target_pid: None,
        }
    }
}

/// 配送する Mapping の実体。どちらか一方だけが型で表せる。
#[derive(Debug, Clone)]
pub enum DeliveredMapping {
    /// 同一プロセスの Host が作った領域。実体は Arc で共有されるので、
    /// Host が先に手放しても Agent の利用中は解放されない。
    InProcess(Shm),
    /// 受け取り側プロセスで有効な File Mapping ハンドル (Windows
    /// プロセス間)。Agent は所有権を引き受けて自分の写像を作る。
    Handle(DeliveredHandle),
}

/// 配送されるハンドル値。作るには、値が「この配送を登録するプロセスで
/// 有効な File Mapping ハンドルで、登録する側がそれを所有する (ほかに
/// 閉じる者がいない)」ことを約束する (`unsafe fn new`)。安全な登録 API が
/// 任意の数を閉じたり写像したりしないための型 (レビュー P1-4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveredHandle(usize);
impl DeliveredHandle {
    /// # Safety
    /// `raw` は、この配送を登録するプロセスで有効な File Mapping ハンドルで、
    /// 登録する側に所有権が移ること (配送は 1 回だけ登録する)。
    pub const unsafe fn new(raw: usize) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> usize {
        self.0
    }
}

/// Host → Agent の管理スレッド。新しい Mapping の情報。
#[derive(Debug)]
pub struct ExpansionDelivery {
    /// Mapping の大きさ (Ring 1 本分 = `RingConfig::required_len()`)。
    pub len: usize,
    pub mapping: DeliveredMapping,
    /// Control 領域に書き込む Generation。追加 Slot の世代
    /// ([`EXPANDED_SLOT_GENERATION`]) と一致すること。
    pub generation: u64,
}

/// 新しい Slot の初回 Generation。追加 Slot は未使用のため 0 から始める。
/// (初期 Pool の Slot は generation 1 から始まる。追加 Slot は「まだ誰も
/// 使っていない」ことを 0 で表す。Host はこの世代を期待値として検証する。)
pub const EXPANDED_SLOT_GENERATION: u64 = 0;

/// Agent の管理スレッドが使う、Host への要求口。
pub trait ExpansionRequester: Send {
    /// 拡張を要求する。受け手 (Host 側) が無くなっている場合は、要求は
    /// 届かないものとして無視してよい (管理スレッドは配送側の切断で
    /// Host 停止を検知し、要求を破棄する)。
    fn request_expansion(&self, req: ExpansionRequest);
}
impl ExpansionRequester for std::sync::mpsc::Sender<ExpansionRequest> {
    fn request_expansion(&self, req: ExpansionRequest) {
        let _ = self.send(req);
    }
}

/// Host 側が使う、Agent への配送口。
pub trait ExpansionDeliverer: Send {
    /// 新しい Mapping を配送する。受け手 (Agent の管理スレッド) が無くなって
    /// いる場合は、配送は届かないものとして無視してよい (Mapping 自体は
    /// 呼び出し側が保持し続けるため、配送の失敗自体は無害)。
    fn deliver(&self, delivery: ExpansionDelivery);
}
impl ExpansionDeliverer for std::sync::mpsc::Sender<ExpansionDelivery> {
    fn deliver(&self, delivery: ExpansionDelivery) {
        let _ = self.send(delivery);
    }
}
