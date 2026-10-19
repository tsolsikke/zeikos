//! `timerfd`（2026-10-19）——締切で満了の回数を数えるファイル。Seinas のイベントループ（polling）が、待ちの期限に使う
//! （`timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC|TFD_NONBLOCK)`、相対の 1 回きりの `timerfd_settime`、0 での停止）。
//!
//! # 形
//!
//! - 時計は `CLOCK_MONOTONIC` だけである（`crate::syscall` が断る）。**締切はタイマのティックで持つ**（10 ms。`nanosleep` と
//!   同じく切り上げる）。16 ms を頼まれた締切は 20 ms になる。
//! - 満了は、読むときと問い合わせるときに、今のティックで決める（遅延評価）。**ティックの割り込みの道には何も足さない**
//!   ——待つ側は、締切を待ちの集合の `Wait::Timer` に入れ、既存の `crate::task::wake_expired_timers` が起こす。
//! - `read` は満了の回数（`u64`）を返して 0 に戻す。間隔があれば締切を進め、無ければ止める。
//! - 規則は [`TimerState`]（純粋な論理。ホストで試す）にあり、この表はそれを番号で引くだけである。
//!
//! # 表と参照の数
//!
//! 8 個。参照を数える（`crate::eventfd` と同じ。`F_DUPFD` のコピーは同じ項目を指す）。**BKL の内側で触る。**

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

/// 同時に作れる timerfd の数。
pub const MAX_TIMERFDS: usize = 8;

/// 締切と間隔（どちらもティック）。**締切が `None` なら止まっている。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimerState {
    deadline: Option<u64>,
    interval: u64,
}

impl TimerState {
    /// 止まった状態。
    pub const STOPPED: Self = Self {
        deadline: None,
        interval: 0,
    };

    /// 締切と間隔を据える（`timerfd_settime`）。**締切が `None` なら止める**（間隔は覚えておく。`timerfd_gettime` が返す）。
    pub fn set(&mut self, deadline: Option<u64>, interval: u64) {
        self.deadline = deadline;
        self.interval = interval;
    }

    /// 次の締切。止まっていれば `None`。
    pub fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    /// 間隔（ティック。0 なら 1 回きり）。
    pub fn interval(&self) -> u64 {
        self.interval
    }

    /// `now` までの満了の回数（取らずに数える）。
    pub fn expirations(&self, now: u64) -> u64 {
        match self.deadline {
            // **間隔が 0 なら 1 回きり**（`checked_div` が `None` を返す）。
            Some(deadline) if now >= deadline => (now - deadline)
                .checked_div(self.interval)
                .map_or(1, |periods| 1 + periods),
            _ => 0,
        }
    }

    /// `now` までの満了の回数を取る（`read`）。**間隔があれば締切を回数ぶん進め、無ければ止める。**
    pub fn take(&mut self, now: u64) -> u64 {
        let count = self.expirations(now);
        if count == 0 {
            return 0;
        }
        if self.interval == 0 {
            self.deadline = None;
        } else if let Some(deadline) = self.deadline {
            self.deadline = Some(deadline.saturating_add(count.saturating_mul(self.interval)));
        }
        count
    }

    /// 次の満了までの残り（ティック）と間隔（`timerfd_gettime` と、`timerfd_settime` の前の値）。止まっていれば残りは 0。
    /// **締切を過ぎてまだ読まれていなければ、残りは 0 である**（Linux は次の満了までを返すが、Seinas の道は読まない）。
    pub fn remaining(&self, now: u64) -> (u64, u64) {
        let remaining = self
            .deadline
            .map_or(0, |deadline| deadline.saturating_sub(now));
        (remaining, self.interval)
    }
}

/// 表の 1 項目。`refs` が 0 なら空いている。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Slot {
    refs: u32,
    state: TimerState,
}

impl Slot {
    const EMPTY: Self = Self {
        refs: 0,
        state: TimerState::STOPPED,
    };
}

static TIMERFDS: Locked<[Slot; MAX_TIMERFDS]> = Locked::new([Slot::EMPTY; MAX_TIMERFDS]);

/// 作った数・空けた数・読んで渡した満了の回数・待った読みの数（計測。判定行に出す）。
static CREATED: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);
static EXPIRATIONS_READ: AtomicU64 = AtomicU64::new(0);
static READER_WAITS: AtomicU64 = AtomicU64::new(0);

/// 作った数。
pub fn created() -> u64 {
    CREATED.load(Ordering::Relaxed)
}

/// 空けた数。
pub fn released() -> u64 {
    RELEASED.load(Ordering::Relaxed)
}

/// 読んで渡した満了の回数の合計。
pub fn expirations_read() -> u64 {
    EXPIRATIONS_READ.load(Ordering::Relaxed)
}

