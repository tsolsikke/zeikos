//! プロセスの写像の表（2026-10-06）——`mmap` が配った範囲を、番地と種類で覚える。
//!
//! # なぜ表が要るのか
//!
//! **以前は、次に配る番地を 1 つ持つだけだった**（`Heap` の `mmap_next`。上へ進むだけで、返した範囲を覚えない）。
//! 覚えていないと、`munmap` で返すことも、重なりを見ることも、空いた所を使い直すこともできない。Linux のプログラム
//! （musl の `malloc`）は、起動の最初に無名の `mmap` を数回打ち、要らなくなった範囲を `munmap` で返す。
//!
//! # 置き場
//!
//! **プロセスの記録（`UserProcess`）や `Heap` には置かない**——あちらは載せる側の遠征スタックの上を通り、64 欄の表
//! （2 KiB）を足すと、子が走っている間ずっと残る枠が太る（`ADR-0079`）。**スロットと、遠征の深さごとの静的な置き場に
//! 置く**（`crate::process_state` と同じ形）。
//!
//! # 入っているもの
//!
//! 無名の写像を配る（first-fit。空いた所を使い直す）、写像の全体や一部を返す（分ける）、`MAP_FIXED` のために重なる
//! 写像を外す、共有メモリと画面の写像を同じ表で覚える、像・スタック・見張りのページ・ヒープも表に載せる（2026-10-06。
//! 2 つの刻みで入れた）。**`mprotect` は、まだ無い**（次の刻み）。

use common::critical::Locked;

use crate::arch::x86_64::{MAX_EXCURSION_DEPTH, USER_TASK_SLOTS};

/// 1 つのプロセスが同時に持てる写像の数。musl の起動は 10 個ほど、Seinas の輪は数十個と見ている。
pub const MAX_MAPPINGS: usize = 64;

/// ページの大きさ。
pub const PAGE_SIZE: u64 = 4096;

/// 長さをページの境界へ切り上げる（2026-10-08）。**切り上げが 2^64 を越えるなら `None`。**
///
/// 利用者が渡す長さ（`munmap`・`mprotect`）は任意の値である。あふれを見ずに `len.div_ceil(PAGE_SIZE) * PAGE_SIZE` とすると、
/// 長さが 2^64 - 4096 を越えたとき乗算があふれ、検査のビルド（overflow-checks が有効）ではカーネルが panic で止まっていた。
pub const fn page_rounded(len: u64) -> Option<u64> {
    match len.checked_add(PAGE_SIZE - 1) {
        Some(sum) => Some(sum & !(PAGE_SIZE - 1)),
        None => None,
    }
}

/// 写像の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingKind {
    /// 無名（`MAP_ANONYMOUS`）。フレームはこのプロセスのもので、返すときはアロケータへ戻す。
    Anonymous,
    /// 共有メモリ（`memfd_create` の fd）。フレームは `crate::shm` が参照数で持つ。
    SharedMemory { shm: u8 },
    /// 画面の裏バッファ。フレームはカーネルのもの。
    Screen,
    /// 載せた像（`PT_LOAD` の区画の全部をまとめた範囲）。**外せない。**
    Image,
    /// スタック。**外せない。**
    Stack,
    /// スタックの下の見張りのページ（写していない）。**外せないし、ここへは置けない。**
    Guard,
    /// `brk` のヒープ。終わりは `brk` が動かす。**`munmap` では外せないが、`MAP_FIXED` は上に置ける**（musl の見張り）。
    Heap,
}

impl MappingKind {
    /// `munmap` で外せる種類か。
    pub const fn can_unmap(self) -> bool {
        matches!(self, MappingKind::Anonymous)
    }

    /// `MAP_FIXED` が上に置き換えてよい種類か（外して置く）。
    pub const fn can_be_replaced(self) -> bool {
        matches!(self, MappingKind::Anonymous | MappingKind::Heap)
    }

    /// `mprotect` で権限を変えてよい種類か。共有メモリと画面（フレームが自分のものでない）と、見張りのページは変えない。
    pub const fn can_change_protection(self) -> bool {
        matches!(
            self,
            MappingKind::Anonymous | MappingKind::Heap | MappingKind::Image | MappingKind::Stack
        )
    }
}

/// 1 つの写像。`end` は排他である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub kind: MappingKind,
    /// 書ける写像か（`PROT_WRITE`）。
    pub writable: bool,
    /// ページが写してあるか。**`PROT_NONE` の無名の写像は、範囲だけ取って、何も写さない**（触ればページフォルト）。
    pub present: bool,
}

impl Mapping {
    /// ページの数。
    pub const fn pages(&self) -> u64 {
        (self.end - self.start) / PAGE_SIZE
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        start < self.end && self.start < end
    }
}

