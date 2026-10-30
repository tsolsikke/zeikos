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
//! リーフの PTE の空きビット 9 で目印を付ける**（`mmap` が `map_4kib` に `shared: true` で頼む）。
//! **`AddressSpace::detach` は目印の立ったリーフを集めない**——**Linux が `struct page` 相当の
//! 管理で空きビットを使うのと同じ思想である**（規模が合わないので `struct page` 相当の表は
//! 持たず、ここが参照数を持つ）。**最後の fd が閉じ、マップしたページを全部外したときにアロケータへ返す**（2026-10-30。
//! それまでは fd だけを数えていて、マップしたまま fd を全部閉じるとフレームが返った）。**`spawn` の
//! 会計は共有フレームを除く**（(A-3)。ウィンドウの間に取ったまま返っていない分を `consumed` から引く）。
//!
//! # マッピングの数と、返す時期（2026-10-30。`ADR-0065` の Addendum）
//!
//! fd の数（`refs`）とは別に、**マップしているページの数**（`maps`。全部のプロセスを合わせて）を持つ。`mmap` が足し、
//! `munmap` とプロセスの終わりが引く。ページで数えるので、途中だけを外して 1 つのマッピングが 2 つに分かれても、呼び手は
//! 分かれたことを知らなくてよい。
//!
//! **最後のページを外したときに TLB の世代を上げ、その世代を欄に控える**（[`map_detach`]）。フレームを返すのは、`refs` と
//! `maps` が 0 になり、**かつ控えた世代を全部の CPU が通り過ぎた後**である（`crate::bkl::generation_is_retired`。`ADR-0027`
//! の不変条件）。まだ通り過ぎていなければ「返す途中」の印を付けて残し、後で返す（[`release_retired_into`]）。一度もマップ
//! されなかった実体は、どの TLB にも載っていないので待たない。
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

/// 同時に在れる共有メモリの数（2026-10-26 に 2 から 16 へ上げた。`ADR-0065` の Addendum）。クライアント 1 つにつき pool が
/// 1〜2 個と、カーソルなどで使う。
pub const MAX_SHM: usize = 16;

/// 1 つの共有メモリのページ数の上限（16 MiB。2026-10-26 に 8 ページから上げた）。FHD の 2 枚（約 16.6 MB）まで入る。越える
/// `ftruncate` は `-EFBIG` である（Linux の `inode_newsize_ok` が、ファイルの大きさの上限を越えるときに返すもの）。
pub const MAX_SHM_PAGES: usize = 4096;

/// 全部の共有メモリを合わせたページ数の上限（64 MiB。2026-10-26）。越える `ftruncate` は `-ENOMEM` である。
pub const MAX_SHM_TOTAL_PAGES: usize = 16_384;

/// 共有メモリ 1 つ。**`Copy` を持たない**（2026-10-26）——フレームの並びが 32 KiB あるので、うっかり値でコピーすると
/// カーネルのスタックに載る。コピーが要る所は、コンパイラが止める。
struct Shm {
    in_use: bool,
    /// アロケータから取ったフレーム（`pages` 枚が有効）。
    frames: [PhysAddr; MAX_SHM_PAGES],
    /// ページ数（`ftruncate` で決まる）。
    pages: usize,
    /// 大きさ（バイト。`pages * PAGE_SIZE` 以下）。
    len: u64,
    /// この実体を指す fd の数（`SCM_RIGHTS` で渡す途中のものを含む）。**`maps` と一緒に 0 になったらフレームを返す。**
    refs: u32,
    /// マップしているページの数（全部のプロセスを合わせて。2026-10-30）。`mmap` が足し、`munmap` とプロセスの終わりが引く。
    maps: u32,
    /// 一度でもマップされたか（2026-10-30）。マップされたことの無いフレームは、どの CPU の TLB にも載っていない。
    ever_mapped: bool,
    /// 最後のページを外したときの TLB の世代（2026-10-30）。フレームは、全部の CPU がこの世代を通り過ぎてから返す。
    unmapped_generation: u64,
    /// 参照は 0 になったが、フレームをまだ返せていない（2026-10-08 はアロケータが借りられないとき。2026-10-30 から、TLB の
    /// 世代を全部の CPU が通り過ぎていないときも）。**使う側からは、在らない
    /// ものとして見える**（[`Shm::live`]）。欄は空かないので、作り直しにも使われない。
    releasing: bool,
    /// 封のビット（`F_SEAL_*`。2026-10-20）。**持って答えるだけで、まだ効かない**——縮める・伸ばす `ftruncate` と、書ける
    /// `mmap` を断る効き目は、2 回目の `ftruncate` とマッピングの数を持つ段で入る（`ADR-0065` の 2026-10-20 の Addendum）。
    seals: u32,
}

impl Shm {
    /// 空の欄（静的な表の初期値にだけ使う。欄を空けるときは [`Shm::clear`] を使い、32 KiB の値を作らない）。
    const EMPTY: Self = Self {
        in_use: false,
        frames: [PhysAddr::new_const(0); MAX_SHM_PAGES],
        pages: 0,
        len: 0,
        refs: 0,
        maps: 0,
        ever_mapped: false,
        unmapped_generation: 0,
        releasing: false,
        seals: 0,
    };

