//! Slot を持てなかったスレッドへ、別のスレッドが作った値 (Ring の状態) を
//! 渡す受け渡し口。記録中のスレッドが使う `request` / `take` はロックも
//! Heap も使わない。
//!
//! 口ごとの状態機械 (1 語の CAS で遷移し、`Ticket` が要求ごとの識別子):
//!
//! ```text
//! Empty(t) --request--> Requested(t, tid) --begin_fill--> Filling(t, tid)
//!    ^                        |    ^                         |       |
//!    |                  clear |    +------- Fill の Drop ----+       | offer
//!    +--------- t+1 ----------+                                      v
//!    +--------- t+1 --- take / clear --- Claimed(t, tid) <--- Offered(t, tid)
//!    +--------- t+1 --- offer / Fill の Drop --- Cancelled(t, tid) <-- clear (Filling 中)
//! ```
//!
//! 値の置き場は `Filling` / `Claimed` を作ったスレッドだけが触る。Empty へ
//! 戻すたびに Ticket を進めるので、取り消して出し直した要求へ古い
//! `Request` で値を置くことはできない (ABA を起こさない)。
//!
//! この file は shirusu-loom-tests からも `#[path]` で読み込む。原子と
//! UnsafeCell は親 module の `adoption_sync` から取る (本体は std、loom 側は
//! loom の型)。

use super::adoption_sync::{AtomicU64, Ordering, UnsafeCell};

/// 受け渡し口の数。埋まっていれば要求は置けず、そのスレッドは記録されない
/// まま (欠落として数える) 次の機会を待つ。
pub const ADOPTION_CELLS: usize = 64;

/// 受け取り手の OS スレッド ID。Windows のスレッド ID は 32 bit なので、
/// それを超える ID は受け渡し口を使えない (`AdoptTid::new` が None)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AdoptTid(u32);
impl AdoptTid {
    pub fn new(thread_id: u64) -> Option<Self> {
        u32::try_from(thread_id).ok().map(Self)
    }
    pub fn get(self) -> u64 {
        u64::from(self.0)
    }
}

/// 口が空に戻るたびに進む 29 bit の番号。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ticket(u32);
impl Ticket {
    const BITS: u32 = 29;
    const MASK: u32 = (1 << Self::BITS) - 1;
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1) & Self::MASK)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Empty,
    Requested,
    Filling,
    Offered,
    Claimed,
    Cancelled,
}
impl Phase {
    const BITS: u32 = 3;
    const fn code(self) -> u64 {
        match self {
            Phase::Empty => 0,
            Phase::Requested => 1,
            Phase::Filling => 2,
            Phase::Offered => 3,
            Phase::Claimed => 4,
            Phase::Cancelled => 5,
        }
    }
    fn from_code(code: u64) -> Self {
        match code {
            0 => Phase::Empty,
            1 => Phase::Requested,
            2 => Phase::Filling,
            3 => Phase::Offered,
            4 => Phase::Claimed,
            5 => Phase::Cancelled,
            // 6 と 7 は書かれない (pack は上の 6 値しか作らない)。
            other => unreachable!("adoption phase code {other}"),
        }
    }
}
const _: () = assert!(Phase::BITS + Ticket::BITS == 32);

/// 口の状態 1 語分。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CellWord {
    phase: Phase,
    ticket: Ticket,
    /// Empty では 0。
    tid: u32,
}
impl CellWord {
    const EMPTY: Self = Self {
        phase: Phase::Empty,
        ticket: Ticket(0),
        tid: 0,
    };
    fn pack(self) -> u64 {
        self.phase.code() | (u64::from(self.ticket.0) << Phase::BITS) | (u64::from(self.tid) << 32)
    }
    fn unpack(word: u64) -> Self {
        let low = word & u64::from(u32::MAX);
        Self {
            phase: Phase::from_code(low & ((1 << Phase::BITS) - 1)),
            ticket: Ticket((low >> Phase::BITS) as u32 & Ticket::MASK),
            tid: (word >> 32) as u32,
        }
    }
    fn with(self, phase: Phase) -> Self {
        Self { phase, ..self }
    }
    /// 次の要求のための空き (Ticket を進める)。
    fn emptied(self) -> Self {
        Self {
            ticket: self.ticket.next(),
            ..Self::EMPTY
        }
    }
    fn is_pending_for(self, tid: AdoptTid) -> bool {
        self.tid == tid.0
            && match self.phase {
                Phase::Requested | Phase::Filling | Phase::Offered => true,
                Phase::Empty | Phase::Claimed | Phase::Cancelled => false,
            }
    }
}