/// 満了がまだで待った読みの数。
pub fn reader_waits() -> u64 {
    READER_WAITS.load(Ordering::Relaxed)
}

/// 読みが待つことを数える。**待つのは呼び出し側である**（`crate::syscall`）。
pub fn note_reader_wait() {
    READER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 作る（止まった状態で）。**空きが無ければ `None`。** 参照は 1 から始まる。
pub fn create() -> Option<u8> {
    let mut slots = TIMERFDS.lock();
    let index = slots.iter().position(|slot| slot.refs == 0)?;
    slots[index] = Slot {
        refs: 1,
        state: TimerState::STOPPED,
    };
    CREATED.fetch_add(1, Ordering::Relaxed);
    Some(index as u8)
}

/// 参照を 1 つ足す（`F_DUPFD` のコピー）。
pub fn attach(id: u8) {
    let mut slots = TIMERFDS.lock();
    if let Some(slot) = slots.get_mut(id as usize).filter(|slot| slot.refs != 0) {
        slot.refs += 1;
    }
}

/// 参照を 1 つ減らす（`close` と、表の `Drop`）。**0 になったら空け、待っている者を起こす**（起きた側は次の周で
/// `-EBADF` を見る）。
pub fn detach(id: u8) {
    let freed = {
        let mut slots = TIMERFDS.lock();
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
        crate::task::wake_tasks_waiting_on(crate::task::Wait::TimerFdReadable { id });
    }
}

/// 今の状態のコピー。**無い番号は `None`。**
pub fn state(id: u8) -> Option<TimerState> {
    let slots = TIMERFDS.lock();
    slots
        .get(id as usize)
        .filter(|slot| slot.refs != 0)
        .map(|slot| slot.state)
}

/// 締切と間隔を据え、**前の状態を返す**（`timerfd_settime` の `old_value`）。無い番号は `None`。据えたら、読みで待って
/// いる者を起こす（新しい締切で待ち直させる）。
pub fn set(id: u8, deadline: Option<u64>, interval: u64) -> Option<TimerState> {
    let old = {
        let mut slots = TIMERFDS.lock();
        let slot = slots.get_mut(id as usize).filter(|slot| slot.refs != 0)?;
        let old = slot.state;
        slot.state.set(deadline, interval);
        old
    };
    crate::task::wake_tasks_waiting_on(crate::task::Wait::TimerFdReadable { id });
    Some(old)
}

/// `now` までの満了の回数を取る（`read`）。無い番号は `None`。
pub fn take(id: u8, now: u64) -> Option<u64> {
    let count = {
        let mut slots = TIMERFDS.lock();
        let slot = slots.get_mut(id as usize).filter(|slot| slot.refs != 0)?;
        slot.state.take(now)
    };
    EXPIRATIONS_READ.fetch_add(count, Ordering::Relaxed);
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_timer_never_expires() {
        let state = TimerState::STOPPED;
        assert_eq!(state.expirations(1_000_000), 0);
        assert_eq!(state.remaining(5), (0, 0));
    }

    #[test]
    fn a_one_shot_timer_expires_once_and_stops_when_read() {
        let mut state = TimerState::STOPPED;
        state.set(Some(12), 0);
        assert_eq!(state.expirations(11), 0);
        assert_eq!(state.remaining(10), (2, 0));
        assert_eq!(state.expirations(12), 1);
        assert_eq!(
            state.expirations(500),
            1,
            "a one-shot timer counts one expiry however late"
        );
        assert_eq!(state.take(500), 1);
        assert_eq!(state.deadline(), None);
        assert_eq!(state.take(600), 0);
    }

    #[test]
    fn an_interval_timer_counts_every_period_and_moves_its_deadline() {
        let mut state = TimerState::STOPPED;
        state.set(Some(10), 5);
        assert_eq!(state.expirations(9), 0);
        assert_eq!(state.expirations(10), 1);
        assert_eq!(state.expirations(24), 3);
        assert_eq!(state.take(24), 3);
        assert_eq!(state.deadline(), Some(25));
        assert_eq!(state.expirations(24), 0);
        assert_eq!(state.remaining(24), (1, 5));
    }

    #[test]
    fn setting_no_deadline_stops_but_keeps_the_interval() {
        let mut state = TimerState::STOPPED;
        state.set(Some(10), 5);
        state.set(None, 5);
        assert_eq!(state.expirations(100), 0);
        assert_eq!(state.remaining(100), (0, 5));
    }

    #[test]
    fn a_deadline_already_in_the_past_expires_at_once() {
        let mut state = TimerState::STOPPED;
        state.set(Some(3), 0);
        assert_eq!(state.expirations(7), 1);
        assert_eq!(state.remaining(7), (0, 0));
    }
}
