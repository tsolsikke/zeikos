//! 共有メモリの実体（`ADR-0065`）。**アロケータから取ったフレームを fd で持ち、
//! 複数のプロセスが同じ物理フレームを自分の空間へマップする。**
//!
//! # `wl_shm.create_pool` の逆算
//!
//! **Wayland が要求する fd は `wl_shm.create_pool` の1つだけである**（`docs/wayland-inventory.md`）。
//! **クライアントが `size` バイトの無名の共有メモリを作り（`memfd_create`＋`ftruncate`）、
//! `mmap` して模様を書き、fd をソケットの補助データ（`SCM_RIGHTS`）で送る。**
//! **サーバーはその fd を `mmap` して読む。** **同じ物理フレームが両方の空間にマップされるので共有になる。**
//!
//! # 共有フレームの寿命（`ADR-0065` の「共有フレームの寿命」。Linux の思想）
//!
//! **フレームは通常どおりアロケータから取る**（`ftruncate`）。**共有であることは
//! 葉の PTE の空きビット 9 で目印を付ける**（`mmap` が `map_4kib` に `shared: true` で頼む）。
//! **`AddressSpace::detach` は目印の立った葉を集めない**——**Linux が `struct page` 相当の
//! 管理で空きビットを使うのと同じ思想である**（規模が合わないので `struct page` 相当の表は
//! 持たず、ここが参照数を持つ）。**最後の fd が閉じたときにアロケータへ返す。** **`spawn` の
//! 会計は共有フレームを除く**（(A-3)。ウィンドウの間に取ったまま返っていない分を `consumed` から引く）。
//!
//! # `detach` は自分でアロケータを借りる（`close` と `Drop` の両方から来る）
//!
//! **`detach` は `close` からも表の `Drop`（プロセスの終わり）からも来る。** **`Drop` は引数で
//! アロケータを持たないので、`detach` は自分で `take`／`give_back` する。** **破棄の経路では
//! `give_back` が `run_loaded_program` の前に済んでいて、破棄は隔離を使うので、`Drop` の時点で
//! アロケータは空いている**（着手前に破棄の順序を実測した。`ADR-0065`）。
//!
//! **ただし、ほかのタスクが借りていることはある**（2026-10-08）。切り離して起動するスロットの読み込みは、アロケータを
//! 持ったままティックで切り替わりうる。**借りられなければ、フレームを持ったまま「返す途中」の印を付けて残し、アイドルの
//! 定常経路が返す**（[`release_pending`]）。以前は表から消した後で借りていて、借りられないとフレームを返さずに、会計
//! （[`frames_held`]）だけを減らしていた。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::addr::PhysAddr;
use common::critical::Locked;

/// ページの大きさ。
pub const PAGE_SIZE: usize = 4096;

/// 同時に在れる共有メモリの数。
pub const MAX_SHM: usize = 2;

/// 1 つの共有メモリのページ数の上限（32 KiB）。**Wayland の初手に足る見込み。**
/// **FHD の枚（8 MiB）は入らない**（`ADR-0065` の限界。見直すきっかけは画面と入力の受け渡しの段階）。
pub const MAX_SHM_PAGES: usize = 8;

/// 共有メモリ 1 つ。
#[derive(Clone, Copy)]
struct Shm {
    in_use: bool,
    /// アロケータから取ったフレーム（`pages` 枚が有効）。
    frames: [PhysAddr; MAX_SHM_PAGES],
    /// ページ数（`ftruncate` で決まる）。
    pages: usize,
    /// 大きさ（バイト。`pages * PAGE_SIZE` 以下）。
    len: u64,
    /// この実体を指す fd の数。**0 でフレームを返す。**
    refs: u32,
    /// 参照は 0 になったが、アロケータが借りられずにフレームをまだ返せていない（2026-10-08）。**使う側からは、在らない
    /// ものとして見える**（[`Shm::live`]）。欄は空かないので、作り直しにも使われない。
    releasing: bool,
}

impl Shm {
    const EMPTY: Self = Self {
        in_use: false,
        frames: [PhysAddr::new_const(0); MAX_SHM_PAGES],
        pages: 0,
        len: 0,
        refs: 0,
        releasing: false,
    };

    /// fd から使える実体か（在って、返す途中でない）。
    fn live(&self) -> bool {
        self.in_use && !self.releasing
    }
}

static SHMS: Locked<[Shm; MAX_SHM]> = Locked::new([Shm::EMPTY; MAX_SHM]);