struct Cell<T> {
    word: AtomicU64,
    /// `Filling` / `Claimed` を作ったスレッドだけが触る。
    value: UnsafeCell<Option<T>>,
}
impl<T> Cell<T> {
    fn load(&self) -> CellWord {
        CellWord::unpack(self.word.load(Ordering::Acquire))
    }
    fn cas(&self, from: CellWord, to: CellWord) -> Result<(), CellWord> {
        self.word
            .compare_exchange(from.pack(), to.pack(), Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(CellWord::unpack)
    }
    /// `Claimed` を持つスレッドが値を取り出して口を空ける。
    fn finish_claim(&self, claimed: CellWord) -> Option<T> {
        // SAFETY: Claimed を CAS で作ったのはこのスレッドだけで、値を置いた
        // 側の書込みは Offered への AcqRel で見えている。
        let value = self.value.with_mut(|p| unsafe { (*p).take() });
        self.word.store(claimed.emptied().pack(), Ordering::Release);
        value
    }
}

/// 受け取り手を待つ要求 (`Adoptions::waiting` が返す)。作った時点の
/// Ticket を持つので、取り消された要求へは値を置けない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    index: usize,
    word: CellWord,
}

/// `N` は口の数 (loom 検査では小さくする)。
pub struct Adoptions<T, const N: usize = ADOPTION_CELLS> {
    cells: [Cell<T>; N],
}

// SAFETY: 値は口の状態機械で排他され、スレッド間を移るだけ。
unsafe impl<T: Send, const N: usize> Sync for Adoptions<T, N> {}
// SAFETY: 同上。
unsafe impl<T: Send, const N: usize> Send for Adoptions<T, N> {}

impl<T, const N: usize> Default for Adoptions<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Adoptions<T, N> {
    pub fn new() -> Self {
        Self {
            cells: std::array::from_fn(|_| Cell {
                word: AtomicU64::new(CellWord::EMPTY.pack()),
                value: UnsafeCell::new(None),
            }),
        }
    }
    /// 要求を置く (既に待っていれば何もしない)。置けなければ false。
    pub fn request(&self, tid: AdoptTid) -> bool {
        if self.cells.iter().any(|c| c.load().is_pending_for(tid)) {
            return true;
        }
        self.cells.iter().any(|c| {
            let w = c.load();
            w.phase == Phase::Empty
                && c.cas(
                    w,
                    CellWord {
                        phase: Phase::Requested,
                        ticket: w.ticket,
                        tid: tid.0,
                    },
                )
                .is_ok()
        })
    }
    /// 自分宛てに置かれた値を受け取り、口を空ける。
    pub fn take(&self, tid: AdoptTid) -> Option<T> {
        self.cells.iter().find_map(|c| {
            let w = c.load();
            if w.tid != tid.0 || w.phase != Phase::Offered {
                return None;
            }
            let claimed = w.with(Phase::Claimed);
            c.cas(w, claimed).ok()?;
            c.finish_claim(claimed)
        })
    }
    /// 値を待つ要求 (管理パス用)。
    pub fn waiting(&self) -> impl Iterator<Item = Request> + '_ {
        self.cells.iter().enumerate().filter_map(|(index, c)| {
            let word = c.load();
            (word.phase == Phase::Requested).then_some(Request { index, word })
        })
    }
    /// 要求を引き受ける。同じ要求を別の作り手が先に引き受けたか、要求が
    /// 取り消されていれば None。
    pub fn begin_fill(&self, request: Request) -> Option<Fill<'_, T>> {
        let cell = &self.cells[request.index];
        let filling = request.word.with(Phase::Filling);
        cell.cas(request.word, filling).ok()?;
        Some(Fill {
            cell,
            word: filling,
        })
    }
    /// 通常の登録で Ring を得たスレッド (同じ ID の再利用を含む) や終了した
    /// スレッド宛ての要求を取り消し、置かれたままの値を捨てる。作っている
    /// 最中の値は作り手が捨てる。
    pub fn clear(&self, tid: AdoptTid) {
        for c in &self.cells {
            let mut w = c.load();
            loop {
                if w.tid != tid.0 {
                    break;
                }
                let to = match w.phase {
                    Phase::Requested => w.emptied(),
                    Phase::Filling => w.with(Phase::Cancelled),
                    Phase::Offered => w.with(Phase::Claimed),
                    Phase::Empty | Phase::Claimed | Phase::Cancelled => break,
                };
                match c.cas(w, to) {
                    Ok(()) => {
                        if to.phase == Phase::Claimed {
                            drop(c.finish_claim(to));
                        }
                        break;
                    }
                    Err(now) => w = now,
                }
            }
        }
    }
}