/// 断る理由（呼ぶ側が errno に写す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// このプロセスの表が据えられていない（起動時の試しなど）。`-ENOMEM`。
    NotActive,
    /// 配る範囲が、`mmap` の始まりから終わりまでの中に無い。`-ENOMEM`。
    NoRoom,
    /// 表が一杯である。`-ENOMEM`。
    TableFull,
    /// 長さが 0 か、ページの境界に無い番地。`-EINVAL`。
    BadRange,
    /// 範囲が、写像の一部にだけ掛かる（写像の全体だけを求める口で）。`-EINVAL`。
    PartOfAMapping,
    /// 範囲に、外せない種類の写像（像・スタック・見張り・共有メモリ・画面。`munmap` ではヒープも）が掛かる。`-EINVAL`。
    NotRemovable,
    /// 置きたい範囲に、写像が在る（`MAP_FIXED_NOREPLACE`。`-EEXIST`）、または登録の範囲が重なる。
    Overlap,
    /// 範囲の全部が写像で覆われていない（`mprotect`。Linux は `-ENOMEM`）。
    NotMapped,
}

/// [`MemoryMap::release_range`] が返す、外した断片の入れ物。
pub type Released = [Option<Mapping>; MAX_MAPPINGS];

/// 1 つのプロセスの写像の表。
pub struct MemoryMap {
    active: bool,
    base: u64,
    limit: u64,
    /// この表が最初に配った番地（`reserve` の 1 回目。2026-10-06）。**外されても変わらない**——`user-mmap:` の行の
    /// 「最初の `mmap`」は、残っている写像のいちばん低い始まりではなく、カーネルが最初に選んだ番地である
    /// （Rust の `std` は、最初に取った代替スタックを終わりに外す）。
    first_reserved: Option<u64>,
    entries: [Option<Mapping>; MAX_MAPPINGS],
}

impl MemoryMap {
    /// 据えられていない形（配らない）。
    pub const INACTIVE: Self = Self {
        active: false,
        base: 0,
        limit: 0,
        first_reserved: None,
        entries: [None; MAX_MAPPINGS],
    };

    /// プロセスが始まる前に、配置の値で据える。表は空になる。
    pub fn activate(&mut self, base: u64, limit: u64) {
        *self = Self {
            active: true,
            base,
            limit,
            first_reserved: None,
            entries: [None; MAX_MAPPINGS],
        };
    }

