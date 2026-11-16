//! Slot を持てなかったスレッドへ、別のスレッドが作った値 (Ring の状態) を
//! 渡す受け渡し口。記録中のスレッドが使う `request` / `take` はロックも
//! Heap も使わない。
//!
//! 口ごとの状態機械 (1 語の CAS で遷移し、`Ticket` が要求ごとの識別子):
//!
//! ```text
//! Empty(t) --request--> Requested(t, tid) --begin_fill--> Filling(t, tid)
//!    ^                        |    ^                         |       |
//!    |          clear / sweep |    +------- Fill の Drop ----+       | offer
//!    +--- t+1 --- (Claimed) --+                                      v
//!    +--------- t+1 --- take / clear / sweep --- Claimed(t, tid) <--- Offered(t, tid)
//!    +--------- t+1 --- offer / Fill の Drop --- Cancelled(t, tid) <-- clear (Filling 中)
//! ```
//!
//! 要求には要求者の [`RequesterKey`] (スレッド ID と生成時刻) を添える。管理パスは
//! 終了した要求者の要求を [`Adoptions::sweep`] で取り消し、置いたまま受け取られ
//! なかった値を返す (外部レビュー 2026-11-16 P1: Slot を持てずに要求したまま
//! 終了したスレッドへ Ring を割り当て続けていた)。鍵の語は口を Empty へ戻す
//! スレッドが先に 0 へ戻し、要求者は Requested を作った直後に書く。0 の間
//! (書く前) の要求は管理パスが引き受けない。
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

/// 要求者の識別 (`shirusu_transport_win::ThreadKey` のビット)。0 ではない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RequesterKey(std::num::NonZeroU64);
impl RequesterKey {
    pub fn from_bits(bits: u64) -> Option<Self> {
        std::num::NonZeroU64::new(bits).map(Self)
    }
    pub fn bits(self) -> u64 {
        self.0.get()
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
    /// 要求者の鍵 (0: まだ書かれていない)。Requested を作った要求者だけが
    /// 書き、口を Empty へ戻すスレッドが 0 へ戻す。
    key: AtomicU64,
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
        self.release(claimed);
        value
    }
    /// 口を独占しているスレッド (Claimed / Cancelled を持つ) が口を空ける。
    /// 鍵を先に消すので、次の要求者が見る鍵は 0 か自分のもの。
    fn release(&self, held: CellWord) {
        self.key.store(0, Ordering::Relaxed);
        self.word.store(held.emptied().pack(), Ordering::Release);
    }
    fn requester(&self) -> Option<RequesterKey> {
        RequesterKey::from_bits(self.key.load(Ordering::Acquire))
    }
}

