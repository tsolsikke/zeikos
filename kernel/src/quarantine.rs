//! 解放したフレームを、TLB から消えるまで隔離する（S7-b）。
//!
//! # 何を守っているか
//!
//! ADR-0027 の Addendum の不変条件である——**フレームは、全コアの
//! `SEEN_GENERATION` が解放時の世代を追い越すまで配られない。**
//!
//! **マッピングを外して世代を上げても、他コアの TLB には古い翻訳が残りうる。** 外した
//! フレームを即座にアロケータへ返して再利用すると、**そのウィンドウで古い翻訳が生きた
//! ページを別用途に使われる。** ここはそのウィンドウを閉じる。
//!
//! # ack も新しいプロトコルも要らない
//!
//! カーネル入口はティックごとに BKL を取るので、**走っている全コアは 100Hz で必ず
//! `bkl::acquire` を通る。** したがって**隔離は遅くとも 1 ティックで解ける。**
//! 送るものも待つものも増えない。
//!
//! # 固定長である
//!
//! **ヒープを使わない。** このカーネルの様式に合うことと（`MAX_CPUS`・
//! `WORKER_COUNT`・フレームアロケータの範囲上限、いずれも固定である）、
//! **S6-c で測って確かめた「定常経路に解放されない確保は無い」を守るためである。**
//!
//! # この形は 2 案のうちの一方である
//!
//! 不変条件の実装には、**別の隔離リストで持つ**案（ここ）と、**フリーリストの
//! エントリに解放時の世代を刻む**案がある。**ADR はどちらも同じ不変条件だと述べて
//! おり、選択は対象外である**（`docs/roadmap.md` の S7）。**フレームアロケータの
//! データ構造の見直し（S7-d）で確定させる。** 刻む案を採るなら、このモジュールは
//! 落ちる。

use crate::arch::x86_64::AddressSpace;
use crate::frame_allocator::FrameAllocator;
use common::addr::{DirectMap, PhysAddr};

/// 隔離できるフレームの本数。
///
/// **1 ティック（10ms）の間に解放した分だけを持てばよい。** 隔離はそれで解けるので、
/// ここが溜まり続けることはない。
///
/// # 「この桁で足りる」は偽だった（B-d。2026-09-11）
///
/// **以前ここは「プロセス 1 つの破棄で必要なのは PML4 と下位テーブル数枚と
/// ユーザーページであり、この桁で足りる見込みである」と書き、「見込みであって、
/// 測っていない」と断っていた。** **B-d で初めて偽になった。**
///
/// **`/bin/ttfglyph` はフォントを読むために 512 KiB のヒープを取る**——
/// **実測で 142 フレームを消費し、64 しか隔離できず、78 枚を漏らした**
/// （**破棄の会計がその場で `DestroyAccounting` として示した**。
/// **黙って漏れなかったのは、会計が在ったからである**）。
///
/// **256 へ上げた。** **`brk` が伸ばせる上限はユーザースタックの下端までで
/// あり**（`crate::userland::HEAP_LIMIT`。約 8 MiB）、**どんな固定容量でも
/// 原理的には溢れうる。** **溢れは漏れとして必ず示される**ので、**ここは
/// 「測った値に余裕を足す」形のままにする**——**穴を持つアロケータも、
/// ヒープを返さずに終わるプログラムも、まだ無い。**
///
/// **4096 へ上げ、同じ日に 256 へ戻した（2026-10-07）。** Linux 向けの像（Seinas の fbdev の裏側。1.5 MiB の像 + 4 MiB の
/// 確保が 2 つ）が途中で落ちたとき、480 フレームが破棄に来て 224 が漏れた（実測。`ADR-0083` の Addendum）ので、いったん
/// 4096 へ上げた。**その後、破棄に 2 つの道を置いた**（[`Retire`]。`ADR-0027` の 2026-10-07 の Addendum）——この CPU でしか
/// 走らなかった空間は隔離を通らず、隔離を通る空間は一杯になっても待って続ける。**容量は、1 回の待ち（1 ティック）で返せる量を
/// 決めるだけになり、溢れは漏れではなく待ちになった。** 256 で足りる（表は 1 本 6 KiB。静的なので遠征スタックは増えない）。
///
/// 一杯になったときの扱いは [`retire_address_space`] の doc。
pub const QUARANTINE_CAPACITY: usize = 256;