    /// 据えられているか。
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// `bytes`（ページの倍数）の範囲を、`mmap` の始まりから上へ、最初に収まる所に取る（first-fit。純粋な論理）。
    /// 返すのは始まりの番地。
    pub fn reserve(
        &mut self,
        bytes: u64,
        kind: MappingKind,
        writable: bool,
        present: bool,
    ) -> Result<u64, MapError> {
        if !self.active {
            return Err(MapError::NotActive);
        }
        if bytes == 0 || !bytes.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::BadRange);
        }
        let Some(slot) = self.entries.iter().position(Option::is_none) else {
            return Err(MapError::TableFull);
        };
        // **候補を、重なる写像の終わりへ進める。** 表の数より多くは進まない（候補が動くたびに、重なる写像が
        // 1 つ減る）。
        let mut candidate = self.base;
        for _ in 0..=MAX_MAPPINGS {
            let end = candidate.checked_add(bytes).ok_or(MapError::NoRoom)?;
            if end > self.limit {
                return Err(MapError::NoRoom);
            }
            match self
                .entries
                .iter()
                .flatten()
                .filter(|mapping| mapping.overlaps(candidate, end))
                .map(|mapping| mapping.end)
                .max()
            {
                Some(next) => candidate = next,
                None => {
                    self.entries[slot] = Some(Mapping {
                        start: candidate,
                        end,
                        kind,
                        writable,
                        present,
                    });
                    if self.first_reserved.is_none() {
                        self.first_reserved = Some(candidate);
                    }
                    return Ok(candidate);
                }
            }
        }
        Err(MapError::NoRoom)
    }

    /// `start` から `bytes` の範囲を、写像の全体として返す（純粋な論理）。
    ///
    /// - 範囲がちょうど 1 つの写像なら、その写像を外して返す。
    /// - 範囲にどの写像も掛かっていなければ `Ok(None)`（Linux の `munmap` は、写していない範囲でも 0 を返す）。
    /// - 範囲が写像の一部にだけ掛かる、または複数の写像にまたがるなら、`PartOfAMapping`（この刻みでは断る）。
    pub fn release_whole(&mut self, start: u64, bytes: u64) -> Result<Option<Mapping>, MapError> {
        if !self.active {
            return Err(MapError::NotActive);
        }
        if bytes == 0 || !start.is_multiple_of(PAGE_SIZE) || !bytes.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::BadRange);
        }
        let end = start.checked_add(bytes).ok_or(MapError::BadRange)?;
        let mut touched = None;
        for (index, mapping) in self.entries.iter().enumerate() {
            let Some(mapping) = mapping else {
                continue;
            };
            if !mapping.overlaps(start, end) {
                continue;
            }
            if touched.is_some() || mapping.start != start || mapping.end != end {
                return Err(MapError::PartOfAMapping);
            }
            touched = Some(index);
        }
        Ok(touched.and_then(|index| self.entries[index].take()))
    }

    /// 決まった番地の範囲を、表に登録する（像・スタック・見張り・ヒープ。載せる側が呼ぶ。純粋な論理）。重なれば断る。
    pub fn register(
        &mut self,
        start: u64,
        end: u64,
        kind: MappingKind,
        writable: bool,
        present: bool,
    ) -> Result<(), MapError> {
        if !self.active {
            return Err(MapError::NotActive);
        }
        if start >= end || !start.is_multiple_of(PAGE_SIZE) || !end.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::BadRange);
        }
        // 破壊テスト (2026-10-06, mappings-overlap-skip-test): 置くときに重なりを見ない（`overlaps_any` も）。
        // `MAP_FIXED_NOREPLACE` が、写像の上でも断らずに置きに行く。
        if !cfg!(feature = "mappings-overlap-skip-test")
            && self
                .entries
                .iter()
                .flatten()
                .any(|mapping| mapping.overlaps(start, end))
        {
            return Err(MapError::Overlap);
        }
        let Some(slot) = self.entries.iter().position(Option::is_none) else {
            return Err(MapError::TableFull);
        };
        self.entries[slot] = Some(Mapping {
            start,
            end,
            kind,
            writable,
            present,
        });
        Ok(())
    }

    /// `start` から `bytes` の範囲に掛かる写像を外し、外した断片を `out` へ置く（純粋な論理。表だけを変える。
    /// ページを外すのは呼ぶ側である）。**範囲の一部に掛かる写像は、残る部分を分けて表に戻す。**
    ///
    /// `may_remove` が偽を返す種類が範囲に掛かっていれば、何も変えずに `NotRemovable`。分けるのに欄が足りなければ、
    /// 何も変えずに `TableFull`。返すのは外した断片の数（範囲に写像が無ければ 0）。
    pub fn release_range(
        &mut self,
        start: u64,
        bytes: u64,
        may_remove: impl Fn(MappingKind) -> bool,
        out: &mut Released,
    ) -> Result<usize, MapError> {
        if !self.active {
            return Err(MapError::NotActive);
        }
        if bytes == 0 || !start.is_multiple_of(PAGE_SIZE) || !bytes.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::BadRange);
        }
        let end = start.checked_add(bytes).ok_or(MapError::BadRange)?;
        // **1 度目: 変えずに確かめる。** 外せない種類が掛かっていないか、分けた後の欄が足りるか。
        let mut extra_slots_needed = 0usize;
        for mapping in self.entries.iter().flatten() {
            if !mapping.overlaps(start, end) {
                continue;
            }
            if !may_remove(mapping.kind) {
                return Err(MapError::NotRemovable);
            }
            // 前にも後ろにも残るなら、欄が 1 つ増える（元の欄に前を、新しい欄に後ろを置く）。
            if mapping.start < start && end < mapping.end {
                extra_slots_needed += 1;
            }
        }
        let free_slots = self.entries.iter().filter(|slot| slot.is_none()).count();
        if extra_slots_needed > free_slots {
            return Err(MapError::TableFull);
        }
        // **2 度目: 外して、残る部分を戻す。**
        let mut released = 0usize;
        for index in 0..MAX_MAPPINGS {
            let Some(mapping) = self.entries[index] else {
                continue;
            };
            if !mapping.overlaps(start, end) {
                continue;
            }
            let piece_start = mapping.start.max(start);
            let piece_end = mapping.end.min(end);
            let piece = Mapping {
                start: piece_start,
                end: piece_end,
                ..mapping
            };
            let before = (mapping.start < start).then_some(Mapping {
                end: start,
                ..mapping
            });
            // 破壊テスト (2026-10-06, mappings-split-swap-test): 前に残る部分が在れば、外す断片と取り違える——
            // 求められた範囲を表に残し、前に残るはずだった部分を外す。
            let (piece, before) = match before {
                Some(kept) if cfg!(feature = "mappings-split-swap-test") => (kept, Some(piece)),
                _ => (piece, before),
            };
            out[released] = Some(piece);
            released += 1;
            let after = (end < mapping.end).then_some(Mapping {
                start: end,
                ..mapping
            });
            self.entries[index] = before.or(after);
            if before.is_some() && after.is_some() {
                let slot = self
                    .entries
                    .iter()
                    .position(Option::is_none)
                    .expect("the free slots were counted above");
                self.entries[slot] = after;
            }
        }
        Ok(released)
    }

    /// `start` から `bytes` の範囲に掛かる写像の、書けるか・写してあるかを変える（純粋な論理。表だけを変える。葉を
    /// 書き換えるのは呼ぶ側である）。**範囲の一部に掛かる写像は、範囲の中と外で分ける。** 変える前の断片（古い
    /// `writable`・`present` を持つ）を `out` へ置き、数を返す。
    ///
    /// 範囲の全部が写像で覆われていなければ `NotMapped`（Linux の `mprotect` は `-ENOMEM`）。
    /// `can_change_protection` が偽の種類が掛かっていれば `NotRemovable`。欄が足りなければ `TableFull`。どの失敗でも、表は
    /// 変えない。
    pub fn change_protection(
        &mut self,
        start: u64,
        bytes: u64,
        writable: bool,
        present: bool,
        out: &mut Released,
    ) -> Result<usize, MapError> {
        if !self.active {
            return Err(MapError::NotActive);
        }
        if bytes == 0 || !start.is_multiple_of(PAGE_SIZE) || !bytes.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::BadRange);
        }
        let end = start.checked_add(bytes).ok_or(MapError::BadRange)?;
        // **1 度目: 変えずに確かめる。** 覆われていること、種類、欄の数。
        let mut covered = 0u64;
        let mut extra_slots_needed = 0usize;
        for mapping in self.entries.iter().flatten() {
            if !mapping.overlaps(start, end) {
                continue;
            }
            if !mapping.kind.can_change_protection() {
                return Err(MapError::NotRemovable);
            }
            covered += mapping.end.min(end) - mapping.start.max(start);
            extra_slots_needed +=
                usize::from(mapping.start < start) + usize::from(end < mapping.end);
        }
        if covered != bytes {
            return Err(MapError::NotMapped);
        }
        let free_slots = self.entries.iter().filter(|slot| slot.is_none()).count();
        if extra_slots_needed > free_slots {
            return Err(MapError::TableFull);
        }
        // **2 度目: 分けて、範囲の中の断片の権限を変える。**
        let mut changed = 0usize;
        for index in 0..MAX_MAPPINGS {
            let Some(mapping) = self.entries[index] else {
                continue;
            };
            if !mapping.overlaps(start, end) {
                continue;
            }
            let piece = Mapping {
                start: mapping.start.max(start),
                end: mapping.end.min(end),
                ..mapping
            };
            out[changed] = Some(piece);
            changed += 1;
            self.entries[index] = Some(Mapping {
                writable,
                present,
                ..piece
            });
            for remainder in [
                (mapping.start < start).then_some(Mapping {
                    end: start,
                    ..mapping
                }),
                (end < mapping.end).then_some(Mapping {
                    start: end,
                    ..mapping
                }),
            ]
            .into_iter()
            .flatten()
            {
                let slot = self
                    .entries
                    .iter()
                    .position(Option::is_none)
                    .expect("the free slots were counted above");
                self.entries[slot] = Some(remainder);
            }
        }
        Ok(changed)
    }

    /// `start` から `bytes` の範囲に、写像が 1 つでも掛かっているか。
    pub fn overlaps_any(&self, start: u64, bytes: u64) -> bool {
        // 破壊テスト (2026-10-06, mappings-overlap-skip-test): `register` と同じく、重なりを見ない。
        !cfg!(feature = "mappings-overlap-skip-test")
            && start.checked_add(bytes).is_some_and(|end| {
                self.entries
                    .iter()
                    .flatten()
                    .any(|m| m.overlaps(start, end))
            })
    }

    /// `brk` のヒープの終わりを動かす（純粋な論理）。
    ///
    /// ヒープの欄（[`MappingKind::Heap`]）のうち、いちばん高い終わりを `end` へ動かす。伸ばす先にほかの写像が在れば
    /// `Overlap`（`brk` は `-ENOMEM` にする）。縮めて欄が空になれば消す。欄が無ければ `[start, end)` を作る（`start == end`
    /// なら作らない）。**ヒープの途中が `MAP_FIXED` で置き換えられていても動く**——残った断片のうち、いちばん高いものを
    /// 伸び縮みさせる。
    pub fn set_heap_end(&mut self, start: u64, end: u64) -> Result<(), MapError> {
        if !self.active {
            return Err(MapError::NotActive);
        }
        if !start.is_multiple_of(PAGE_SIZE) || !end.is_multiple_of(PAGE_SIZE) || end < start {
            return Err(MapError::BadRange);
        }
        let highest = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, m)| {
                m.filter(|m| m.kind == MappingKind::Heap)
                    .map(|m| (index, m))
            })
            .max_by_key(|(_, m)| m.end);
        match highest {
            Some((index, piece)) => {
                if end > piece.end {
                    // 伸ばす先に、ほかの写像が無いこと。
                    if self
                        .entries
                        .iter()
                        .enumerate()
                        .any(|(i, m)| i != index && m.is_some_and(|m| m.overlaps(piece.end, end)))
                    {
                        return Err(MapError::Overlap);
                    }
                    self.entries[index] = Some(Mapping { end, ..piece });
                } else if end <= piece.start {
                    self.entries[index] = None;
                    // さらに下の断片が在れば、そちらも縮める。
                    if end < piece.start {
                        return self.set_heap_end(start, end);
                    }
                } else {
                    self.entries[index] = Some(Mapping { end, ..piece });
                }
                Ok(())
            }
            None if end == start => Ok(()),
            None => self.register(start, end, MappingKind::Heap, true, true),
        }
    }

    /// `address` を含む写像。
    pub fn find(&self, address: u64) -> Option<Mapping> {
        self.entries
            .iter()
            .flatten()
            .find(|mapping| mapping.start <= address && address < mapping.end)
            .copied()
    }

    /// `mmap` が配った写像か（無名・共有メモリ・画面。像・スタック・見張り・ヒープは、載せる側と `brk` のもの）。
    pub const fn is_mmapped(kind: MappingKind) -> bool {
        matches!(
            kind,
            MappingKind::Anonymous | MappingKind::SharedMemory { .. } | MappingKind::Screen
        )
    }

    /// 要約——**`mmap` が配った写像**の数、最初に配った番地、いちばん高い終わり、無名のページの数。**判定の行に出す**
    /// （像・スタック・ヒープは数えない。`user-mmap:` の行は `mmap` の番地を言う）。
    ///
    /// **最初に配った番地は、カーネルが選んだ 1 回目の `reserve` の番地で、外されても変わらない**（2026-10-06）。
    /// **いちばん高い終わりは、`mmap` の区画（基点から上）に在る写像だけで数える**——`MAP_FIXED` で区画の外に置いたもの
    /// （musl の `malloc` が `brk` の先頭に置く見張り）は、数には入るが番地には出ない。行の「どのプロセスも基点から
    /// 配り始める」は、カーネルが選んだ番地の話である。
    pub fn summary(&self) -> MapSummary {
        let mut summary = MapSummary {
            count: 0,
            first: self.first_reserved,
            highest: None,
            anonymous_pages: 0,
        };
        for mapping in self.entries.iter().flatten() {
            if !Self::is_mmapped(mapping.kind) {
                continue;
            }
            summary.count += 1;
            if mapping.kind == MappingKind::Anonymous {
                summary.anonymous_pages += mapping.pages();
            }
            if mapping.start < self.base {
                continue;
            }
            summary.highest = Some(
                summary
                    .highest
                    .map_or(mapping.end, |high| high.max(mapping.end)),
            );
        }
        summary
    }
}