static CREATED: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);
static MAPPED_PAGES: AtomicU64 = AtomicU64::new(0);
static FDS_SENT: AtomicU64 = AtomicU64::new(0);
static FDS_RECEIVED: AtomicU64 = AtomicU64::new(0);
/// **いまアロケータから取ったままの共有フレームの数（`ADR-0065` の (A-3)）。**
///
/// **`spawn` の会計がウィンドウの差で読む**——**共有フレームはアロケータから出る（`consumed` に入る）
/// が `AddressSpace::detach` が飛ばす（`quarantined` に入らない）ので、ウィンドウの間に増えた分を `consumed` から引く。**
static SHARED_FRAMES_HELD: AtomicU64 = AtomicU64::new(0);

/// 返す途中の実体が在るか（BKL を取らずに読める印。2026-10-08。アイドルの定常経路が見る）。
static RELEASE_PENDING: AtomicBool = AtomicBool::new(false);

macro_rules! gauge {
    ($name:ident, $static:ident) => {
        pub fn $name() -> u64 {
            $static.load(Ordering::Relaxed)
        }
    };
}
gauge!(created, CREATED);
gauge!(released, RELEASED);
gauge!(mapped_pages, MAPPED_PAGES);
gauge!(fds_sent, FDS_SENT);
gauge!(fds_received, FDS_RECEIVED);
/// いまアロケータから取ったままの共有フレームの数（`ADR-0065` の (A-3)。会計がウィンドウの差で読む）。
pub fn frames_held() -> u64 {
    SHARED_FRAMES_HELD.load(Ordering::Relaxed)
}

pub fn note_fd_sent() {
    FDS_SENT.fetch_add(1, Ordering::Relaxed);
}
pub fn note_fd_received() {
    FDS_RECEIVED.fetch_add(1, Ordering::Relaxed);
}
pub fn note_mapped_page() {
    MAPPED_PAGES.fetch_add(1, Ordering::Relaxed);
}