/// 受け取り手を待つ要求 (`Adoptions::waiting` が返す)。作った時点の
/// Ticket を持つので、取り消された要求へは値を置けない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    index: usize,
    word: CellWord,
    key: RequesterKey,
}
impl Request {
    /// 要求したスレッド。
    pub fn key(&self) -> RequesterKey {
        self.key
    }
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
                key: AtomicU64::new(0),
                value: UnsafeCell::new(None),
            }),
        }
    }
    /// 要求を置く (同じ要求者の要求が既に待っていれば何もしない)。置けな
    /// ければ false。ロックも確保もしない。
    pub fn request(&self, tid: AdoptTid, key: RequesterKey) -> bool {
        if self
            .cells
            .iter()
            .any(|c| c.load().is_pending_for(tid) && c.requester() == Some(key))
        {
            return true;
        }
        self.cells.iter().any(|c| {
            let w = c.load();
            let placed = w.phase == Phase::Empty
                && c.cas(
                    w,
                    CellWord {
                        phase: Phase::Requested,
                        ticket: w.ticket,
                        tid: tid.0,
                    },
                )
                .is_ok();
            if placed {
                // 鍵を書くまで管理パスは引き受けない。この 2 命令の間で強制
                // 終了されると (終了通知も届かない)、この口は鍵なしの要求の
                // まま残る。失うのは口 1 つで、他の要求も記録も止まらない。
                c.key.store(key.bits(), Ordering::Release);
            }
            placed
        })
    }
    /// 自分宛てに置かれた値を受け取り、口を空ける。
    pub fn take(&self, tid: AdoptTid, key: RequesterKey) -> Option<T> {
        self.cells.iter().find_map(|c| {
            let w = c.load();
            if w.tid != tid.0 || w.phase != Phase::Offered || c.requester() != Some(key) {
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
            if word.phase != Phase::Requested {
                return None;
            }
            // 鍵が別の要求のものでも、begin_fill の CAS (Ticket 込み) が外れる。
            let key = c.requester()?;
            Some(Request { index, word, key })
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
                    Phase::Requested => w.with(Phase::Claimed),
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
    /// 終了した要求者の要求を取り消し、受け取られずに置かれた値を `retire`
    /// へ渡す (管理パス)。`is_dead` は終了が確かな鍵だけに true を返すこと
    /// (分からなければ生きている扱い)。作っている最中の口は作り手が扱う。
    pub fn sweep(
        &self,
        mut is_dead: impl FnMut(RequesterKey) -> bool,
        mut retire: impl FnMut(AdoptTid, T),
    ) {
        for c in &self.cells {
            let w = c.load();
            match w.phase {
                Phase::Requested | Phase::Offered => {}
                Phase::Empty | Phase::Filling | Phase::Claimed | Phase::Cancelled => continue,
            }
            let Some(key) = c.requester() else {
                continue;
            };
            if !is_dead(key) {
                continue;
            }
            // 鍵を読んだ後に口が別の要求へ移っていれば、Ticket 込みの CAS が外れる。
            let claimed = w.with(Phase::Claimed);
            if c.cas(w, claimed).is_err() {
                continue;
            }
            if let Some(value) = c.finish_claim(claimed) {
                retire(AdoptTid(w.tid), value);
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
                cell.release(word);
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
            self.cell.release(self.word);
        }
    }
}

#[cfg(all(test, not(shirusu_loom)))]
mod tests {
    use super::*;

    fn tid(n: u64) -> AdoptTid {
        AdoptTid::new(n).unwrap()
    }
    /// 試験用の要求者鍵 (ID ごとに 1 つ)。
    fn key(n: u64) -> RequesterKey {
        RequesterKey::from_bits(n | (1 << 40)).unwrap()
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
        assert!(a.request(tid(5), key(5)));
        assert!(
            a.request(tid(5), key(5)),
            "a second request is the same one"
        );
        assert_eq!(a.waiting().count(), 1);
        assert_eq!(a.take(tid(5), key(5)), None, "nothing offered yet");
        let req = a.waiting().next().unwrap();
        let fill = a.begin_fill(req).unwrap();
        assert_eq!(fill.tid(), tid(5));
        assert!(a.begin_fill(req).is_none(), "one filler per request");
        assert_eq!(a.waiting().count(), 0);
        assert_eq!(a.take(tid(5), key(5)), None, "still filling");
        fill.offer(42).unwrap();
        assert_eq!(a.take(tid(6), key(6)), None, "only the requester takes it");
        assert_eq!(a.take(tid(5), key(5)), Some(42));
        assert_eq!(a.take(tid(5), key(5)), None);
        assert_eq!(a.waiting().count(), 0);
    }

    #[test]
    fn a_cancelled_request_cannot_be_filled_with_its_old_ticket() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5), key(5)));
        let stale = a.waiting().next().unwrap();
        a.clear(tid(5));
        assert!(a.request(tid(5), key(5)));
        let fresh = a.waiting().next().unwrap();
        assert_ne!(stale, fresh);
        assert!(a.begin_fill(stale).is_none(), "the old request id is dead");
        a.begin_fill(fresh).unwrap().offer(1).unwrap();
        assert_eq!(a.take(tid(5), key(5)), Some(1));
    }

    #[test]
    fn clear_while_filling_hands_the_value_back() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5), key(5)));
        let fill = a.begin_fill(a.waiting().next().unwrap()).unwrap();
        a.clear(tid(5));
        assert_eq!(fill.offer(9), Err(9));
        assert_eq!(a.take(tid(5), key(5)), None);
        assert_eq!(a.waiting().count(), 0);
        assert!(a.request(tid(5), key(5)), "the cell is free again");
    }

    #[test]
    fn dropped_fill_returns_the_request() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(5), key(5)));
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
        assert!(a.request(tid(1), key(1)));
        a.begin_fill(a.waiting().next().unwrap())
            .unwrap()
            .offer(Arc::clone(&v))
            .unwrap();
        assert_eq!(Arc::strong_count(&v), 2);
        a.clear(tid(1));
        assert_eq!(Arc::strong_count(&v), 1, "the offered value is released");
        for n in 0..ADOPTION_CELLS as u64 {
            assert!(a.request(tid(100 + n), key(100 + n)));
        }
        assert!(!a.request(tid(9999), key(9999)), "every cell is taken");
        // 置かれたままの値は口と一緒に捨てる。
        let req = a.waiting().next().unwrap();
        a.begin_fill(req).unwrap().offer(Arc::clone(&v)).unwrap();
        drop(a);
        assert_eq!(Arc::strong_count(&v), 1);
    }

    /// レビュー 2026-11-16 P1: 終了した要求者の要求は取り消され、置いたまま
    /// の値は回収へ渡る。生きている要求者のものには触れない。
    #[test]
    fn sweep_cancels_dead_requests_and_returns_unclaimed_values() {
        let a = Adoptions::<u32>::new();
        assert!(a.request(tid(1), key(1))); // dead, still requested
        assert!(a.request(tid(2), key(2))); // dead, offered
        assert!(a.request(tid(3), key(3))); // alive, offered
        for req in a
            .waiting()
            .filter(|r| r.key() != key(1))
            .collect::<Vec<_>>()
        {
            let fill = a.begin_fill(req).unwrap();
            let v = fill.tid().get() as u32 * 10;
            fill.offer(v).unwrap();
        }
        let mut retired = Vec::new();
        a.sweep(
            |k| k == key(1) || k == key(2),
            |t, v| retired.push((t.get(), v)),
        );
        assert_eq!(retired, vec![(2, 20)]);
        assert_eq!(a.waiting().count(), 0, "the dead request is gone");
        assert_eq!(a.take(tid(3), key(3)), Some(30), "the live one is kept");
        assert_eq!(a.take(tid(2), key(2)), None);
    }

    /// A later thread with a reused ID neither takes the value made for the
    /// earlier one nor shares its request.
    #[test]
    fn a_reused_thread_id_is_another_requester() {
        let a = Adoptions::<u32>::new();
        let (old, new) = (key(5), RequesterKey::from_bits(5 | (2 << 40)).unwrap());
        assert!(a.request(tid(5), old));
        a.begin_fill(a.waiting().next().unwrap())
            .unwrap()
            .offer(1)
            .unwrap();
        assert!(a.request(tid(5), new));
        assert_eq!(a.waiting().count(), 1, "the new requester waits on its own");
        assert_eq!(a.take(tid(5), new), None, "not the old requester's value");
        let mut retired = Vec::new();
        a.sweep(|k| k == old, |_, v| retired.push(v));
        assert_eq!(retired, vec![1]);
        a.begin_fill(a.waiting().next().unwrap())
            .unwrap()
            .offer(2)
            .unwrap();
        assert_eq!(a.take(tid(5), new), Some(2));
    }

    /// A requester whose key is not written yet is never served (its cell
    /// reads as no request), and emptied cells start without a key.
    #[test]
    fn unkeyed_requests_are_not_served_and_keys_reset_with_the_cell() {
        let a = Adoptions::<u32, 1>::new();
        let c = &a.cells[0];
        c.word.store(
            CellWord {
                phase: Phase::Requested,
                ticket: Ticket(0),
                tid: 9,
            }
            .pack(),
            Ordering::Release,
        );
        assert_eq!(a.waiting().count(), 0);
        c.key.store(key(9).bits(), Ordering::Release);
        assert_eq!(a.waiting().count(), 1);
        a.clear(tid(9));
        assert_eq!(c.key.load(Ordering::Acquire), 0);
        assert!(a.request(tid(9), key(9)));
        assert_eq!(a.waiting().next().unwrap().key(), key(9));
    }
}