/// 空間を壊すときの、フレームの返し方（2026-10-07）。
///
/// # 2 つの道
///
/// - **その場で返す**（[`Retire::Directly`]）——**この CPU でしか走らなかった空間**に使う。古い翻訳を持ちうるのは、
///   その空間を走らせた CPU の TLB だけで、その CPU は遠征から戻るときに CR3 を載せ替えて（G ビットは使わない）
///   翻訳を全部落としている。**念のため、返す前にもう 1 度この CPU の TLB を落とす**（[`crate::bkl::flush_this_cpu`]）。
///   隔離を通らないので、容量の上限が無く、漏れない。**`munmap` がその場で返しているのと同じ根拠である**
///   （`crate::syscall` の `release_range_and_unmap`）。
/// - **隔離を通す**（[`Retire::ThroughQuarantine`]）——**ほかの CPU でも走った空間**（スレッド、または CPU をまたいだ
///   移動が入ったとき）に使う。`ADR-0027` の Addendum の不変条件どおり、世代が退くまで隔離する。**隔離が一杯になったら、
///   BKL を放して世代が退くのを待ち、退いた分を返してから続ける**（`wait_until_retired`）。**待てなかったときだけ漏らす**
///   （上限 `WAIT_LIMIT_CYCLES`。起動の直線で、ほかの CPU が BKL を通らない間）。
///
/// # どちらを使うかは「どの CPU で走ったか」で決める
///
/// プロセスは走った CPU の印（`UserProcess::ran_on`）を持つ。**今は 1 つの CPU に留まる**（切り替えで CPU を変えない）
/// ので、いつも「その場で返す」になる。**スレッド（M3a）が入って印が 2 つ以上になった空間は、隔離の道を通る**——
/// 前提に頼らず、印で分ける。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retire {
    /// この CPU でしか走らなかった空間。その場でアロケータへ返す。
    Directly,
    /// ほかの CPU でも走った（かもしれない）空間。世代が退くまで隔離する。
    ThroughQuarantine,
}

/// 空間を壊した結果（2026-10-07）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retired {
    /// その場でアロケータへ返した本数（[`Retire::Directly`]）。
    pub returned: usize,
    /// 隔離へ入れた本数（[`Retire::ThroughQuarantine`]）。
    pub quarantined: usize,
    /// 隔離へ入れた後、同じ破棄の中で世代が退いて返した本数（一杯になって待った分）。
    pub released_meanwhile: usize,
    /// 隔離に残っていた、前の破棄のフレームのうち、この破棄の始めに返した本数（会計の補正に使う）。
    pub released_others: usize,
    /// 漏らした本数（隔離が一杯で、待てなかった分）。
    pub leaked: usize,
    /// 一杯になって待った回数。
    pub waits: usize,
    /// 壊すのに掛かったサイクル（`rdtsc`。揺れる値。判定に使わない）。
    pub cycles: u64,
}

impl Retired {
    pub const ZERO: Retired = Retired {
        returned: 0,
        quarantined: 0,
        released_meanwhile: 0,
        released_others: 0,
        leaked: 0,
        waits: 0,
        cycles: 0,
    };

    /// 破棄が集めた本数（返した + 隔離へ入れた + 漏らした）。空間が取った本数と釣り合うべき数。
    pub fn collected(&self) -> usize {
        self.returned + self.quarantined + self.leaked
    }

    /// この破棄のフレームのうち、まだ隔離に残っている本数。
    pub fn still_quarantined(&self) -> usize {
        self.quarantined.saturating_sub(self.released_meanwhile)
    }
}

/// 一杯になった隔離が解けるのを待つ上限（`rdtsc` のサイクル）。**1 ティック（10 ms）で解ける**ので、十分に長い。
/// 越えるのは、ほかの CPU が BKL を通らない区間（起動の直線）だけである。
const WAIT_LIMIT_CYCLES: u64 = 2_000_000_000;

/// 空間を壊す側が、フレームのアロケータへどう触るか（2026-10-07。運用者のレビューで分けた）。
///
/// - [`Frames::OnLoan`]——**使うときだけ借りて、使い終えたら返す**（`crate::frame_allocator::take` / `give_back`）。
///   **隔離の道で BKL を放して待つ間は、何も持たない**——持ったままだと、その間にフレームを取りに来たほかの CPU が
///   「貸し出し中」で失敗する（待たされるのではなく、`mmap` や `spawn` が失敗で返る）。プロセスの破棄はこちらである。
/// - [`Frames::Owned`]——呼び出し側が持っているアロケータを使う。**起動の試し（`demo_two_address_spaces`）だけ**——
///   起動の直線で、ほかの CPU はフレームを取らず、試しの空間は小さいので待ちも起きない。
pub enum Frames<'a> {
    Owned(&'a mut FrameAllocator),
    OnLoan,
}