impl<T, const N: usize> Drop for Adoptions<T, N> {
    fn drop(&mut self) {
        for c in &self.cells {
            // SAFETY: &mut self により他の参照は無い。
            drop(c.value.with_mut(|p| unsafe { (*p).take() }));
        }
    }
}

/// `Filling` を持っている間の権利。`offer` で値を置く。置かずに捨てると
/// 要求を `Requested` へ戻す (取り消されていれば口を空ける)。
pub struct Fill<'a, T> {
    cell: &'a Cell<T>,
    word: CellWord,
}
impl<T> Fill<'_, T> {
    pub fn tid(&self) -> AdoptTid {
        AdoptTid(self.word.tid)
    }
    /// 値を置く。作っている間に要求が取り消されていれば値を返す
    /// (呼び出し側が片付ける)。
    pub fn offer(self, value: T) -> Result<(), T> {
        let cell = self.cell;
        let word = self.word;
        std::mem::forget(self);
        // SAFETY: Filling はこのスレッドだけが持つ。
        cell.value.with_mut(|p| unsafe { *p = Some(value) });
        match cell.cas(word, word.with(Phase::Offered)) {
            Ok(()) => Ok(()),
            Err(now) => {
                debug_assert_eq!(now, word.with(Phase::Cancelled));
                // SAFETY: Cancelled へ移せるのは Filling の間だけで、値の置き場は
                // まだこのスレッドだけが持つ。
                let back = cell.value.with_mut(|p| unsafe { (*p).take() });
                cell.word.store(word.emptied().pack(), Ordering::Release);
                Err(back.expect("the value this thread just stored"))
            }
        }
    }
}
impl<T> Drop for Fill<'_, T> {
    fn drop(&mut self) {
        if self
            .cell
            .cas(self.word, self.word.with(Phase::Requested))
            .is_err()
        {
            // 取り消された (Cancelled)。口を空ける。
            self.cell
                .word
                .store(self.word.emptied().pack(), Ordering::Release);
        }
    }
}

#[cfg(all(test, not(shirusu_loom)))]
mod tests {
    use super::*;

    fn tid(n: u64) -> AdoptTid {
        AdoptTid::new(n).unwrap()
    }