/// [`MemoryMap::summary`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapSummary {
    pub count: usize,
    pub first: Option<u64>,
    pub highest: Option<u64>,
    pub anonymous_pages: u64,
}

/// スロットと、遠征の深さごとの置き場（`crate::process_state` と同じ添字の付け方）。
static MAPS: [[Locked<MemoryMap>; MAX_EXCURSION_DEPTH]; USER_TASK_SLOTS] =
    [const { [const { Locked::new(MemoryMap::INACTIVE) }; MAX_EXCURSION_DEPTH] }; USER_TASK_SLOTS];

#[inline(never)]
#[cold]
fn report_depth_out_of_range(depth: usize) -> ! {
    panic!(
        "mappings: the excursion depth index {depth} is out of range (MAX_EXCURSION_DEPTH = \
         {MAX_EXCURSION_DEPTH}); the memory map would be read from the wrong slot"
    );
}

/// これから走らせるプロセスの表を、配置の値で据える。**載せる側が、Ring 3 へ落ちる前に呼ぶ**（`crate::userland`）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - 今のスロットの、今の遠征の深さの欄を書く。ほかは何も変えない。
pub fn activate_for_next_process(base: u64, limit: u64) {
    with_loaded(|map| map.activate(base, limit));
}

/// 載せる側から、これから走らせる（または走り終えた）プロセスの表へ触る（添字は今の遠征の深さ）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - 今のスロットの、今の遠征の深さの欄へ触る。
pub fn with_loaded<R>(body: impl FnOnce(&mut MemoryMap) -> R) -> R {
    let depth = crate::arch::x86_64::excursion_depth();
    if depth >= MAX_EXCURSION_DEPTH {
        report_depth_out_of_range(depth);
    }
    body(&mut MAPS[crate::arch::x86_64::current_excursion_slot()][depth].lock())
}