impl Frames<'_> {
    /// アロケータを借りる。**借りられなければ `None`**（`OnLoan` で、ほかの CPU が借りている間）。返すのは落ちるときである。
    fn borrow(&mut self) -> Option<Lease<'_>> {
        match self {
            Frames::Owned(allocator) => Some(Lease::Owned(allocator)),
            Frames::OnLoan => {
                crate::frame_allocator::take().map(|allocator| Lease::Taken(Some(allocator)))
            }
        }
    }

    /// アロケータを使う。**借りられなければ `None`。**
    fn with<T>(&mut self, f: impl FnOnce(&mut FrameAllocator) -> T) -> Option<T> {
        let mut lease = self.borrow()?;
        Some(f(&mut lease))
    }
}

/// 借りている間のアロケータ（[`Frames::borrow`]）。**`Taken` は落ちるときに返す**（`Option` は、落ちるときに値で取り出して
/// `give_back` へ渡すため。落ちるまでは必ず `Some`）。
enum Lease<'a> {
    Owned(&'a mut FrameAllocator),
    Taken(Option<&'static mut FrameAllocator>),
}

impl core::ops::Deref for Lease<'_> {
    type Target = FrameAllocator;
    fn deref(&self) -> &FrameAllocator {
        match self {
            Lease::Owned(allocator) => allocator,
            Lease::Taken(allocator) => allocator
                .as_deref()
                .expect("the lease holds the allocator until it drops"),
        }
    }
}

impl core::ops::DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut FrameAllocator {
        match self {
            Lease::Owned(allocator) => allocator,
            Lease::Taken(allocator) => allocator
                .as_deref_mut()
                .expect("the lease holds the allocator until it drops"),
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Lease::Taken(allocator) = self {
            if let Some(allocator) = allocator.take() {
                crate::frame_allocator::give_back(allocator);
            }
        }
    }
}

/// 退いた分を返す（借りられるまで、BKL を放して少し待つのを繰り返す。上限 100 回）。返した本数。借りられなければ `None`。
fn release_retired_when_borrowable(
    quarantine: &mut Quarantine,
    frames: &mut Frames<'_>,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> Option<usize> {
    for _ in 0..100 {
        if let Some(released) = frames.with(|allocator| {
            quarantine.release_retired(allocator, crate::bkl::generation_is_retired)
        }) {
            return Some(released);
        }
        drop(bkl.take());
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop));
    }
    None
}

/// BKL を放して、世代 `generation` が退くのを待つ（2026-10-07）。**退いたら BKL を取り直して真を返す。**
///
/// 自分の見た世代は `acquire` が進める（`flush_if_generation_is_stale`）ので、**放す→少し待つ→取り直す→確かめる**を
/// 繰り返す。ほかの CPU は、タイマの入口で 100 Hz で BKL を通り、そのときに見た世代を進める。
fn wait_until_retired(generation: u64, bkl: &mut Option<crate::bkl::BklGuard>) -> bool {
    let started = common::arch::x86_64::read_timestamp_counter();
    loop {
        drop(bkl.take());
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop));
        if crate::bkl::generation_is_retired(generation) {
            return true;
        }
        if common::arch::x86_64::read_timestamp_counter().wrapping_sub(started) > WAIT_LIMIT_CYCLES
        {
            return false;
        }
    }
}