    #[test]
    fn words_round_trip() {
        for phase in [
            Phase::Empty,
            Phase::Requested,
            Phase::Filling,
            Phase::Offered,
            Phase::Claimed,
            Phase::Cancelled,
        ] {
            for ticket in [0, 1, Ticket::MASK] {
                for t in [0, 1, u32::MAX] {
                    let w = CellWord {
                        phase,
                        ticket: Ticket(ticket),
                        tid: t,
                    };
                    assert_eq!(CellWord::unpack(w.pack()), w);
                }
            }
        }
        assert_eq!(Ticket(Ticket::MASK).next(), Ticket(0));
    }

    #[test]
    fn wide_thread_ids_cannot_adopt() {
        assert_eq!(AdoptTid::new(u64::from(u32::MAX) + 1), None);
        assert_eq!(tid(7).get(), 7);
    }

    #[test]
    fn request_fill_offer_take() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5)));
        assert!(a.request(tid(5)), "a second request is the same one");
        assert_eq!(a.waiting().count(), 1);
        assert_eq!(a.take(tid(5)), None, "nothing offered yet");
        let req = a.waiting().next().unwrap();
        let fill = a.begin_fill(req).unwrap();
        assert_eq!(fill.tid(), tid(5));
        assert!(a.begin_fill(req).is_none(), "one filler per request");
        assert_eq!(a.waiting().count(), 0);
        assert_eq!(a.take(tid(5)), None, "still filling");
        fill.offer(42).unwrap();
        assert_eq!(a.take(tid(6)), None, "only the requester takes it");
        assert_eq!(a.take(tid(5)), Some(42));
        assert_eq!(a.take(tid(5)), None);
        assert_eq!(a.waiting().count(), 0);
    }

    #[test]
    fn a_cancelled_request_cannot_be_filled_with_its_old_ticket() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5)));
        let stale = a.waiting().next().unwrap();
        a.clear(tid(5));
        assert!(a.request(tid(5)));
        let fresh = a.waiting().next().unwrap();
        assert_ne!(stale, fresh);
        assert!(a.begin_fill(stale).is_none(), "the old request id is dead");
        a.begin_fill(fresh).unwrap().offer(1).unwrap();
        assert_eq!(a.take(tid(5)), Some(1));
    }

    #[test]
    fn clear_while_filling_hands_the_value_back() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5)));
        let fill = a.begin_fill(a.waiting().next().unwrap()).unwrap();
        a.clear(tid(5));
        assert_eq!(fill.offer(9), Err(9));
        assert_eq!(a.take(tid(5)), None);
        assert_eq!(a.waiting().count(), 0);
        assert!(a.request(tid(5)), "the cell is free again");
    }

    #[test]
    fn dropped_fill_returns_the_request() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5)));
        drop(a.begin_fill(a.waiting().next().unwrap()).unwrap());
        assert_eq!(a.waiting().count(), 1);
        // 取り消し後に捨てた場合は口が空く。
        let fill = a.begin_fill(a.waiting().next().unwrap()).unwrap();
        a.clear(tid(5));
        drop(fill);
        assert_eq!(a.waiting().count(), 0);
    }

    #[test]
    fn clear_drops_an_offered_value_and_full_cells_refuse() {
        use std::sync::Arc;
        let a = Adoptions::<Arc<()>>::new();
        let v = Arc::new(());
        assert!(a.request(tid(1)));
        a.begin_fill(a.waiting().next().unwrap())
            .unwrap()
            .offer(Arc::clone(&v))
            .unwrap();
        assert_eq!(Arc::strong_count(&v), 2);
        a.clear(tid(1));
        assert_eq!(Arc::strong_count(&v), 1, "the offered value is released");
        for n in 0..ADOPTION_CELLS as u64 {
            assert!(a.request(tid(100 + n)));
        }
        assert!(!a.request(tid(9999)), "every cell is taken");
        // 置かれたままの値は口と一緒に捨てる。
        let req = a.waiting().next().unwrap();
        a.begin_fill(req).unwrap().offer(Arc::clone(&v)).unwrap();
        drop(a);
        assert_eq!(Arc::strong_count(&v), 1);
    }
}