    /// fd から使える実体か（在って、返す途中でない）。
    fn live(&self) -> bool {
        self.in_use && !self.releasing
    }

    /// 欄を空ける（その場で書き換える）。フレームの並びは使っていた分だけ消す。
    fn clear(&mut self) {
        let pages = self.pages;
        self.frames[..pages].fill(PhysAddr::new_const(0));
        self.in_use = false;
        self.pages = 0;
        self.len = 0;
        self.refs = 0;
        self.maps = 0;
        self.ever_mapped = false;
        self.unmapped_generation = 0;
        self.releasing = false;
        self.seals = 0;
    }
}

static SHMS: Locked<[Shm; MAX_SHM]> = Locked::new([const { Shm::EMPTY }; MAX_SHM]);

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
/// 世代を待ってから返した実体の数（2026-10-30。計測）。
static RELEASED_AFTER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// 世代を全部の CPU が通り過ぎる前に返した実体の数（2026-10-30）。**0 でなければ誤りである**——返す直前に確かめ直し、
/// 数えて名指しの行を出す（`crate::syscall::report_shm_released_before_retired`）。
static RELEASED_BEFORE_RETIRED: AtomicU64 = AtomicU64::new(0);

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
gauge!(released_after_generation, RELEASED_AFTER_GENERATION);
gauge!(released_before_retired, RELEASED_BEFORE_RETIRED);
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

/// 封のビット（Linux の `F_SEAL_*`）。
pub const F_SEAL_SEAL: u32 = 0x0001;
pub const F_SEAL_SHRINK: u32 = 0x0002;
pub const F_SEAL_GROW: u32 = 0x0004;
pub const F_SEAL_WRITE: u32 = 0x0008;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
/// `chmod` で実行のビットを変えさせない封（Linux 6.3 から）。ZeikOS の memfd には実行のビットを変える道が無いので、持つだけである。
pub const F_SEAL_EXEC: u32 = 0x0020;
/// 知っている封のビットの全部（Linux の `F_ALL_SEALS`）。
pub const ALL_SEALS: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE | F_SEAL_EXEC;

/// 封を足せなかった理由（`fcntl(F_ADD_SEALS)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// 知らないビットがある（`-EINVAL`）。
    Unknown,
    /// 既に `F_SEAL_SEAL` が立っている（`-EPERM`）。
    Sealed,
}

/// 封を足した後の値（純粋な論理。Linux の `memfd_add_seals` と同じ順で見る——知らないビットが先、`F_SEAL_SEAL` が後）。
///
/// **`F_SEAL_EXEC` は、書き・伸び縮みの封も一緒に立てる**（2026-10-20。Linux 6.3 からの形で、実行のビットを持つ memfd の
/// とき。ZeikOS の memfd は `fstat` で 0777 と答えるので、いつもこの形になる。WSL で測った値は 62）。
pub fn added_seals(current: u32, add: u32) -> Result<u32, SealError> {
    if add & !ALL_SEALS != 0 {
        return Err(SealError::Unknown);
    }
    if current & F_SEAL_SEAL != 0 {
        return Err(SealError::Sealed);
    }
    let implied = if add & F_SEAL_EXEC != 0 {
        F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE
    } else {
        0
    };
    Ok(current | add | implied)
}

/// 作ったときの封（純粋な論理）。**`MFD_ALLOW_SEALING` が無ければ、最初から `F_SEAL_SEAL` を持つ**（Linux と同じ。封を足す
/// 求めは `-EPERM` になる）。
pub const fn initial_seals(allow_sealing: bool) -> u32 {
    if allow_sealing {
        0
    } else {
        F_SEAL_SEAL
    }
}