/// アドレス空間を壊し、下位で使っていたフレームを返す（S7-d。2026-10-07 に、2 つの道と、一杯になって待つ形にした）。
///
/// # 隔離の道の順序
///
/// (1) 世代を上げ、(2) マッピングを外しながら、外したフレームをその世代で隔離へ入れる。**先に上げてよいのは、この空間が
/// どの CPU でも稼働していないからである**——古い翻訳は、上げる前からしか作られていない。**上げた後に控えを捨てた CPU は、
/// この空間の翻訳を持たない。** 隔離が一杯になったら、BKL を放して世代が退くのを待ち（`wait_until_retired`）、
/// 退いた分を返してから続ける。**待った後に入れる分も同じ世代でよい**（もう退いているので、次に返す所で返る）。
///
/// # Safety
///
/// - **この空間がどのコアでも稼働していないこと。** 稼働中の表を壊すと、そのコアは次の翻訳で死ぬ。
/// - `direct_map` が、この空間の表を覆っていること。
/// - BKL を持っていること（`bkl` が `Some`）。**隔離の道では、一杯になったときに放して取り直す**——
///   **その間、アロケータは持たない**（[`Frames::OnLoan`]。使うときだけ借りる）。
///
/// **その場で返す道で、アロケータが借りられなければ（ほかの CPU が借りている間）、隔離の道へ倒す**——隔離は借りずに入れられ、
/// 残った分はアイドルの定常経路が返す。漏らさない。
pub unsafe fn retire_address_space(
    space: AddressSpace,
    direct_map: DirectMap,
    how: Retire,
    quarantine: &mut Quarantine,
    frames: &mut Frames<'_>,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> Retired {
    assert!(bkl.is_some(), "retire_address_space needs the BKL");
    let started = common::arch::x86_64::read_timestamp_counter();
    let mut result = Retired::ZERO;
    // **前の破棄のフレームで、退いたものを先に返す**（隔離に空きを作る。会計のために数を控える）。借りられなければ、次の機会に。
    result.released_others = frames
        .with(|allocator| quarantine.release_retired(allocator, crate::bkl::generation_is_retired))
        .unwrap_or(0);
    let mut how = how;
    if how == Retire::Directly {
        // **先に借りる。借りられなければ隔離の道へ倒す**（`space` はまだ壊していない）。
        match frames.borrow() {
            Some(mut allocator) => {
                crate::bkl::flush_this_cpu();
                // SAFETY: 呼び出し元の契約（稼働していない空間、覆っている direct map、BKL の内側）。
                unsafe {
                    space.detach(direct_map, &mut |frame| {
                        if allocator.deallocate_frame(frame).is_ok() {
                            result.returned += 1;
                        } else {
                            result.leaked += 1;
                        }
                    })
                };
                return finish(result, started);
            }
            None => how = Retire::ThroughQuarantine,
        }
    }
    if how == Retire::ThroughQuarantine {
        crate::bkl::note_mapping_changed();
        let generation = crate::bkl::tlb_generation();
        // SAFETY: 同上。**途中で BKL を放す区間は、この空間の表しか触らない**（稼働していない）。
        unsafe {
            space.detach(direct_map, &mut |frame| {
                if quarantine.push(frame, generation) {
                    result.quarantined += 1;
                    return;
                }
                // 破壊テスト (2026-10-07, quarantine-overflow-leaks-test): 一杯になっても待たず、漏らす（直す前の形）。
                if cfg!(feature = "quarantine-overflow-leaks-test") {
                    result.leaked += 1;
                    return;
                }
                result.waits += 1;
                if wait_until_retired(generation, bkl) {
                    if let Some(released) = release_retired_when_borrowable(quarantine, frames, bkl)
                    {
                        result.released_meanwhile += released;
                    }
                    if quarantine.push(frame, generation) {
                        result.quarantined += 1;
                        return;
                    }
                }
                result.leaked += 1;
            })
        };
    }
    finish(result, started)
}

/// 掛かったサイクルを埋めて返す。
fn finish(mut result: Retired, started: u64) -> Retired {
    result.cycles = common::arch::x86_64::read_timestamp_counter().wrapping_sub(started);
    result
}

/// 隔離中の 1 件。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Held {
    frame: u64,
    /// 解放した時点の世代。**この世代を全コアが追い越したら配ってよい。**
    generation: u64,
}

/// 解放したフレームの隔離。
///
/// **`retired` を外から受け取る。** 「その世代が退いたか」を判定するのは
/// [`crate::bkl::generation_is_retired`] だが、**ここはそれを知らない形にしてある**
/// ——ホスト上の単体テストで、世代の進み方を自由に作って検査するためである
/// （ハード依存と純粋ロジックの分離）。
pub struct Quarantine {
    held: [Option<Held>; QUARANTINE_CAPACITY],
    /// 隔離が溢れて捨てられなかった回数。**観測用。**
    overflow_count: u64,
}

impl Quarantine {
    pub const fn new() -> Self {
        Self {
            held: [None; QUARANTINE_CAPACITY],
            overflow_count: 0,
        }
    }

    /// 隔離へ入れる。
    ///
    /// **溢れたら `false` を返し、呼び出し側がフレームを保持し続ける**（漏らす）。
    /// **アロケータへ返してはならない**——それが閉じようとしているウィンドウそのものである。
    /// **漏らすほうが、早く配るより安全である。**
    #[must_use]
    pub fn push(&mut self, frame: PhysAddr, generation: u64) -> bool {
        let entry = Held {
            frame: frame.as_u64(),
            generation,
        };
        for slot in self.held.iter_mut() {
            if slot.is_none() {
                *slot = Some(entry);
                return true;
            }
        }
        self.overflow_count += 1;
        false
    }

