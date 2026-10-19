//! `eventfd`（2026-10-19）——数を 1 つ持つだけのファイル。Seinas のイベントループ（calloop と polling）が、自分を起こす
//! 合図に使う（`eventfd2(0, EFD_CLOEXEC|EFD_NONBLOCK)`）。
//!
//! # 形
//!
//! - 値は `u64`。`write` は 8 バイトの値を足し、`read` は値を返して 0 に戻す。**`EFD_SEMAPHORE` なら、`read` は 1 を返して
//!   1 減らす。** 値の最大は `2^64 - 2`（[`EventCounter::MAX`]）で、越える `write` は待つか `-EAGAIN`。`2^64 - 1` を書く
//!   `write` は `-EINVAL`（Linux と同じ）。
//! - 待たない印（`EFD_NONBLOCK`）は fd の側（`crate::vfs::File::EventFd`）が持つ。
//! - 値の規則は [`EventCounter`]（純粋な論理。ホストで試す）にあり、この表はそれを番号で引くだけである。
//!
//! # 表と参照の数
//!
//! 8 個。尽きれば `-ENFILE` ではなく `-ENOMEM`（`memfd_create` と同じ答え）。**参照を数える**——`F_DUPFD` のコピーは
//! 同じ項目を指し、最後のコピーを閉じたときに項目を空ける（ソケットとパイプの端は数えていない。`ADR-0064` の
//! 2026-10-19 の Addendum の既知の不具合）。**BKL の内側で触る**（システムコールの入口）。

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

/// 同時に作れる eventfd の数。
pub const MAX_EVENTFDS: usize = 8;

/// `eventfd` の値と、その読み書きの規則（純粋な論理）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EventCounter {
    value: u64,
    semaphore: bool,
}

/// 読んだ結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadOutcome {
    /// 読めた値。
    Value(u64),
    /// 値が 0 で、読めない（待つか `-EAGAIN`）。
    Empty,
}

/// 書いた結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteOutcome {
    /// 足した。
    Added,
    /// 足すと最大を越える（待つか `-EAGAIN`）。
    Full,
    /// `2^64 - 1` を書こうとした（`-EINVAL`）。
    Invalid,
}

impl EventCounter {
    /// 値の最大（`2^64 - 2`）。
    pub const MAX: u64 = u64::MAX - 1;

    /// 初めの値と、`EFD_SEMAPHORE` かどうかで作る。
    pub const fn new(initial: u64, semaphore: bool) -> Self {
        Self {
            value: initial,
            semaphore,
        }
    }

    /// 今の値。
    pub fn value(&self) -> u64 {
        self.value
    }

    /// 読む。**0 なら [`ReadOutcome::Empty`]。** ふつうは値を返して 0 に戻し、`EFD_SEMAPHORE` なら 1 を返して 1 減らす。
    pub fn read(&mut self) -> ReadOutcome {
        if self.value == 0 {
            return ReadOutcome::Empty;
        }
        if self.semaphore {
            self.value -= 1;
            ReadOutcome::Value(1)
        } else {
            let value = self.value;
            self.value = 0;
            ReadOutcome::Value(value)
        }
    }

    /// `add` を足す。**`u64::MAX` は [`WriteOutcome::Invalid`]、足すと [`Self::MAX`] を越えるなら [`WriteOutcome::Full`]**
    /// （値は変えない）。
    pub fn write(&mut self, add: u64) -> WriteOutcome {
        if add == u64::MAX {
            return WriteOutcome::Invalid;
        }
        if add > Self::MAX - self.value {
            return WriteOutcome::Full;
        }
        self.value += add;
        WriteOutcome::Added
    }

    /// 読めるか（値が 0 でない）。
    pub fn readable(&self) -> bool {
        self.value != 0
    }

    /// 少なくとも 1 を書けるか（値が最大より小さい）。
    pub fn writable(&self) -> bool {
        self.value < Self::MAX
    }
}

/// 表の 1 項目。`refs` が 0 なら空いている。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Slot {
    refs: u32,
    counter: EventCounter,
}

impl Slot {
    const EMPTY: Self = Self {
        refs: 0,
        counter: EventCounter::new(0, false),
    };
}

static EVENTFDS: Locked<[Slot; MAX_EVENTFDS]> = Locked::new([Slot::EMPTY; MAX_EVENTFDS]);

/// 作った数・空けた数・待った読みと書きの数（計測。判定行に出す）。
static CREATED: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);
static READER_WAITS: AtomicU64 = AtomicU64::new(0);
static WRITER_WAITS: AtomicU64 = AtomicU64::new(0);

/// 作った数。
pub fn created() -> u64 {
    CREATED.load(Ordering::Relaxed)
}

/// 空けた数。
pub fn released() -> u64 {
    RELEASED.load(Ordering::Relaxed)
}

/// 値が 0 で待った読みの数。
pub fn reader_waits() -> u64 {
    READER_WAITS.load(Ordering::Relaxed)
}

/// 最大を越えて待った書きの数。
pub fn writer_waits() -> u64 {
    WRITER_WAITS.load(Ordering::Relaxed)
}