/// 今走っているプロセス（システムコールを打った側）の表へ触る。**遠征の中から呼ぶ**（`crate::syscall`）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - 今のスロットの、今の遠征の深さから 1 を引いた欄へ触る。遠征の外（深さ 0）から呼ぶと止まる。
pub fn with_current<R>(body: impl FnOnce(&mut MemoryMap) -> R) -> R {
    let depth = crate::arch::x86_64::excursion_depth();
    let Some(index) = depth
        .checked_sub(1)
        .filter(|index| *index < MAX_EXCURSION_DEPTH)
    else {
        report_depth_out_of_range(depth);
    };
    body(&mut MAPS[crate::arch::x86_64::current_excursion_slot()][index].lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 長さの切り上げは、2^64 を越える長さで `None` を返し、あふれない（2026-10-08）。
    #[test]
    fn page_rounding_refuses_lengths_that_would_wrap() {
        assert_eq!(page_rounded(0), Some(0));
        assert_eq!(page_rounded(1), Some(PAGE_SIZE));
        assert_eq!(page_rounded(PAGE_SIZE), Some(PAGE_SIZE));
        assert_eq!(page_rounded(PAGE_SIZE + 1), Some(2 * PAGE_SIZE));
        assert_eq!(
            page_rounded(u64::MAX - (PAGE_SIZE - 1)),
            Some(u64::MAX - (PAGE_SIZE - 1))
        );
        assert_eq!(page_rounded(u64::MAX - (PAGE_SIZE - 2)), None);
        assert_eq!(page_rounded(u64::MAX), None);
    }

    const BASE: u64 = 0x1000_0000;
    const LIMIT: u64 = 0x1001_0000;

    fn active() -> MemoryMap {
        let mut map = MemoryMap::INACTIVE;
        map.activate(BASE, LIMIT);
        map
    }

    /// 据えていない表は配らない。据えると、始まりから順に配り、終わりで断る。
    #[test]
    fn an_inactive_map_hands_out_nothing_and_an_active_one_fills_from_the_base() {
        let mut map = MemoryMap::INACTIVE;
        assert_eq!(
            map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true),
            Err(MapError::NotActive)
        );
        let mut map = active();
        assert_eq!(
            map.reserve(2 * PAGE_SIZE, MappingKind::Anonymous, true, true),
            Ok(BASE)
        );
        assert_eq!(
            map.reserve(PAGE_SIZE, MappingKind::Screen, true, true),
            Ok(BASE + 2 * PAGE_SIZE)
        );
        // 残りの全部。
        assert_eq!(
            map.reserve(
                LIMIT - BASE - 3 * PAGE_SIZE,
                MappingKind::Anonymous,
                false,
                true
            ),
            Ok(BASE + 3 * PAGE_SIZE)
        );
        assert_eq!(
            map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true),
            Err(MapError::NoRoom)
        );
        assert_eq!(
            map.reserve(0, MappingKind::Anonymous, true, true),
            Err(MapError::BadRange)
        );
        assert_eq!(
            map.reserve(100, MappingKind::Anonymous, true, true),
            Err(MapError::BadRange)
        );
    }

    /// 返した所は使い直す（first-fit）。**空きが足りない穴は飛ばす。**
    #[test]
    fn a_released_range_is_used_again_when_it_fits() {
        let mut map = active();
        let first = map
            .reserve(PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        let second = map
            .reserve(2 * PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        let third = map
            .reserve(PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        assert_eq!(
            (first, second, third),
            (BASE, BASE + PAGE_SIZE, BASE + 3 * PAGE_SIZE)
        );
        let released = map.release_whole(second, 2 * PAGE_SIZE).unwrap().unwrap();
        assert_eq!(released.kind, MappingKind::Anonymous);
        assert_eq!(released.pages(), 2);
        // 3 ページは穴（2 ページ）に収まらないので、その先へ。1 ページは穴に入る。
        assert_eq!(
            map.reserve(3 * PAGE_SIZE, MappingKind::Anonymous, true, true),
            Ok(BASE + 4 * PAGE_SIZE)
        );
        assert_eq!(
            map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true),
            Ok(BASE + PAGE_SIZE)
        );
    }

    /// 返すのは写像の全体だけ。一部や、またがる範囲は断る。写していない範囲は `None` で 0 になる。
    #[test]
    fn only_a_whole_mapping_is_released_and_an_empty_range_is_not_an_error() {
        let mut map = active();
        let start = map
            .reserve(4 * PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        assert_eq!(
            map.release_whole(start, PAGE_SIZE),
            Err(MapError::PartOfAMapping)
        );
        assert_eq!(
            map.release_whole(start + PAGE_SIZE, 3 * PAGE_SIZE),
            Err(MapError::PartOfAMapping)
        );
        assert_eq!(
            map.release_whole(start, 5 * PAGE_SIZE),
            Err(MapError::PartOfAMapping)
        );
        assert_eq!(
            map.release_whole(start + 8 * PAGE_SIZE, PAGE_SIZE),
            Ok(None)
        );
        assert_eq!(
            map.release_whole(start + 1, PAGE_SIZE),
            Err(MapError::BadRange)
        );
        assert_eq!(map.release_whole(start, 0), Err(MapError::BadRange));
        let whole = map.release_whole(start, 4 * PAGE_SIZE).unwrap().unwrap();
        assert_eq!((whole.start, whole.end), (start, start + 4 * PAGE_SIZE));
        assert_eq!(map.find(start), None);
    }

    /// 範囲の一部を外すと、残りが分かれて表に戻る。外せない種類が掛かっていれば、何も変えない。
    #[test]
    fn releasing_part_of_a_mapping_splits_what_remains() {
        let mut map = active();
        let start = map
            .reserve(4 * PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        let mut out: Released = [None; MAX_MAPPINGS];
        // 真ん中の 2 ページを外す → 前 1 ページと後ろ 1 ページが残る。
        let n = map
            .release_range(
                start + PAGE_SIZE,
                2 * PAGE_SIZE,
                MappingKind::can_unmap,
                &mut out,
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            out[0].map(|m| (m.start, m.end)),
            Some((start + PAGE_SIZE, start + 3 * PAGE_SIZE))
        );
        assert_eq!(map.find(start).map(|m| m.end), Some(start + PAGE_SIZE));
        assert_eq!(map.find(start + PAGE_SIZE), None);
        assert_eq!(
            map.find(start + 3 * PAGE_SIZE).map(|m| m.start),
            Some(start + 3 * PAGE_SIZE)
        );
        assert_eq!(map.summary().count, 2);
        // 2 つにまたがる範囲を外すと、2 つの断片が返る。
        let n = map
            .release_range(start, 4 * PAGE_SIZE, MappingKind::can_unmap, &mut out)
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(map.summary().count, 0);
        // 外せない種類が掛かっていれば、何も変えない。
        map.register(BASE, BASE + PAGE_SIZE, MappingKind::Stack, true, true)
            .unwrap();
        map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        assert_eq!(
            map.release_range(BASE, 2 * PAGE_SIZE, MappingKind::can_unmap, &mut out),
            Err(MapError::NotRemovable)
        );
        // 要約が数えるのは `mmap` の写像だけ（スタックは入らない）。
        assert_eq!(map.summary().count, 1);
        assert_eq!(map.find(BASE).map(|m| m.kind), Some(MappingKind::Stack));
        // 写像の無い範囲は 0。
        assert_eq!(
            map.release_range(
                BASE + 8 * PAGE_SIZE,
                PAGE_SIZE,
                MappingKind::can_unmap,
                &mut out
            ),
            Ok(0)
        );
    }

    /// `MAP_FIXED` が置き換えてよいのは、無名とヒープだけ。登録は重なりを断る。
    #[test]
    fn registration_refuses_overlaps_and_replacement_is_limited_to_anonymous_and_heap() {
        let mut map = active();
        map.register(BASE, BASE + 2 * PAGE_SIZE, MappingKind::Image, false, true)
            .unwrap();
        assert_eq!(
            map.register(
                BASE + PAGE_SIZE,
                BASE + 3 * PAGE_SIZE,
                MappingKind::Stack,
                true,
                true
            ),
            Err(MapError::Overlap)
        );
        assert_eq!(
            map.register(BASE, BASE, MappingKind::Heap, true, true),
            Err(MapError::BadRange)
        );
        assert!(map.overlaps_any(BASE + PAGE_SIZE, PAGE_SIZE));
        assert!(!map.overlaps_any(BASE + 2 * PAGE_SIZE, PAGE_SIZE));
        assert!(MappingKind::Anonymous.can_be_replaced() && MappingKind::Heap.can_be_replaced());
        assert!(!MappingKind::Image.can_be_replaced() && !MappingKind::Guard.can_be_replaced());
        assert!(!MappingKind::Heap.can_unmap() && MappingKind::Anonymous.can_unmap());
    }

    /// `mprotect`: 範囲の中の断片の権限が変わり、外の断片は元のまま。覆われていなければ変えない。変えられない種類も。
    #[test]
    fn changing_protection_splits_and_keeps_the_old_flags_for_the_caller() {
        let mut map = active();
        let start = map
            .reserve(4 * PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        let mut out: Released = [None; MAX_MAPPINGS];
        // 真ん中の 2 ページを読むだけにする。
        let n = map
            .change_protection(start + PAGE_SIZE, 2 * PAGE_SIZE, false, true, &mut out)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            out[0].map(|m| (m.start, m.end, m.writable)),
            Some((start + PAGE_SIZE, start + 3 * PAGE_SIZE, true))
        );
        assert_eq!(map.find(start).map(|m| m.writable), Some(true));
        assert_eq!(
            map.find(start + PAGE_SIZE).map(|m| (m.writable, m.end)),
            Some((false, start + 3 * PAGE_SIZE))
        );
        assert_eq!(
            map.find(start + 3 * PAGE_SIZE).map(|m| m.writable),
            Some(true)
        );
        assert_eq!(map.summary().count, 3);
        // 全部を写さない形（PROT_NONE）にすると、3 つの断片が返り、どれも present が偽になる。
        let n = map
            .change_protection(start, 4 * PAGE_SIZE, false, false, &mut out)
            .unwrap();
        assert_eq!(n, 3);
        assert!(out.iter().take(3).flatten().all(|m| m.present));
        assert!((0..4).all(|i| map.find(start + i * PAGE_SIZE).is_some_and(|m| !m.present)));
        // 覆われていない範囲は NotMapped で、何も変えない。
        assert_eq!(
            map.change_protection(start + 3 * PAGE_SIZE, 2 * PAGE_SIZE, true, true, &mut out),
            Err(MapError::NotMapped)
        );
        // 見張りのページは変えられない。
        map.register(
            BASE + 0x100000,
            BASE + 0x101000,
            MappingKind::Guard,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            map.change_protection(BASE + 0x100000, PAGE_SIZE, true, true, &mut out),
            Err(MapError::NotRemovable)
        );
    }

    /// `brk` のヒープの終わりは、伸ばす・縮める・消す・作り直す。途中が置き換えられていても、いちばん高い断片が動く。
    #[test]
    fn the_heap_end_moves_and_survives_a_hole_in_the_middle() {
        let mut map = active();
        let heap = BASE + 0x8000;
        assert_eq!(map.set_heap_end(heap, heap), Ok(()));
        assert_eq!(map.summary().count, 0);
        assert_eq!(map.set_heap_end(heap, heap + 2 * PAGE_SIZE), Ok(()));
        assert_eq!(map.find(heap).map(|m| m.kind), Some(MappingKind::Heap));
        // musl の見張り: ヒープの先頭の 1 ページを、無名の写像で置き換える。
        let mut out: Released = [None; MAX_MAPPINGS];
        assert_eq!(
            map.release_range(heap, PAGE_SIZE, MappingKind::can_be_replaced, &mut out),
            Ok(1)
        );
        map.register(heap, heap + PAGE_SIZE, MappingKind::Anonymous, false, false)
            .unwrap();
        // ヒープを伸ばすと、残った断片が伸びる。
        assert_eq!(map.set_heap_end(heap, heap + 4 * PAGE_SIZE), Ok(()));
        assert_eq!(
            map.find(heap + 3 * PAGE_SIZE).map(|m| m.kind),
            Some(MappingKind::Heap)
        );
        // 伸ばす先に写像が在れば断る。
        map.register(
            heap + 4 * PAGE_SIZE,
            heap + 5 * PAGE_SIZE,
            MappingKind::Anonymous,
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            map.set_heap_end(heap, heap + 6 * PAGE_SIZE),
            Err(MapError::Overlap)
        );
        // 断片より下へ縮めると、断片は消える。
        assert_eq!(map.set_heap_end(heap, heap + PAGE_SIZE), Ok(()));
        assert_eq!(map.find(heap + 2 * PAGE_SIZE), None);
        assert_eq!(map.find(heap).map(|m| m.kind), Some(MappingKind::Anonymous));
    }

    /// 表が一杯なら断る。要約は、数と両端と無名のページ数を言う。
    #[test]
    fn a_full_table_refuses_and_the_summary_counts_what_is_there() {
        let mut map = MemoryMap::INACTIVE;
        map.activate(BASE, BASE + (MAX_MAPPINGS as u64 + 1) * PAGE_SIZE);
        for _ in 0..MAX_MAPPINGS {
            map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true)
                .unwrap();
        }
        assert_eq!(
            map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true),
            Err(MapError::TableFull)
        );
        let summary = map.summary();
        assert_eq!(summary.count, MAX_MAPPINGS);
        assert_eq!(summary.first, Some(BASE));
        assert_eq!(
            summary.highest,
            Some(BASE + MAX_MAPPINGS as u64 * PAGE_SIZE)
        );
        assert_eq!(summary.anonymous_pages, MAX_MAPPINGS as u64);
        assert_eq!(MemoryMap::INACTIVE.summary().count, 0);
        // 共有メモリは、無名のページには数えない。
        let mut map = active();
        map.reserve(
            2 * PAGE_SIZE,
            MappingKind::SharedMemory { shm: 0 },
            true,
            true,
        )
        .unwrap();
        assert_eq!(map.summary().anonymous_pages, 0);
        assert_eq!(
            map.find(BASE + PAGE_SIZE).map(|m| m.kind),
            Some(MappingKind::SharedMemory { shm: 0 })
        );
    }

    /// 区画の外へ `MAP_FIXED` で置いた写像（`brk` の見張り）は、要約の数には入るが、番地には出ない。最初に配った番地は、
    /// その写像を外しても変わらない。
    #[test]
    fn a_fixed_mapping_below_the_base_counts_but_does_not_move_the_addresses() {
        let mut map = active();
        let guard = BASE - 16 * PAGE_SIZE;
        map.register(
            guard,
            guard + PAGE_SIZE,
            MappingKind::Anonymous,
            false,
            false,
        )
        .unwrap();
        let summary = map.summary();
        assert_eq!(summary.count, 1);
        assert_eq!(summary.anonymous_pages, 1);
        assert_eq!(summary.first, None);
        assert_eq!(summary.highest, None);
        map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        map.reserve(PAGE_SIZE, MappingKind::Anonymous, true, true)
            .unwrap();
        map.release_whole(BASE, PAGE_SIZE).unwrap();
        let summary = map.summary();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.first, Some(BASE));
        assert_eq!(summary.highest, Some(BASE + 2 * PAGE_SIZE));
    }
}