/// 無名の共有メモリを作る。**大きさは 0。** **fd を持つ側が参照 1。** 封は [`initial_seals`] で決める（2026-10-20）。
pub fn create(allow_sealing: bool) -> Option<u8> {
    let mut shms = SHMS.lock();
    for index in 0..MAX_SHM {
        if !shms[index].in_use {
            shms[index].clear();
            shms[index].in_use = true;
            shms[index].refs = 1;
            // 破壊テスト (2026-10-20, memfd-ignores-allow-sealing-test): `MFD_ALLOW_SEALING` を見ず、いつも封を足せる形にする。
            // 確かめのイメージ（wl-memfd-seals-rust）の、`MFD_ALLOW_SEALING` 無しの memfd への `F_ADD_SEALS` が 0 で通り、
            // イメージが名指しの行を出して 93 で終わる。
            shms[index].seals = if cfg!(feature = "memfd-ignores-allow-sealing-test") {
                0
            } else {
                initial_seals(allow_sealing)
            };
            CREATED.fetch_add(1, Ordering::Relaxed);
            return Some(index as u8);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncateOutcome {
    /// 据えた後のページ数。
    Pages(usize),
    /// 1 つの上限（[`MAX_SHM_PAGES`]）を越える（`-EFBIG`）。
    TooLarge,
    /// マップ中の実体を縮める向き（`-EINVAL`。`ADR-0065` の 2026-10-15 の Addendum。Linux は通し、外れたページに触ると
    /// `SIGBUS`。マップしていなければ縮める。2026-10-30）。
    Shrink,
    /// マップしていない実体を縮めるが、最後に外したときの TLB の世代を、まだ全部の CPU が通り過ぎていない（2026-10-30）。
    /// 呼び手は世代を待ってから、もう 1 度呼ぶ。
    NotRetired(u64),
    /// `F_SEAL_GROW` が立っているのに伸ばす（`-EPERM`）。
    Sealed,
    /// 全体の上限（[`MAX_SHM_TOTAL_PAGES`]）を越えるか、フレームが足りない（`-ENOMEM`）。
    NoRoom,
    NoShm,
}

/// 何ページ要るか。**純粋。ホストで検査する。**
pub fn pages_for(size: u64) -> usize {
    size.div_ceil(PAGE_SIZE as u64) as usize
}

/// 据えた大きさ（バイト。`fstat` が答える。2026-10-06）。**無い共有メモリなら `None`。**
pub fn size_of(shm: u8) -> Option<u64> {
    let index = shm as usize;
    let shms = SHMS.lock();
    (index < MAX_SHM && shms[index].live()).then(|| shms[index].len)
}

/// `ftruncate` の判断（2026-10-26。**純粋。ホストで検査する**）。[`set_size`] の doc の「向きごとの扱い」の順で決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatePlan {
    /// 同じ大きさ。何もしない（今のページ数）。
    Same(usize),
    /// 伸ばす。`want` ページにし、`more` ページを新しく取る（ページ数が変わらない伸ばしなら 0）。
    Grow { want: usize, more: usize },
    /// 縮める（2026-10-30。マップしていないときだけ）。`want` ページにし、後ろのページを返す。
    Shrink { want: usize },
    /// 断る。
    Refuse(TruncateOutcome),
}

/// 今の大きさ `len`（バイト）・ページ数 `pages`・封 `seals`・全体で取っているページ数 `held`・マップ中か（`mapped`）から、
/// `size` への `ftruncate` の判断を返す。
///
/// 縮める向き（2026-10-30）は、`F_SEAL_SHRINK` なら `-EPERM`（Linux の `shmem_setattr`）、マップ中なら `-EINVAL`
/// （ZeikOS の差）、どちらでもなければ縮める。
pub fn truncate_plan(
    len: u64,
    pages: usize,
    seals: u32,
    held: usize,
    mapped: bool,
    size: u64,
) -> TruncatePlan {
    if size == len {
        return TruncatePlan::Same(pages);
    }
    if size < len {
        if seals & F_SEAL_SHRINK != 0 {
            return TruncatePlan::Refuse(TruncateOutcome::Sealed);
        }
        if mapped {
            return TruncatePlan::Refuse(TruncateOutcome::Shrink);
        }
        return TruncatePlan::Shrink {
            want: pages_for(size),
        };
    }
    let want = pages_for(size);
    if want > MAX_SHM_PAGES {
        return TruncatePlan::Refuse(TruncateOutcome::TooLarge);
    }
    if seals & F_SEAL_GROW != 0 {
        return TruncatePlan::Refuse(TruncateOutcome::Sealed);
    }
    let more = want.saturating_sub(pages);
    if held.saturating_add(more) > MAX_SHM_TOTAL_PAGES {
        return TruncatePlan::Refuse(TruncateOutcome::NoRoom);
    }
    TruncatePlan::Grow { want, more }
}

/// 大きさを据え、足りないページのフレームをアロケータから取る。**取ったページの中身は 0 にする。**
///
/// **アロケータは呼び手が借りて渡す**（2026-10-08。`ftruncate` は、借りられるまで待ってから呼ぶ。以前は自分で借りていた）。
///
/// # 向きごとの扱い（2026-10-26。`ADR-0065` の Addendum）
///
/// - **同じ大きさ**は何もしない。
/// - **伸ばす**（初めての `ftruncate` も、大きさ 0 からの伸ばしである）。確かめる順は Linux と同じである——1 つの上限を越えれば
///   [`TruncateOutcome::TooLarge`]（`inode_newsize_ok` の `-EFBIG`）、`F_SEAL_GROW` が立っていれば [`TruncateOutcome::Sealed`]
///   （`shmem_setattr` の `-EPERM`）。その後に全体の上限とフレームを見る（[`TruncateOutcome::NoRoom`]）。**既にあるマッピングは
///   触らない**——伸ばした分は、後から `mmap` すれば見える（Linux と同じ）。
/// - **縮める**（2026-10-30）。`F_SEAL_SHRINK` なら [`TruncateOutcome::Sealed`]。マップ中なら [`TruncateOutcome::Shrink`] で
///   断る（`ADR-0065` の 2026-10-15 の Addendum、`ADR-0085` の決定 6）。マップしていなければ、後ろのページを返して縮める。
///   一度でもマップされた実体で、最後に外したときの TLB の世代をまだ全部の CPU が通り過ぎていなければ、返さずに
///   [`TruncateOutcome::NotRetired`] を返す（呼び手が待ってから、もう 1 度呼ぶ）。
///
/// **ページは `ftruncate` の時点で取り、0 で埋める**（Linux は書いた時点で取る。`ADR-0065` の Addendum の「合わせなかったもの」）。
/// システムコールの中なので、埋める間は BKL を持ち、割り込みも止まっている。
///
/// **共有メモリの表（`SHMS`）のロックは、フレームを 1 つ取るたびに放して取り直す。** その間に同じ欄を別のタスクが触らないのは、呼び手が BKL を
/// 持ち続けているからである。BKL を細かいロックに分けるときは、ここを見直す。
///
/// 破壊テスト (`ADR-0065`, shm-ftruncate-ignores-size): 何ページ要っても 1 ページしか取らない。
/// **模様の後ろが欠け、往復のバイト比べが落ちる。**
pub fn set_size(
    shm: u8,
    size: u64,
    allocator: &mut crate::frame_allocator::FrameAllocator,
) -> TruncateOutcome {
    let index = shm as usize;
    let (already, len, seals, maps) = {
        let shms = SHMS.lock();
        if index >= MAX_SHM || !shms[index].live() {
            return TruncateOutcome::NoShm;
        }
        (
            shms[index].pages,
            shms[index].len,
            shms[index].seals,
            shms[index].maps,
        )
    };
    let held = SHARED_FRAMES_HELD.load(Ordering::Relaxed) as usize;
    let (want, more) = match truncate_plan(len, already, seals, held, maps != 0, size) {
        TruncatePlan::Same(pages) => return TruncateOutcome::Pages(pages),
        TruncatePlan::Grow { want, more } => (want, more),
        TruncatePlan::Shrink { want } => return shrink(index, want, size, allocator),
        #[cfg(not(feature = "shm-ftruncate-shrinks-mapped-test"))]
        TruncatePlan::Refuse(outcome) => return outcome,
        #[cfg(feature = "shm-ftruncate-shrinks-mapped-test")]
        TruncatePlan::Refuse(outcome) if outcome != TruncateOutcome::Shrink => return outcome,
        // 破壊テスト (2026-10-15, shm-ftruncate-shrinks-mapped-test): 縮める向きなら、マップ中でも末尾のフレームを
        // アロケータへ返して縮める。マップしたままのページがアロケータに戻る（直す前に無かった形を、わざと作る）。
        #[cfg(feature = "shm-ftruncate-shrinks-mapped-test")]
        TruncatePlan::Refuse(_) => {
            let want = pages_for(size);
            let mut shms = SHMS.lock();
            for frame in &shms[index].frames[want..already] {
                let _ = allocator.deallocate_frame(*frame);
            }
            shms[index].pages = want;
            shms[index].len = size;
            SHARED_FRAMES_HELD.fetch_sub((already - want) as u64, Ordering::Relaxed);
            return TruncateOutcome::Pages(want);
        }
    };
    #[cfg(feature = "shm-ftruncate-ignores-size")]
    let (want, more) = {
        let want = want.min(already.max(1));
        (want, want.saturating_sub(already))
    };
    let direct_map = common::addr::direct_map();
    for got in 0..more {
        let Some(frame) = allocator.allocate_frame() else {
            // **足りなければ、取った分を返して、大きさを変えない。**
            let mut shms = SHMS.lock();
            for frame in &shms[index].frames[already..already + got] {
                let _ = allocator.deallocate_frame(*frame);
            }
            shms[index].frames[already..already + got].fill(PhysAddr::new_const(0));
            return TruncateOutcome::NoRoom;
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
        // 取るたびに欄へ置く（32 KiB の並びをスタックに作らない）。`pages` はまだ変えないので、置いた分は使う側から見えない。
        SHMS.lock()[index].frames[already + got] = frame;
    }
    {
        let mut shms = SHMS.lock();
        shms[index].pages = want;
        shms[index].len = size;
    }
    SHARED_FRAMES_HELD.fetch_add(more as u64, Ordering::Relaxed);
    TruncateOutcome::Pages(want)
}

/// マップしていない実体を `want` ページへ縮め、後ろのページのフレームを返す（2026-10-30。[`set_size`] の縮める向き）。
/// 一度でもマップされた実体で、最後に外したときの世代をまだ全部の CPU が通り過ぎていなければ、何も変えずに
/// [`TruncateOutcome::NotRetired`] を返す。
fn shrink(
    index: usize,
    want: usize,
    size: u64,
    allocator: &mut crate::frame_allocator::FrameAllocator,
) -> TruncateOutcome {
    let mut shms = SHMS.lock();
    let slot = &mut shms[index];
    if slot.ever_mapped
        && !cfg!(feature = "shm-release-ignores-generation-test")
        && !crate::bkl::generation_is_retired(slot.unmapped_generation)
    {
        return TruncateOutcome::NotRetired(slot.unmapped_generation);
    }
    let already = slot.pages;
    // **返す直前に確かめ直す**（[`free_frames`] と同じ。破壊テストで世代を見ない形だけが、ここへ来る）。
    if slot.ever_mapped && !crate::bkl::generation_is_retired(slot.unmapped_generation) {
        RELEASED_BEFORE_RETIRED.fetch_add(1, Ordering::Relaxed);
        crate::syscall::report_shm_released_before_retired(
            already - want,
            slot.unmapped_generation,
        );
    }
    for frame in &slot.frames[want..already] {
        let _ = allocator.deallocate_frame(*frame);
    }
    slot.frames[want..already].fill(PhysAddr::new_const(0));
    slot.pages = want;
    slot.len = size;
    SHARED_FRAMES_HELD.fetch_sub((already - want) as u64, Ordering::Relaxed);
    TruncateOutcome::Pages(want)
}

/// 今の封（`fcntl(F_GET_SEALS)`。2026-10-20）。無い共有メモリなら `None`。
pub fn seals(shm: u8) -> Option<u32> {
    let index = shm as usize;
    let shms = SHMS.lock();
    (index < MAX_SHM && shms[index].live()).then(|| shms[index].seals)
}

/// 封を足す（`fcntl(F_ADD_SEALS)`。2026-10-20）。**無い共有メモリなら `None`。** 足せたら `Some(Ok(足した後の値))`。
pub fn add_seals(shm: u8, add: u32) -> Option<Result<u32, SealError>> {
    let index = shm as usize;
    let mut shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() {
        return None;
    }
    let result = added_seals(shms[index].seals, add);
    if let Ok(seals) = result {
        // 破壊テスト (2026-10-20, memfd-seals-not-kept-test): 足した封を持たない（2026-10-19 までの、受けて 0 を返すだけの形）。
        // 確かめのイメージの `F_GET_SEALS` が 0 を返し、イメージが名指しの行を出して 94 で終わる。
        if !cfg!(feature = "memfd-seals-not-kept-test") {
            shms[index].seals = seals;
        }
    }
    Some(result)
}

/// 据えたページ数と参照の数（2 回目の `ftruncate` を名指しして断る行のため。2026-10-15）。無ければ `None`。
pub fn size_and_refs(shm: u8) -> Option<(usize, u32)> {
    let index = shm as usize;
    let shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() {
        return None;
    }
    Some((shms[index].pages, shms[index].refs))
}

/// `mmap` のために、据えたページ数と大きさを返す。**大きさが 0 の共有メモリと、無い共有メモリなら `None`。** フレームは
/// [`frame_at`] で 1 ページずつ引く（2026-10-26。並びを丸ごとコピーすると、4,096 本で 32 KiB になる）。
pub fn mapping_size(shm: u8) -> Option<(usize, u64)> {
    let index = shm as usize;
    let shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() || shms[index].pages == 0 {
        return None;
    }
    Some((shms[index].pages, shms[index].len))
}

/// `page` 枚目のフレーム（2026-10-26。`mmap` が 1 ページずつ引く）。**据えたページの外か、無い共有メモリなら `None`。**
pub fn frame_at(shm: u8, page: usize) -> Option<PhysAddr> {
    let index = shm as usize;
    let shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() || page >= shms[index].pages {
        return None;
    }
    Some(shms[index].frames[page])
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

/// 参照を 1 つ減らす。**`refs` と `maps` が 0 になったら、フレームを返す**（`retire`）。**自分でアロケータを借りる**
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
    retire(&mut shms[index]);
}

/// `pages` ページをマップしたことを数える（2026-10-30。`mmap` が、マップし終えた後に呼ぶ）。無い共有メモリなら偽。
pub fn map_attach(shm: u8, pages: u32) -> bool {
    let index = shm as usize;
    let mut shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].live() {
        return false;
    }
    shms[index].maps = shms[index].maps.saturating_add(pages);
    shms[index].ever_mapped = true;
    true
}

/// `pages` ページを外したことを数える（2026-10-30。`munmap` とプロセスの終わりが、リーフを外した後に呼ぶ）。**最後のページを
/// 外したら、TLB の世代を上げて控える**——外し終えてから上げるので、その世代を全部の CPU が通り過ぎれば、どの TLB にも
/// 古い翻訳は残っていない（`crate::bkl::generation_is_retired` の doc）。`refs` も 0 なら、フレームを返す（`retire`）。
///
/// **BKL を持って呼ぶこと**（世代を上げるため）。
///
/// 破壊テスト (2026-10-30, shm-munmap-keeps-maps-test): `munmap` の分を引かない（呼び手の側。`crate::syscall`）。
pub fn map_detach(shm: u8, pages: u32) {
    let index = shm as usize;
    let mut shms = SHMS.lock();
    if index >= MAX_SHM || !shms[index].in_use {
        return;
    }
    let slot = &mut shms[index];
    // **足した数より多く引くのは、数え方の不具合である**（2026-10-30）。黙って 0 に止めると不具合が隠れるので、名指しの行を
    // 出し、デバッグビルドでは止める。リリースビルドでは 0 に止めて続ける（マップしたページが残っていても、ここから先は
    // 返す時期を早めない——`maps` を 0 にした後は、世代を待って返す）。
    if pages > slot.maps {
        crate::syscall::report_shm_maps_underflow(index, slot.maps, pages);
        debug_assert!(
            pages <= slot.maps,
            "shm {index}: unmapping {pages} page(s) but only {} are mapped",
            slot.maps
        );
    }
    slot.maps = slot.maps.saturating_sub(pages);
    if slot.maps == 0 {
        crate::bkl::note_mapping_changed();
        slot.unmapped_generation = crate::bkl::tlb_generation();
    }
    if slot.live() {
        retire(slot);
    }
}

/// フレームを返す時期（2026-10-30。**純粋。ホストで検査する**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseWhen {
    /// まだ参照（fd か、マップしたページ）が在る。
    NotYet,
    /// すぐ返してよい（一度もマップされなかったか、最後に外したときの世代を全部の CPU が通り過ぎた）。
    Now,
    /// 最後に外したときの世代を、全部の CPU が通り過ぎるのを待つ。
    AfterGeneration,
}

/// `refs`・`maps`・一度でもマップされたか・控えた世代を全部の CPU が通り過ぎたか（`retired`）から、返す時期を決める。
pub fn release_when(refs: u32, maps: u32, ever_mapped: bool, retired: bool) -> ReleaseWhen {
    if refs != 0 || maps != 0 {
        ReleaseWhen::NotYet
    } else if !ever_mapped || retired {
        ReleaseWhen::Now
    } else {
        ReleaseWhen::AfterGeneration
    }
}

/// 参照が無くなっていれば、フレームを返すか、返す途中の印を付ける（2026-10-30。[`detach`] と [`map_detach`] の共通の本体）。
///
/// **表を空ける前に、アロケータを借りる**（2026-10-08）。借りられなければ、フレームを持ったまま返す途中の印を付けて残す。
/// 控えた世代をまだ全部の CPU が通り過ぎていなければ、同じく印を付けて残す。どちらも [`release_retired_into`] が後で返す。
///
/// 破壊テスト (2026-10-30, shm-release-ignores-generation-test): 世代を見ず、参照が無くなればすぐ返す。返す直前の確かめ直し
/// （`free_frames`）が、名指しの行を出す。
fn retire(slot: &mut Shm) {
    let retired = cfg!(feature = "shm-release-ignores-generation-test")
        || crate::bkl::generation_is_retired(slot.unmapped_generation);
    match release_when(slot.refs, slot.maps, slot.ever_mapped, retired) {
        ReleaseWhen::NotYet => {}
        ReleaseWhen::Now if slot.pages == 0 => {
            slot.clear();
            RELEASED.fetch_add(1, Ordering::Relaxed);
        }
        ReleaseWhen::Now => match crate::frame_allocator::Loan::take() {
            Some(mut allocator) => free_frames(slot, &mut allocator),
            None => {
                slot.releasing = true;
                RELEASE_PENDING.store(true, Ordering::SeqCst);
            }
        },
        ReleaseWhen::AfterGeneration => {
            slot.releasing = true;
            RELEASE_PENDING.store(true, Ordering::SeqCst);
        }
    }
}

/// 実体のフレームをアロケータへ返して、欄を空ける（2026-10-08。`retire` と [`release_retired_into`] の共通の本体）。
///
/// **返す直前に、控えた世代を全部の CPU が通り過ぎたかを確かめ直す**（2026-10-30）。一度でもマップされた実体で、まだ通り
/// 過ぎていなければ誤りなので、数えて名指しの行を出す（返すのは止めない——止めると、別の形で漏れる）。
fn free_frames(slot: &mut Shm, allocator: &mut crate::frame_allocator::FrameAllocator) {
    if slot.ever_mapped && !crate::bkl::generation_is_retired(slot.unmapped_generation) {
        RELEASED_BEFORE_RETIRED.fetch_add(1, Ordering::Relaxed);
        crate::syscall::report_shm_released_before_retired(slot.pages, slot.unmapped_generation);
    }
    for frame in slot.frames.iter().take(slot.pages) {
        let _ = allocator.deallocate_frame(*frame);
    }
    SHARED_FRAMES_HELD.fetch_sub(slot.pages as u64, Ordering::Relaxed);
    slot.clear();
    RELEASED.fetch_add(1, Ordering::Relaxed);
}

/// 返す途中の実体が在るか（2026-10-08。BKL を取らずに読める印）。
pub fn release_pending_exists() -> bool {
    RELEASE_PENDING.load(Ordering::SeqCst)
}

/// 返す途中の実体のうち、返せるもの（控えた世代を全部の CPU が通り過ぎたもの）のフレームを、借りたアロケータへ返す
/// （2026-10-30）。返した実体の数。**BKL を持って呼ぶこと**（世代の判定のため）。
///
/// 呼ぶ所は 3 つある——アイドルの定常経路（[`release_pending`]）、システムコールがアロケータを借りた直後
/// （`crate::syscall` の `borrow_allocator_counting`）、`memfd_create` と `ftruncate` が `-ENOMEM` を返す前（同じく
/// `crate::syscall`。返す途中の分が残っているせいで断らないため）。
pub fn release_retired_into(allocator: &mut crate::frame_allocator::FrameAllocator) -> usize {
    let mut shms = SHMS.lock();
    let mut released = 0;
    let mut left = false;
    for slot in shms.iter_mut().filter(|slot| slot.in_use && slot.releasing) {
        if crate::bkl::generation_is_retired(slot.unmapped_generation) {
            if slot.ever_mapped {
                RELEASED_AFTER_GENERATION.fetch_add(1, Ordering::Relaxed);
            }
            free_frames(slot, allocator);
            released += 1;
        } else {
            left = true;
        }
    }
    RELEASE_PENDING.store(left, Ordering::SeqCst);
    released
}

/// 返す途中の実体が持っているフレームの数（2026-10-30。起動時の確かめの会計が、ウィンドウの前後の差で読む）。
pub fn pending_frames() -> u64 {
    let shms = SHMS.lock();
    shms.iter()
        .filter(|slot| slot.in_use && slot.releasing)
        .map(|slot| slot.pages as u64)
        .sum()
}

/// 返す途中の実体が待っている世代のうち、いちばん新しいもの（2026-10-30）。無ければ `None`。**これを全部の CPU が通り
/// 過ぎれば、返す途中の実体は全部返せる。**
pub fn pending_generation() -> Option<u64> {
    let shms = SHMS.lock();
    shms.iter()
        .filter(|slot| slot.in_use && slot.releasing)
        .map(|slot| slot.unmapped_generation)
        .max()
}

/// 返す途中の実体を、TLB の世代が退くのを待って返す（2026-10-30。起動時の確かめが、共有メモリの数を出す前に呼ぶ）。待った
/// ティックの数を返す。**上限は `limit_ticks`**（世代は、全部の CPU が 1 ティック以内に BKL を通って進める）。**BKL を持たずに
/// 呼ぶこと**——返すときだけ取る。ほかの CPU が世代を進めるには、取れる必要がある。
pub fn release_pending_within(limit_ticks: u64) -> u64 {
    let started = crate::arch::x86_64::monotonic_ticks();
    while release_pending_exists()
        && crate::arch::x86_64::monotonic_ticks() < started.saturating_add(limit_ticks)
    {
        {
            let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
            release_pending();
        }
        core::hint::spin_loop();
    }
    crate::arch::x86_64::monotonic_ticks().saturating_sub(started)
}

/// 返す途中の実体のフレームを返す（2026-10-08。アイドルの定常経路から、BKL を持って呼ぶ）。返した実体の数。**アロケータが
/// 借りられなければ何もしない**（次の機会に返す）。2026-10-30 から、控えた世代を全部の CPU が通り過ぎたものだけを返す
/// （[`release_retired_into`]）。
pub fn release_pending() -> usize {
    let Some(mut allocator) = crate::frame_allocator::Loan::take() else {
        return 0;
    };
    release_retired_into(&mut allocator)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 返す時期（2026-10-30）。fd とマップしたページのどちらかが残っていれば返さない。両方が無くなったら、一度もマップされ
    /// なかったか、最後に外したときの世代を全部の CPU が通り過ぎていれば、すぐ返す。そうでなければ世代を待つ。
    #[test]
    fn frames_go_back_only_after_the_fds_the_mappings_and_the_generation_are_gone() {
        assert_eq!(release_when(1, 0, false, true), ReleaseWhen::NotYet);
        assert_eq!(release_when(0, 3, true, true), ReleaseWhen::NotYet);
        assert_eq!(release_when(1, 3, true, false), ReleaseWhen::NotYet);
        assert_eq!(release_when(0, 0, false, false), ReleaseWhen::Now);
        assert_eq!(release_when(0, 0, true, true), ReleaseWhen::Now);
        assert_eq!(
            release_when(0, 0, true, false),
            ReleaseWhen::AfterGeneration
        );
    }

    #[test]
    fn truncate_plan_follows_the_linux_order() {
        const PAGE: u64 = PAGE_SIZE as u64;
        // 同じ大きさは何もしない。
        assert_eq!(
            truncate_plan(256_000, 63, 0, 63, false, 256_000),
            TruncatePlan::Same(63)
        );
        // 初めての ftruncate は 0 からの伸ばし。タスクバーの pool が 2 枚目のバッファで伸びる形も。
        assert_eq!(
            truncate_plan(0, 0, 0, 0, false, 256_000),
            TruncatePlan::Grow { want: 63, more: 63 }
        );
        assert_eq!(
            truncate_plan(256_000, 63, 0, 63, false, 512_000),
            TruncatePlan::Grow {
                want: 125,
                more: 62
            }
        );
        // ページ数の変わらない伸ばし。
        assert_eq!(
            truncate_plan(100, 1, 0, 1, false, 200),
            TruncatePlan::Grow { want: 1, more: 0 }
        );
        // 縮める向き（2026-10-30）。マップ中なら断り（-EINVAL）、マップしていなければ縮める。F_SEAL_SHRINK なら、マップ中かに
        // よらず -EPERM（Linux の shmem_setattr）。
        assert_eq!(
            truncate_plan(200, 1, 0, 1, true, 100),
            TruncatePlan::Refuse(TruncateOutcome::Shrink)
        );
        assert_eq!(
            truncate_plan(512_000, 125, 0, 125, false, 256_000),
            TruncatePlan::Shrink { want: 63 }
        );
        assert_eq!(
            truncate_plan(PAGE, 1, 0, 1, false, 0),
            TruncatePlan::Shrink { want: 0 }
        );
        assert_eq!(
            truncate_plan(2 * PAGE, 2, F_SEAL_SHRINK, 2, true, PAGE),
            TruncatePlan::Refuse(TruncateOutcome::Sealed)
        );
        assert_eq!(
            truncate_plan(2 * PAGE, 2, F_SEAL_GROW, 2, false, PAGE),
            TruncatePlan::Shrink { want: 1 }
        );
        // 1 つの上限ちょうどは通り、1 バイト越えると -EFBIG。封より先に見る（Linux の setattr_prepare の後に封）。
        let max = MAX_SHM_PAGES as u64 * PAGE;
        assert_eq!(
            truncate_plan(0, 0, 0, 0, false, max),
            TruncatePlan::Grow {
                want: MAX_SHM_PAGES,
                more: MAX_SHM_PAGES
            }
        );
        assert_eq!(
            truncate_plan(0, 0, F_SEAL_GROW, 0, false, max + 1),
            TruncatePlan::Refuse(TruncateOutcome::TooLarge)
        );
        // F_SEAL_GROW なら伸ばせない。F_SEAL_SHRINK だけなら伸ばせる。
        assert_eq!(
            truncate_plan(PAGE, 1, F_SEAL_GROW, 1, false, 2 * PAGE),
            TruncatePlan::Refuse(TruncateOutcome::Sealed)
        );
        assert_eq!(
            truncate_plan(PAGE, 1, F_SEAL_SHRINK, 1, false, 2 * PAGE),
            TruncatePlan::Grow { want: 2, more: 1 }
        );
        // 全体の上限ちょうどは通り、1 ページ越えると -ENOMEM。
        let held = MAX_SHM_TOTAL_PAGES - 1;
        assert_eq!(
            truncate_plan(0, 0, 0, held, false, PAGE),
            TruncatePlan::Grow { want: 1, more: 1 }
        );
        assert_eq!(
            truncate_plan(0, 0, 0, held, false, 2 * PAGE),
            TruncatePlan::Refuse(TruncateOutcome::NoRoom)
        );
    }

    #[test]
    fn pages_round_up() {
        assert_eq!(pages_for(0), 0);
        assert_eq!(pages_for(1), 1);
        assert_eq!(pages_for(PAGE_SIZE as u64), 1);
        assert_eq!(pages_for(PAGE_SIZE as u64 + 1), 2);
        assert_eq!(pages_for(1536), 1);
    }

    /// 封の足し方は Linux と同じ順である（2026-10-20）——知らないビットは `-EINVAL`、`F_SEAL_SEAL` の後は `-EPERM`、それ以外は
    /// 重ねて持つ。`MFD_ALLOW_SEALING` が無い memfd は、最初から `F_SEAL_SEAL` を持つ。
    #[test]
    fn seals_are_added_the_way_linux_adds_them() {
        assert_eq!(initial_seals(true), 0);
        assert_eq!(initial_seals(false), F_SEAL_SEAL);
        assert_eq!(added_seals(0, F_SEAL_SHRINK), Ok(F_SEAL_SHRINK));
        assert_eq!(
            added_seals(F_SEAL_SHRINK, F_SEAL_GROW | F_SEAL_SEAL),
            Ok(F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL)
        );
        assert_eq!(
            added_seals(F_SEAL_SEAL, F_SEAL_SHRINK),
            Err(SealError::Sealed)
        );
        assert_eq!(added_seals(F_SEAL_SEAL, 0), Err(SealError::Sealed));
        assert_eq!(added_seals(0, 0x40), Err(SealError::Unknown));
        assert_eq!(
            added_seals(F_SEAL_SEAL, 0x40),
            Err(SealError::Unknown),
            "an unknown bit is -EINVAL before the sealed check"
        );
        assert_eq!(added_seals(0, F_SEAL_FUTURE_WRITE), Ok(F_SEAL_FUTURE_WRITE));
        assert_eq!(
            added_seals(0, F_SEAL_EXEC),
            Ok(F_SEAL_EXEC | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE),
            "F_SEAL_EXEC brings the write and size seals with it (62, as Linux answers)"
        );
    }
}