/// 無名の共有メモリを作る。**大きさは 0。** **fd を持つ側が参照 1。**
pub fn create() -> Option<u8> {
    let mut shms = SHMS.lock();
    for index in 0..MAX_SHM {
        if !shms[index].in_use {
            shms[index] = Shm::EMPTY;
            shms[index].in_use = true;
            shms[index].refs = 1;
            CREATED.fetch_add(1, Ordering::Relaxed);
            return Some(index as u8);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncateOutcome {
    Pages(usize),
    TooLarge,
    AlreadySet,
    NoRoom,
    NoShm,
}

/// 何ページ要るか。**純粋。ホストで検査する。**
pub fn pages_for(size: u64) -> usize {
    size.div_ceil(PAGE_SIZE as u64) as usize
}

/// 大きさを据え、アロケータからフレームを取る。**中身は 0 にする。** **自分でアロケータを借りる。**
///
/// 破壊テスト (`ADR-0065`, shm-ftruncate-ignores-size): 何ページ要っても 1 ページしか取らない。
/// **模様の後ろが欠け、往復のバイト比べが落ちる。**
/// 据えた大きさ（バイト。`fstat` が答える。2026-10-06）。**無い共有メモリなら `None`。**
pub fn size_of(shm: u8) -> Option<u64> {
    let index = shm as usize;
    let shms = SHMS.lock();
    (index < MAX_SHM && shms[index].live()).then(|| shms[index].len)
}

///
/// **アロケータは呼び手が借りて渡す**（2026-10-08。`ftruncate` は、借りられるまで待ってから呼ぶ）。
pub fn set_size(
    shm: u8,
    size: u64,
    allocator: &mut crate::frame_allocator::FrameAllocator,
) -> TruncateOutcome {
    let index = shm as usize;
    {
        let shms = SHMS.lock();
        if index >= MAX_SHM || !shms[index].live() {
            return TruncateOutcome::NoShm;
        }
        if shms[index].pages != 0 {
            return TruncateOutcome::AlreadySet;
        }
    }
    let want = pages_for(size);
    if want > MAX_SHM_PAGES {
        return TruncateOutcome::TooLarge;
    }
    #[cfg(feature = "shm-ftruncate-ignores-size")]
    let want = want.min(1);
    let direct_map = common::addr::direct_map();
    let mut taken = [PhysAddr::new_const(0); MAX_SHM_PAGES];
    let mut got = 0usize;
    while got < want {
        let Some(frame) = allocator.allocate_frame() else {
            break;
        };
        // **中身を 0 にする。** **前の住人の中身を共有しない。**
        // SAFETY: いま取ったフレームで、direct map が覆っている。
        unsafe {
            core::ptr::write_bytes(
                direct_map.phys_to_virt(frame).as_u64() as *mut u8,
                0,
                PAGE_SIZE,
            )
        };
        taken[got] = frame;
        got += 1;
    }
    if got < want {
        for frame in taken.iter().take(got) {
            let _ = allocator.deallocate_frame(*frame);
        }
        return TruncateOutcome::NoRoom;
    }
    {
        let mut shms = SHMS.lock();
        shms[index].frames[..want].copy_from_slice(&taken[..want]);
        shms[index].pages = want;
        shms[index].len = size;
    }
    SHARED_FRAMES_HELD.fetch_add(want as u64, Ordering::Relaxed);
    TruncateOutcome::Pages(want)
}

/// `mmap` のために、フレームの物理アドレスと大きさをコピーして返す。**マップするのは呼び出し側。**
pub fn frames_of(shm: u8, out: &mut [PhysAddr; MAX_SHM_PAGES]) -> Option<(usize, u64)> {
    let index = shm as usize;
    let shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() || shms[index].pages == 0 {
        return None;
    }
    let pages = shms[index].pages;
    out[..pages].copy_from_slice(&shms[index].frames[..pages]);
    Some((pages, shms[index].len))
}

/// 参照を 1 つ増やす。**`sendmsg` の `SCM_RIGHTS` が呼ぶ。**
pub fn attach(shm: u8) -> bool {
    let index = shm as usize;
    let mut shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() {
        return false;
    }
    shms[index].refs = shms[index].refs.saturating_add(1);
    true
}

/// 参照を 1 つ減らす。**0 でアロケータへフレームを返す。** **自分でアロケータを借りる**
/// （`close` と表の `Drop` の両方から来る）。
///
/// 破壊テスト (`ADR-0065`, shm-close-keeps-refs): 参照を減らさない。**フレームが返らず、
/// 作った数と返した数が合わない。**
pub fn detach(shm: u8) {
    let index = shm as usize;
    let mut shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() {
        return;
    }
    #[cfg(not(feature = "shm-close-keeps-refs"))]
    {
        shms[index].refs = shms[index].refs.saturating_sub(1);
    }
    if shms[index].refs != 0 {
        return;
    }
    // **参照が 0 になった。** **表を空ける前に、アロケータを借りる**（2026-10-08）。借りられなければ、フレームを持ったまま
    // 返す途中の印を付けて残す（アイドルの定常経路が [`release_pending`] で返す）。
    if shms[index].pages > 0 {
        let Some(mut allocator) = crate::frame_allocator::Loan::take() else {
            shms[index].releasing = true;
            RELEASE_PENDING.store(true, Ordering::SeqCst);
            return;
        };
        free_frames(&mut shms[index], &mut allocator);
    } else {
        shms[index] = Shm::EMPTY;
        RELEASED.fetch_add(1, Ordering::Relaxed);
    }
}

/// 実体のフレームをアロケータへ返して、欄を空ける（2026-10-08。[`detach`] と [`release_pending`] の共通の本体）。
fn free_frames(slot: &mut Shm, allocator: &mut crate::frame_allocator::FrameAllocator) {
    for frame in slot.frames.iter().take(slot.pages) {
        let _ = allocator.deallocate_frame(*frame);
    }
    SHARED_FRAMES_HELD.fetch_sub(slot.pages as u64, Ordering::Relaxed);
    *slot = Shm::EMPTY;
    RELEASED.fetch_add(1, Ordering::Relaxed);
}

/// 返す途中の実体が在るか（2026-10-08。BKL を取らずに読める印）。
pub fn release_pending_exists() -> bool {
    RELEASE_PENDING.load(Ordering::SeqCst)
}

/// 返す途中の実体のフレームを返す（2026-10-08。アイドルの定常経路から、BKL を持って呼ぶ）。返した実体の数。**アロケータが
/// 借りられなければ何もしない**（次の機会に返す）。
pub fn release_pending() -> usize {
    let Some(mut allocator) = crate::frame_allocator::Loan::take() else {
        return 0;
    };
    let mut shms = SHMS.lock();
    let mut released = 0;
    for slot in shms.iter_mut().filter(|slot| slot.in_use && slot.releasing) {
        free_frames(slot, &mut allocator);
        released += 1;
    }
    RELEASE_PENDING.store(false, Ordering::SeqCst);
    released
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_round_up() {
        assert_eq!(pages_for(0), 0);
        assert_eq!(pages_for(1), 1);
        assert_eq!(pages_for(PAGE_SIZE as u64), 1);
        assert_eq!(pages_for(PAGE_SIZE as u64 + 1), 2);
        assert_eq!(pages_for(1536), 1);
    }
}
