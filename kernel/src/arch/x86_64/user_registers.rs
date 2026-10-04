//! ユーザーの実行の文脈が持つレジスタ——FP の状態と、FS・GS の基底（2026-10-05）。
//!
//! # なぜ 1 つの組にするか
//!
//! **保存と復元が要る場面が、FP の状態と FS・GS の基底で同じだからである。**
//!
//! - タスクの切り替え（出る側を保存し、入る側を復元する）
//! - プログラムを走らせる直前（新しいプログラムは、既定の値から始まる）
//! - 子を起動する前後（親の値を控え、戻ったら戻す。子は同じタスクの上で、入れ子で走る）
//!
//! 別の置き場にすると、入れ替える所が 2 系統になり、片方だけ忘れる形ができる。**1 つの型にして、入れ替える
//! 関数も 1 組にする**（[`save_user_registers`] と [`restore_user_registers`]）。
//!
//! # 置き場は「実行の文脈」に付ける
//!
//! **プロセスではなく、タスクごとに持つ**（共通の側の `crate::task` が、タスクの数だけ並べる）。スレッドが入ると、
//! スレッドごとにタスクを持つことになり、この組は、そのままスレッドごとのものになる。プロセスが持つもの
//! （アドレス空間・ファイルの表・ヒープと `mmap` の番地）とは、分けてある。
//!
//! # FS と GS の基底
//!
//! **Linux 向けのプログラムは、スレッドローカルの領域（TLS）を FS の基底から引く。** 基底は、`arch_prctl` で
//! 入れる（`crate::syscall` の `sys_arch_prctl`）。
//!
//! - **カーネルは FS も GS も使わない**（`fs:`・`gs:`・`swapgs` が 0 個であることを、基本の検査が数えている）。
//!   カーネルへ入るとき・出るときには、基底に触らない。**ユーザーは、FS も GS も自由に使える。**
//!   **将来 `swapgs` を入れると決めたら、GS をユーザーに使わせる所は作り直しになる。**
//! - **出る側では、控えた値を信じずに、レジスタから読んで保存する。** ユーザーは `arch_prctl` 以外でも基底を
//!   変えられる——FS に区画のセレクタを載せ直すと、Intel の石では基底が 0 になる。
//! - **MSR で読み書きする。** `CR4.FSGSBASE` は 0 のままにする（立てると、Ring 3 が命令で基底を書ける。
//!   `cpu_state` の棚卸し）。読んで保存する形にしてあるので、後で立てても、置き場と入れ替えの位置は変わらない。

use common::arch::x86_64::cpu;

use crate::arch::x86_64::fp::{restore_fp_state, save_fp_state, FpArea};
use crate::arch::x86_64::system_call_entry::USER_ADDRESS_LIMIT;

/// ユーザーの実行の文脈が持つレジスタの組（FP の状態と、FS・GS の基底）。
///
/// # 契約（境界の型）
///
/// - 共通の側（`crate::task` と `crate::userland`）は、置き場を持ち、[`save_user_registers`] と
///   [`restore_user_registers`] へ渡すだけである。中身は読まない。
#[derive(Clone, Copy)]
pub struct UserRegisters {
    fp: FpArea,
    fs_base: u64,
    gs_base: u64,
}

impl UserRegisters {
    /// 新しいプログラムが始まるときの値。**FP は既定の状態、FS と GS の基底は 0 である。**
    pub const fn fresh() -> Self {
        Self {
            fp: FpArea::fresh(),
            fs_base: 0,
            gs_base: 0,
        }
    }

    /// FP の状態の部分。
    pub fn fp(&self) -> &FpArea {
        &self.fp
    }

    /// 控えてある FS の基底。
    pub const fn fs_base(&self) -> u64 {
        self.fs_base
    }

    /// 控えてある GS の基底。
    pub const fn gs_base(&self) -> u64 {
        self.gs_base
    }
}

/// いまのユーザーのレジスタ（FP の状態と、FS・GS の基底）を、領域へ書き出す。
///
/// # 契約（境界の関数）
///
/// - この CPU のレジスタを読んで書き出す。ほかの CPU には触れない。**基底は、レジスタ（MSR）から読む。**
/// - 呼ぶのは、切り替え（`crate::task`）と、子を起動する前に親の値を控える所（`crate::userland`）である。
///
/// # Safety
///
/// [`save_fp_state`] と同じ（`area` は、型が保証する境界と大きさを持つ）。
pub unsafe fn save_user_registers(area: &mut UserRegisters) {
    // SAFETY: この関数の契約。
    unsafe { save_fp_state(&mut area.fp) };
    area.fs_base = cpu::read_fs_base();
    area.gs_base = cpu::read_gs_base();
}

