//! `flock`（`ADR-0087` の決定 4。2026-10-16）——ファイルの助言ロック。
//!
//! # 形
//!
//! **鍵は ext2 の inode 番号、持ち主は開いたファイル（`crate::vfs::File::Regular` の 1 つ）である。** 取れたロックは
//! この表の 1 項目で、`File::Regular` の `lock` の欄がその番号（1 から。0 は持っていない）を持つ。Linux の `flock` が
//! open file description に付くのと同じ形で、**`F_DUPFD` の写しは別の持ち主になる**（ZeikOS の `File` は写しごとに別で、
//! 写しは `lock` を 0 にして渡す。Linux は共有する。Seinas はロックファイルの fd を dup しない）。
//!
//! - `LOCK_EX`: ほかの持ち主が同じ inode を取っていれば衝突。`LOCK_SH`: ほかの持ち主の `LOCK_EX` と衝突。
//! - 衝突したとき、`LOCK_NB` なら `-EWOULDBLOCK`（`-EAGAIN`）、無ければ `Wait::FileLock { inode }` で眠り、放されたら
//!   取り直す（`crate::syscall::sys_flock`）。
//! - 放すのは `LOCK_UN` と `close`（プロセスの終わりは fd を全部閉じるので、そこで放される）。放したとき、同じ inode で
//!   眠る者を全部起こす（空振りを許す。起きた側が取り直す）。
//! - 同じ持ち主の取り直し（`LOCK_SH` ↔ `LOCK_EX`）は、ほかと衝突しなければその場で換える。
//!
//! # 表
//!
//! 8 本（Seinas の 3 本と、検査の分）。尽きれば `-ENOLCK`。**BKL の内側で触る**（システムコールの入口）。

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

/// 同時に持てるロックの数。
pub const MAX_FLOCKS: usize = 8;

/// `flock` の `operation`（`sys/file.h`）。
pub const LOCK_SH: u64 = 1;
pub const LOCK_EX: u64 = 2;
pub const LOCK_NB: u64 = 4;
pub const LOCK_UN: u64 = 8;

/// 表の 1 項目。`token` は `File::Regular` の `lock` の欄の値（添字 + 1）である。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub in_use: bool,
    pub inode: u32,
    pub exclusive: bool,
}

impl Entry {
    pub const EMPTY: Self = Self {
        in_use: false,
        inode: 0,
        exclusive: false,
    };
}

static LOCKS: Locked<[Entry; MAX_FLOCKS]> = Locked::new([Entry::EMPTY; MAX_FLOCKS]);

/// 取った回数・衝突で断った（または眠った）回数・放した回数（計測。起動ログの行に出す）。
static ACQUIRED: AtomicU64 = AtomicU64::new(0);
static CONFLICTS: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);

/// `acquire` の結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// 取れた（または換えた）。値は `File::Regular` の `lock` に入れる番号（添字 + 1）。
    Acquired(u8),
    /// ほかの持ち主と衝突した。
    Conflict,
    /// 表が尽きた（`-ENOLCK`）。
    NoRoom,
}

/// 衝突の判定（純粋な論理。ホストの単体試験が固定する）。**`holding` は自分の項目の番号（0 なら無し）で、自分とは
/// 衝突しない。**
pub fn conflicts(entries: &[Entry], inode: u32, exclusive: bool, holding: u8) -> bool {
    entries.iter().enumerate().any(|(index, entry)| {
        entry.in_use
            && entry.inode == inode
            && index + 1 != holding as usize
            && (entry.exclusive || exclusive)
    })
}

/// 取る（または同じ持ち主の取り直し）。`holding` は今持っている番号（0 なら無し）。
pub fn acquire(inode: u32, exclusive: bool, holding: u8) -> Outcome {
    let mut locks = LOCKS.lock();
    // 破壊テスト (2026-10-16, flock-ignores-holder-test): ほかの持ち主を見ない。2 本目の `LOCK_EX|LOCK_NB` が 0 で通り、
    // 確かめのイメージが名指しして 97 で終わる。
    let blocked = !cfg!(feature = "flock-ignores-holder-test")
        && conflicts(&*locks, inode, exclusive, holding);
    if blocked {
        CONFLICTS.fetch_add(1, Ordering::Relaxed);
        return Outcome::Conflict;
    }
    if holding != 0 {
        let entry = &mut locks[holding as usize - 1];
        debug_assert!(entry.in_use && entry.inode == inode);
        entry.exclusive = exclusive;
        return Outcome::Acquired(holding);
    }
    let Some(index) = locks.iter().position(|entry| !entry.in_use) else {
        return Outcome::NoRoom;
    };
    locks[index] = Entry {
        in_use: true,
        inode,
        exclusive,
    };
    ACQUIRED.fetch_add(1, Ordering::Relaxed);
    Outcome::Acquired(index as u8 + 1)
}

/// 放す（`LOCK_UN`）。`token` が 0 なら何もしない。同じ inode で眠る者を起こす。
pub fn release(token: u8) {
    if token == 0 {
        return;
    }
    let inode = {
        let mut locks = LOCKS.lock();
        let entry = &mut locks[token as usize - 1];
        if !entry.in_use {
            return;
        }
        let inode = entry.inode;
        *entry = Entry::EMPTY;
        inode
    };
    RELEASED.fetch_add(1, Ordering::Relaxed);
    crate::task::wake_tasks_waiting_on(crate::task::Wait::FileLock { inode });
}

/// `close` で放す。**プロセスの終わりもここを通る**（fd の表の `Drop`）。**破壊テストの入口はここと [`acquire`] に
/// 置く**（確かめのイメージと一緒に足す）。
pub fn release_on_close(token: u8) {
    // 破壊テスト (2026-10-16, flock-keeps-lock-after-close-test): `close` で放さない。閉じた後の `LOCK_EX|LOCK_NB` が
    // `-EWOULDBLOCK` のままになり、確かめのイメージが名指しして 98 で終わる。
    if cfg!(feature = "flock-keeps-lock-after-close-test") {
        return;
    }
    release(token);
}

/// 今持たれている本数。
pub fn held_count() -> usize {
    LOCKS.lock().iter().filter(|entry| entry.in_use).count()
}

/// 取った回数・衝突した回数・放した回数の累計。
pub fn counts() -> (u64, u64, u64) {
    (
        ACQUIRED.load(Ordering::Relaxed),
        CONFLICTS.load(Ordering::Relaxed),
        RELEASED.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(inode: u32, exclusive: bool) -> Entry {
        Entry {
            in_use: true,
            inode,
            exclusive,
        }
    }

    /// 衝突の判定——別の持ち主の EX とは常に、SH とは EX を求めるときだけ衝突し、自分の項目と別の inode とは衝突しない。
    #[test]
    fn conflicts_follow_the_flock_rules() {
        let table = [held(7, true), held(9, false), Entry::EMPTY];
        assert!(conflicts(&table, 7, true, 0));
        assert!(conflicts(&table, 7, false, 0));
        assert!(conflicts(&table, 9, true, 0));
        assert!(!conflicts(&table, 9, false, 0));
        assert!(!conflicts(&table, 8, true, 0));
        assert!(!conflicts(&table, 7, true, 1));
        assert!(conflicts(&table, 7, true, 2));
    }
}