/// 読みが待つことを数える。**待つのは呼び出し側である**（`crate::syscall`）。
pub fn note_reader_wait() {
    READER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 書きが待つことを数える。
pub fn note_writer_wait() {
    WRITER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 作る。**空きが無ければ `None`。** 参照は 1 から始まる。
pub fn create(initial: u64, semaphore: bool) -> Option<u8> {
    let mut slots = EVENTFDS.lock();
    let index = slots.iter().position(|slot| slot.refs == 0)?;
    slots[index] = Slot {
        refs: 1,
        counter: EventCounter::new(initial, semaphore),
    };
    CREATED.fetch_add(1, Ordering::Relaxed);
    Some(index as u8)
}

/// 参照を 1 つ足す（`F_DUPFD` のコピー）。
pub fn attach(id: u8) {
    let mut slots = EVENTFDS.lock();
    if let Some(slot) = slots.get_mut(id as usize).filter(|slot| slot.refs != 0) {
        slot.refs += 1;
    }
}

/// 参照を 1 つ減らす（`close` と、表の `Drop`）。**0 になったら空ける。** 空けたとき、待っている者を起こす
/// （起きた側は fd がもう無いので、次の入口で `-EBADF` を見る。ふつうは自分の fd を閉じた者しか居ない）。
pub fn detach(id: u8) {
    let freed = {
        let mut slots = EVENTFDS.lock();
        let Some(slot) = slots.get_mut(id as usize).filter(|slot| slot.refs != 0) else {
            return;
        };
        slot.refs -= 1;
        if slot.refs == 0 {
            *slot = Slot::EMPTY;
            RELEASED.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    };
    if freed {
        crate::task::wake_tasks_waiting_on(crate::task::Wait::EventFdReadable { id });
        crate::task::wake_tasks_waiting_on(crate::task::Wait::EventFdWritable { id });
    }
}

/// 値を取らずに、読めるか（値が 0 でない）を見る。**無い番号は `None`。** `read` の前に書き先を確かめるために使う
/// （値を取ってから書き先が無いと分かっても、戻せないため）。
pub fn readable(id: u8) -> Option<bool> {
    let slots = EVENTFDS.lock();
    let slot = slots.get(id as usize).filter(|slot| slot.refs != 0)?;
    Some(slot.counter.readable())
}

/// 読む。**無い番号は `None`。** 読めたら、最大で待っていた書き手を起こす。
pub fn read(id: u8) -> Option<ReadOutcome> {
    let outcome = {
        let mut slots = EVENTFDS.lock();
        let slot = slots.get_mut(id as usize).filter(|slot| slot.refs != 0)?;
        slot.counter.read()
    };
    if matches!(outcome, ReadOutcome::Value(_)) {
        crate::task::wake_tasks_waiting_on(crate::task::Wait::EventFdWritable { id });
    }
    Some(outcome)
}

/// 書く。**無い番号は `None`。** 足せたら、値が 0 で待っていた読み手を起こす。
pub fn write(id: u8, add: u64) -> Option<WriteOutcome> {
    let outcome = {
        let mut slots = EVENTFDS.lock();
        let slot = slots.get_mut(id as usize).filter(|slot| slot.refs != 0)?;
        slot.counter.write(add)
    };
    if outcome == WriteOutcome::Added && add != 0 {
        crate::task::wake_tasks_waiting_on(crate::task::Wait::EventFdReadable { id });
    }
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_returns_the_whole_value_and_resets_it() {
        let mut counter = EventCounter::new(0, false);
        assert_eq!(counter.read(), ReadOutcome::Empty);
        assert_eq!(counter.write(3), WriteOutcome::Added);
        assert_eq!(counter.write(4), WriteOutcome::Added);
        assert!(counter.readable());
        assert_eq!(counter.read(), ReadOutcome::Value(7));
        assert_eq!(counter.read(), ReadOutcome::Empty);
        assert!(!counter.readable());
    }

    #[test]
    fn a_semaphore_read_returns_one_at_a_time() {
        let mut counter = EventCounter::new(2, true);
        assert_eq!(counter.read(), ReadOutcome::Value(1));
        assert_eq!(counter.read(), ReadOutcome::Value(1));
        assert_eq!(counter.read(), ReadOutcome::Empty);
    }

    #[test]
    fn a_write_past_the_maximum_is_full_and_leaves_the_value() {
        let mut counter = EventCounter::new(EventCounter::MAX - 1, false);
        assert!(counter.writable());
        assert_eq!(counter.write(2), WriteOutcome::Full);
        assert_eq!(counter.value(), EventCounter::MAX - 1);
        assert_eq!(counter.write(1), WriteOutcome::Added);
        assert_eq!(counter.value(), EventCounter::MAX);
        assert!(!counter.writable());
        assert_eq!(counter.write(1), WriteOutcome::Full);
    }

    #[test]
    fn writing_all_ones_is_invalid() {
        let mut counter = EventCounter::new(0, false);
        assert_eq!(counter.write(u64::MAX), WriteOutcome::Invalid);
        assert_eq!(counter.value(), 0);
    }

    #[test]
    fn writing_zero_adds_nothing_but_succeeds() {
        let mut counter = EventCounter::new(0, false);
        assert_eq!(counter.write(0), WriteOutcome::Added);
        assert!(!counter.readable());
    }
}