/// 領域から、ユーザーのレジスタ（FP の状態と、FS・GS の基底）を戻す。
///
/// # 契約（境界の関数）
///
/// - この CPU のレジスタへ書く。ほかの CPU には触れない。
/// - 呼ぶのは、切り替え（`crate::task`）と、プログラムの起動と子の終わり（`crate::userland`）である。
///
/// # Safety
///
/// **領域の中身が、[`save_user_registers`] が書いたものか、[`UserRegisters::fresh`] であること。**
/// FP の部分は [`restore_fp_state`] の契約に従う。**基底は、正準な番地であること**（正準でない値を MSR へ書くと、
/// カーネルの中で一般保護例外になる。[`set_user_fs_base`] と [`set_user_gs_base`] は、ユーザーの範囲の番地だけを
/// 通すので、読んで保存した値は正準である）。
pub unsafe fn restore_user_registers(area: &UserRegisters) {
    // SAFETY: この関数の契約。
    unsafe {
        restore_fp_state(&area.fp);
        restore_user_segment_bases(area);
    }
}

/// 領域から、FS・GS の基底だけを戻す（[`restore_user_registers`] の後半）。
///
/// # Safety
///
/// [`restore_user_registers`] と同じ（基底が正準な番地であること）。
pub unsafe fn restore_user_segment_bases(area: &UserRegisters) {
    // SAFETY: この関数の契約。
    unsafe {
        cpu::write_fs_base(area.fs_base);
        cpu::write_gs_base(area.gs_base);
    }
}

/// `address` を、ユーザーが FS・GS の基底にしてよいか。**ユーザーの範囲の、正準な番地だけを通す**
/// （[`USER_ADDRESS_LIMIT`] より下）。カーネルの番地と、正準でない番地は断る。
pub const fn is_allowed_user_segment_base(address: u64) -> bool {
    address < USER_ADDRESS_LIMIT
}

/// いまのユーザーの FS の基底を、`address` にする。**断ったら偽**（[`is_allowed_user_segment_base`]）。
///
/// **書くのは、この CPU のレジスタである。** 走っている文脈のものなので、置き場へは、次に出るときに
/// [`save_user_registers`] が読んで入れる。
pub fn set_user_fs_base(address: u64) -> bool {
    // 破壊テスト (2026-10-05, arch-prctl-skips-address-check): 番地を確かめない。**本物の石では、正準でない値を
    // MSR へ書くと、カーネルの中で一般保護例外になる。** **QEMU の TCG は例外にしない**（実測）ので、検査が見るのは
    // 「断られずに通ってしまう」ことである。
    if !is_allowed_user_segment_base(address) && !cfg!(feature = "arch-prctl-skips-address-check") {
        return false;
    }
    // SAFETY: 上で、ユーザーの範囲の正準な番地であることを確かめた。カーネルは FS を使わない。
    unsafe { cpu::write_fs_base(address) };
    true
}

/// いまのユーザーの GS の基底を、`address` にする。**断ったら偽。**
pub fn set_user_gs_base(address: u64) -> bool {
    if !is_allowed_user_segment_base(address) {
        return false;
    }
    // SAFETY: 上で、ユーザーの範囲の正準な番地であることを確かめた。カーネルは GS を使わない（`swapgs` も無い）。
    unsafe { cpu::write_gs_base(address) };
    true
}

/// いまのユーザーの FS の基底（この CPU のレジスタから読む）。
pub fn user_fs_base() -> u64 {
    // 破壊テスト (2026-10-05, arch-prctl-get-fs-returns-zero): レジスタを読まずに 0 を返す。
    if cfg!(feature = "arch-prctl-get-fs-returns-zero") {
        return 0;
    }
    cpu::read_fs_base()
}

/// いまのユーザーの GS の基底（この CPU のレジスタから読む）。
pub fn user_gs_base() -> u64 {
    cpu::read_gs_base()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新しいプログラムは、基底を持たずに始まる。
    #[test]
    fn a_fresh_context_has_no_segment_bases() {
        let fresh = UserRegisters::fresh();
        assert_eq!(fresh.fs_base(), 0);
        assert_eq!(fresh.gs_base(), 0);
    }

    /// 基底にしてよいのは、ユーザーの範囲の正準な番地だけである。**上限ちょうど・正準でない番地・カーネルの番地は
    /// 断る。**
    #[test]
    fn only_user_addresses_may_become_a_segment_base() {
        assert!(is_allowed_user_segment_base(0));
        assert!(is_allowed_user_segment_base(0x40_0000));
        assert!(is_allowed_user_segment_base(USER_ADDRESS_LIMIT - 1));
        assert!(!is_allowed_user_segment_base(USER_ADDRESS_LIMIT));
        assert!(!is_allowed_user_segment_base(0x0000_8000_0000_0000));
        assert!(!is_allowed_user_segment_base(0xffff_8000_0000_0000));
        assert!(!is_allowed_user_segment_base(0xffff_ffff_8010_0000));
        assert!(!is_allowed_user_segment_base(u64::MAX));
    }
}