    /// 退いた世代のフレームをアロケータへ返す。**返した本数を返す。**
    ///
    /// **BKL を保持したまま呼ぶこと。** 判定と再利用の間に他コアが割り込むと、
    /// 追い越しの判定が古くなる（ADR-0027 の Addendum の失効条件）。
    pub fn release_retired(
        &mut self,
        allocator: &mut FrameAllocator,
        retired: impl Fn(u64) -> bool,
    ) -> usize {
        let mut released = 0;
        for slot in self.held.iter_mut() {
            let Some(entry) = *slot else {
                continue;
            };
            if !retired(entry.generation) {
                continue;
            }
            let Some(frame) = PhysAddr::new(entry.frame) else {
                continue;
            };
            // **返せなかったら隔離に残す。** 消してしまうと、どこにも属さない
            // フレームができる。
            if allocator.deallocate_frame(frame).is_ok() {
                *slot = None;
                released += 1;
            }
        }
        released
    }

    /// 中身を空にする（B-d）。
    ///
    /// # なぜ `new()` の代入で済ませないか
    ///
    /// **静的な置き場を使い回すためである。** **`*self = Self::new()` と書くと、
    /// 512 バイトを超える値がいったんスタックに積まれうる**——**それを避ける
    /// ために静的へ移したので、代入で戻しては意味が無い。** **その場で潰す。**
    pub fn reset(&mut self) {
        for slot in self.held.iter_mut() {
            *slot = None;
        }
        self.overflow_count = 0;
    }

    /// 隔離中の本数。**観測用。**
    pub fn held_count(&self) -> usize {
        self.held.iter().filter(|slot| slot.is_some()).count()
    }

    /// 溢れて漏らした回数。**観測用。**
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count
    }
}

impl Default for Quarantine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_allocator::FrameAllocator;

    fn allocator_with(start_frame: u64, count: u64) -> FrameAllocator {
        let mut allocator = FrameAllocator::new();
        allocator
            .insert_free_range(start_frame, count)
            .expect("the test range fits");
        allocator
    }

    fn frame(index: u64) -> PhysAddr {
        PhysAddr::new(index * 4096).expect("aligned test frame")
    }

    #[test]
    fn a_frame_is_not_returned_while_its_generation_is_still_live() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        assert!(quarantine.push(frame(2000), 7));

        let released = quarantine.release_retired(&mut allocator, |_| false);

        assert_eq!(released, 0);
        assert_eq!(quarantine.held_count(), 1);
        assert_eq!(allocator.free_frame_count(), 1);
    }

    #[test]
    fn a_frame_is_returned_once_every_core_has_passed_its_generation() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        assert!(quarantine.push(frame(2000), 7));

        let released = quarantine.release_retired(&mut allocator, |generation| generation < 8);

        assert_eq!(released, 1);
        assert_eq!(quarantine.held_count(), 0);
        assert_eq!(allocator.free_frame_count(), 2);
    }

    #[test]
    fn only_the_retired_generations_are_returned() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        assert!(quarantine.push(frame(2000), 3));
        assert!(quarantine.push(frame(2001), 9));

        // 世代 8 まで退いた状態。**3 は退き、9 はまだである。**
        let released = quarantine.release_retired(&mut allocator, |generation| generation < 8);

        assert_eq!(released, 1);
        assert_eq!(quarantine.held_count(), 1);
        assert_eq!(allocator.free_frame_count(), 2);
    }

    #[test]
    fn overflowing_the_quarantine_leaks_instead_of_handing_the_frame_back() {
        let mut quarantine = Quarantine::new();
        for index in 0..QUARANTINE_CAPACITY {
            assert!(quarantine.push(frame(3000 + index as u64), 1));
        }

        // **溢れた 1 本は `false` になる。** 呼び出し側が保持し続ける（漏らす）。
        assert!(!quarantine.push(frame(9999), 1));
        assert_eq!(quarantine.held_count(), QUARANTINE_CAPACITY);
        assert_eq!(quarantine.overflow_count(), 1);
    }

    #[test]
    fn a_released_slot_can_be_reused_by_a_later_free() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        for index in 0..QUARANTINE_CAPACITY {
            assert!(quarantine.push(frame(3000 + index as u64), 1));
        }
        assert_eq!(
            quarantine.release_retired(&mut allocator, |_| true),
            QUARANTINE_CAPACITY
        );

        assert!(quarantine.push(frame(9999), 2));
        assert_eq!(quarantine.held_count(), 1);
    }
}
