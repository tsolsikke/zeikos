//! `syscall` 命令の入口（2026-10-04）。
//!
//! Linux 向けのプログラム（musl・Rust の標準ライブラリ）は、システムコールを `syscall` 命令で呼ぶ。**`int 0x80` の
//! 入口は残し、こちらを足す。** どちらの入口から来ても、同じ形のフレーム（[`IrqContext`]）を積み、同じ関数
//! （`crate::syscall::syscall_entry`）へ入り、同じ番号表を引く。
//!
//! # スタックの切り替え——`swapgs` を使わない
//!
//! **`syscall` 命令は、スタックを切り替えない。** 入った時点の RSP は、ユーザーの値のままである。カーネルのスタックの
//! 番地を、レジスタを 1 つも壊さずに手に入れる必要がある。
//!
//! **CPU ごとに短いスタブを 1 つずつ持ち、それぞれが自分の CPU の置き場を RIP 相対で読む。** `LSTAR` は CPU ごとの
//! レジスタなので、CPU ごとに自分のスタブを指させる。スタブは、ユーザーの RSP を自分の CPU の退避の欄へ置き、
//! **その CPU の TSS の RSP0 を直に読んで** RSP へ入れる。`int 0x80` で入ったときに CPU が切り替える先と、同じ欄である
//! ——写しを持たないので、遠征の深さやタスクの切り替えで RSP0 が動いても、合わせる所が増えない。
//!
//! **`swapgs` を使わないので、カーネルは今までどおり `fs:` も `gs:` も使わない**（基本の検査が数えている）。割り込みと
//! 例外の入口にも触らない。**欠点は、CPU の数だけスタブが要ることである。** CPU の数を増やす段で、この形を見直す。
//!
//! # 入った直後の数命令
//!
//! スタブの最初の 2 命令の間は、Ring 0 のままユーザーの RSP で走る。**その間に届きうるものは、3 つとも専用のスタック
//! （IST）で受ける**——NMI・機械チェック・デバッグ例外である（`gdt::NMI_IST_INDEX` の doc）。割り込みは、入るときに
//! IF を落とすので届かない（[`FLAGS_CLEARED_ON_ENTRY`]）。2 命令が読み書きするのは、カーネルの静的な領域だけで、
//! フォルトしない。
//!
//! # 戻り方
//!
//! **`iretq` だけで戻る**（`int 0x80` と同じ出口を通る）。`sysretq` は使わない。`sysretq` は、戻り先（RCX）が正準で
//! ない番地のとき、Intel の石では Ring 0 のまま `#GP` を起こし、そのとき RSP は既にユーザーの値である。`iretq` なら
//! その形は起きない。**ただし `iretq` も、戻り先が正準でなければカーネルの中で `#GP` になる**ので、戻る直前に
//! 戻り先を確かめ、違えばそのプロセスを終わらせる（`IrqContext::returns_to_user_address`）。
//!
//! `iretq` で戻っても、RCX には戻り先、R11 には RFLAGS が入った状態で戻る（スタブが積んだ値を、出口がそのまま
//! 戻す）。Linux のプログラムから見た形は、`sysretq` で戻る場合と同じである。
//!
//! # 互換モードからの `syscall`
//!
//! GDT には 32 ビットのユーザーのコード区画が在る（`sysret` の並びのために空けられない）ので、Ring 3 はそこへ飛んで
//! から `syscall` を打てる。**Intel の石では `#UD` になり、今までどおり畳まれる。AMD の石では `CSTAR` の飛び先へ
//! 来る**ので、そこにも同じ形でスタックを切り替えるスタブを置き、そのプロセスを終わらせる。**Linux は 32 ビットの
//! 呼び出しを受け付けるので、ここは Linux との差である**（32 ビットは使わない。`docs/architecture.md` の
//! 「ABIの形は合わせる」）。

use core::ptr::addr_of;
use core::sync::atomic::{AtomicU64, Ordering};

use common::arch::x86_64::cpu::{self, Efer, SystemCallMsrs};
use common::log::Logger;
use common::machine::pc::serial::Serial;
use common::percpu::MAX_CPUS;

use crate::arch::x86_64::gdt;
use crate::arch::x86_64::idt::IrqContext;

/// フレームのベクタの欄に入れる目印——64 ビットのコードが打った `syscall` 命令から来た。
///
/// **割り込みのベクタ（0 から 255）と重ならない値にする。** `int 0x80` から来たフレームは、この欄に `0x80` を持つ。
pub const SYSTEM_CALL_INSTRUCTION_MARK: u64 = 0x100;

/// フレームのベクタの欄に入れる目印——互換モードのコードが打った `syscall` 命令から来た（AMD の石だけ）。
pub const COMPAT_SYSTEM_CALL_MARK: u64 = 0x101;

/// `syscall` 命令で入るときに、CPU が RFLAGS から落とすビット（SFMASK に入れる値）。
///
/// - **IF（`0x200`）**: スタックを切り替える前に、ユーザーの RSP の上で割り込みを受けないため。
/// - **TF（`0x100`）**: 入口の最初の命令で、Ring 0 のまま単発の実行の例外を受けないため。
/// - **DF（`0x400`）**: カーネルのコピーの向きを、ユーザーに決めさせないため（割り込みの入口の `cld` と同じ理由）。
/// - **AC（`0x40000`）**: SMAP を入れる段で、入口に `clac` を足さずに済むように、先に落としておく。
/// - **NT（`0x4000`）**: 立ったまま入ると、出口の `iretq` が `#GP` になり、カーネルが止まる。割り込みゲートは CPU が
///   落とすが、`syscall` 命令は落とさない。
/// - **IOPL（`0x3000`）**: ユーザーは変えられないが、落としておく（Linux と同じ）。
///
/// 破壊テスト (2026-10-04, sfmask-keeps-nt-test): NT を落とさない。**あるべき値もこの定数から作るので、読み戻しの
/// 確かめは通る**——NT を立てて `syscall` 命令を打ったプログラムの戻りで、`iretq` がカーネルの中で `#GP` を起こす。
pub const FLAGS_CLEARED_ON_ENTRY: u64 = if cfg!(feature = "sfmask-keeps-nt-test") {
    0x200 | 0x100 | 0x400 | 0x4_0000 | 0x3000
} else {
    0x200 | 0x100 | 0x400 | 0x4_0000 | 0x4000 | 0x3000
};

/// スタブが、カーネルのスタックへ切り替えるか（アセンブラの条件に渡す）。
///
/// 破壊テスト (2026-10-04, syscall-stub-keeps-user-stack-test): 切り替えない。**ユーザーのスタックの上で、カーネルが
/// 走る。** 入口の確かめ（[`IrqContext::note_system_call_entrance`]）が、スタックの番地を見て止める。
const STUB_SWITCHES_STACK: usize = if cfg!(feature = "syscall-stub-keeps-user-stack-test") {
    0
} else {
    1
};

/// ユーザーの番地の上限（これより下だけがユーザーの番地である）。
///
/// **正準な番地の下半分の、最後の 1 ページを使わせない。** 最後のページの末尾に `syscall` 命令を置くと、戻り先
/// （命令の次の番地）が `0x0000_8000_0000_0000` になり、正準でなくなる。Linux も同じ値を上限にしている。
///
/// **写す側（`mmap(MAP_FIXED)`・`munmap`・`mprotect`）も、この上限を見る**（2026-10-07。`crate::syscall` の
/// `exceeds_user_limit`。それまでは見ていなかった——`ADR-0082` の Addendum）。**戻る直前の確かめ
/// （`IrqContext::returns_to_user_address`）も在る。**
pub const USER_ADDRESS_LIMIT: u64 = 0x0000_7fff_ffff_f000;

/// スタブが、ユーザーの RSP を退避する欄（CPU ごと）。
///
/// **読み書きするのはスタブだけである**（Rust の側は番地を渡すだけ）。入ってから、カーネルのスタックへ積み直すまでの
/// 数命令の間だけ使う。その間は IF が落ちていて、NMI・機械チェック・デバッグ例外は専用のスタックで受けるので、
/// 同じ CPU の上で 2 つ目の `syscall` 命令が割り込むことは無い。
static mut USER_STACK_POINTER_AT_ENTRY: [u64; MAX_CPUS] = [0; MAX_CPUS];

/// TSS の中の RSP0 の位置（`gdt::layout` の単体試験が、同じ値を確かめている）。
const TSS_RSP0_OFFSET: usize = 4;
/// TSS の 1 つぶんの大きさ（同上）。
const TSS_SIZE: usize = 104;

// **スタブは CPU の数だけ書く。** 数が変わったら、ここでビルドを止める（スタブと、下の表を足すこと）。
const _: () = assert!(MAX_CPUS == 2);
const _: () =
    assert!(core::mem::size_of::<gdt::TaskStateSegment>() == TSS_SIZE && TSS_RSP0_OFFSET == 4);

// `syscall` 命令の入口のスタブ（CPU ごと）と、互換モードから来たときのスタブ（CPU ごと）。
//
// 入った時点: RCX = 戻り先、R11 = 入る前の RFLAGS、RSP = ユーザーの値、CS/SS = カーネルの区画（STAR から）、
// RFLAGS は SFMASK のビットが落ちている（IF=0）。ほかのレジスタは、ユーザーの値のままである。
//
// **割り込みで入ったときに CPU が積むのと同じ 5 語（SS・RSP・RFLAGS・CS・RIP）を自分で積み、その上に目印を積む。**
// そこから先は `int 0x80` の共通の経路（`zeikos_syscall_common`）へ合流する——退避・整列・`call`・復元・`iretq` が
// 同じなので、積む語数も同じで、スタックの境界の計算も共通で正しい（RSP0 はページの境界に在る）。
macro_rules! system_call_stubs {
    ($cpu:literal, $instruction_stub:literal, $compat_stub:literal) => {
        core::arch::global_asm!(
            ".section .text",
            ".p2align 4",
            concat!(".globl ", $instruction_stub),
            concat!($instruction_stub, ":"),
            // ユーザーの RSP を退避し、この CPU の TSS の RSP0 へ切り替える。**レジスタは 1 つも壊さない。**
            "  mov qword ptr [rip + {scratch} + {scratch_offset}], rsp",
            "  .if {switch_stack}",
            "  mov rsp, qword ptr [rip + {tss} + {rsp0_offset}]",
            "  .endif",
            "  push {user_ss}",
            "  push qword ptr [rip + {scratch} + {scratch_offset}]",
            "  push r11",
            "  push {user_cs}",
            "  push rcx",
            "  push {mark}",
            "  jmp zeikos_syscall_common",
            ".p2align 4",
            concat!(".globl ", $compat_stub),
            concat!($compat_stub, ":"),
            "  mov qword ptr [rip + {scratch} + {scratch_offset}], rsp",
            "  mov rsp, qword ptr [rip + {tss} + {rsp0_offset}]",
            "  push {user_ss}",
            "  push qword ptr [rip + {scratch} + {scratch_offset}]",
            "  push r11",
            "  push {user_cs32}",
            "  push rcx",
            "  push {compat_mark}",
            "  jmp zeikos_compat_syscall_common",
            scratch = sym USER_STACK_POINTER_AT_ENTRY,
            scratch_offset = const $cpu * 8,
            tss = sym gdt::TSS,
            rsp0_offset = const $cpu * TSS_SIZE + TSS_RSP0_OFFSET,
            user_ss = const gdt::USER_DATA_SELECTOR.bits(),
            user_cs = const gdt::USER_CODE_SELECTOR.bits(),
            user_cs32 = const gdt::USER_CODE32_SELECTOR.bits(),
            mark = const SYSTEM_CALL_INSTRUCTION_MARK,
            compat_mark = const COMPAT_SYSTEM_CALL_MARK,
            switch_stack = const STUB_SWITCHES_STACK,
        );
    };
}

system_call_stubs!(
    0,
    "zeikos_syscall_instruction_stub_0",
    "zeikos_compat_syscall_stub_0"
);
system_call_stubs!(
    1,
    "zeikos_syscall_instruction_stub_1",
    "zeikos_compat_syscall_stub_1"
);

// 互換モードから来た `syscall` の共通の続き。**戻らない**——レジスタを積んで（記録のため）、プロセスを終わらせる
// 関数を呼ぶ。積む順は `zeikos_syscall_common` と同じで、[`IrqContext`] の欄の順と一対一である。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zeikos_compat_syscall_common",
    "zeikos_compat_syscall_common:",
    "  push r15",
    "  push r14",
    "  push r13",
    "  push r12",
    "  push r11",
    "  push r10",
    "  push r9",
    "  push r8",
    "  push rbp",
    "  push rdi",
    "  push rsi",
    "  push rdx",
    "  push rcx",
    "  push rbx",
    "  push rax",
    "  cld",
    "  mov rdi, rsp",
    "  sub rsp, {adjust}",
    "  mov rsi, rsp",
    "  call {refuse}",
    "  ud2",
    refuse = sym refuse_compat_system_call,
    adjust = const crate::arch::x86_64::idt::STACK_ALIGN_ADJUST,
);

extern "C" {
    static zeikos_syscall_instruction_stub_0: u8;
    static zeikos_syscall_instruction_stub_1: u8;
    static zeikos_compat_syscall_stub_0: u8;
    static zeikos_compat_syscall_stub_1: u8;
}

/// 互換モードのコードが打った `syscall` を断り、そのプロセスを終わらせる。**戻らない。**
///
/// **無効な命令の例外（ベクタ 6）として記録する**——Intel の石が同じ場面で起こす例外と、同じ終わり方に揃える。
///
/// # Safety
///
/// `context` はスタブが積んだ有効な [`IrqContext`] を指していること。**呼ぶのは asm のスタブだけである**（`sym` で
/// 指す）。Ring 3 から `syscall` 命令で入った文脈で、遠征の中であること（遠征の外から Ring 3 は走らない）。
unsafe extern "sysv64" fn refuse_compat_system_call(
    context: *const IrqContext,
    rsp_at_call: u64,
) -> ! {
    // SAFETY: スタブが直前に積んだ有効な文脈を指す。読み取りのみ。
    let context = unsafe { &*context };
    ENTRANCES[Entrance::CompatInstruction as usize].fetch_add(1, Ordering::Relaxed);
    // SAFETY: Ring 3 から来た文脈で、遠征の中である（この関数の契約）。BKL は取っていない。
    unsafe {
        crate::arch::x86_64::ring3::record_and_fold(
            6,
            context.cs,
            context.rip,
            context.rsp,
            0,
            0,
            rsp_at_call,
        )
    }
}

/// システムコールが、どの入口から来たか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entrance {
    /// `int 0x80`。
    Interrupt = 0,
    /// 64 ビットのコードが打った `syscall` 命令。
    Instruction = 1,
    /// 互換モードのコードが打った `syscall` 命令（断る）。
    CompatInstruction = 2,
}

/// 入口ごとの、来た回数（起動してからの合計。全 CPU の合計）。
static ENTRANCES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

impl IrqContext {
    /// このフレームが、どの入口から来たかを数える（システムコールの入口が、入ってすぐに呼ぶ）。
    ///
    /// **フレームのベクタの欄で見分ける**——`syscall` 命令のスタブは [`SYSTEM_CALL_INSTRUCTION_MARK`] を積み、
    /// `int 0x80` のスタブはベクタの番号を積む。ベクタの欄を、共通の側に出さない（`ADR-0072` の 3）。
    ///
    /// **カーネルへ入ったスタックが、遠征のスタックの中に在ることも確かめる**（`rsp_at_call` は、スタブが `call` の
    /// 直前に読んだ RSP）。`int 0x80` では CPU が、`syscall` 命令ではスタブが、TSS の RSP0 へ切り替える。**切り替えて
    /// いなければ、カーネルはユーザーが決めたスタックの上で走っている。** 名指しして止まる。
    pub(crate) fn note_system_call_entrance(&self, rsp_at_call: u64) {
        let entrance = if self.vector == SYSTEM_CALL_INSTRUCTION_MARK {
            Entrance::Instruction
        } else {
            Entrance::Interrupt
        };
        ENTRANCES[entrance as usize].fetch_add(1, Ordering::Relaxed);
        let (bottom, top) = crate::arch::x86_64::ring3::excursion_stack_range();
        if !(bottom..top).contains(&rsp_at_call) {
            use core::fmt::Write as _;
            let mut serial = Serial::primary();
            serial.init();
            let _ = writeln!(
                serial,
                "[ERROR] syscall entry: the kernel was entered on a stack outside the excursion \
                 stack (rsp at call {rsp_at_call:#x}, the excursion stack is {bottom:#x}..{top:#x}, \
                 entrance {entrance:?}); the stub or the CPU did not switch to the kernel stack"
            );
            let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");
            cpu::halt_forever();
        }
    }

    /// 試しの形 (2026-10-04, syscall-return-noncanonical-test。破壊ではない): `syscall` 命令から来た最初の呼び出しの
    /// 戻り先を、正準でない番地に書き換える。**戻る直前の確かめ（[`returns_to_user_address`](Self::returns_to_user_address)）
    /// が、そのプロセスを終わらせること**を見る。今は、戻り先を書き換える経路がほかに無いので、この形で作る。
    #[cfg(feature = "syscall-return-noncanonical-test")]
    pub(crate) fn corrupt_return_address_for_the_test(&mut self) {
        if self.vector == SYSTEM_CALL_INSTRUCTION_MARK
            && ENTRANCES[Entrance::Instruction as usize].load(Ordering::Relaxed) == 1
        {
            self.rip = 0x0000_8000_0000_0000;
        }
    }

    /// このフレームの戻り先が、ユーザーの範囲の正準な番地か（[`USER_ADDRESS_LIMIT`] より下か）。
    ///
    /// **Ring 3 へ戻る直前に確かめる。** 正準でない番地へ `iretq` で戻ろうとすると、カーネルの中で `#GP` になり、
    /// カーネルが止まる。カーネルの番地へ戻ろうとするフレームも、ここで断る。**今は、戻り先を書き換える経路が無い**
    /// ので、偽になることは無い。シグナルから戻る経路（ユーザーが戻り先を用意する）でも、同じ確かめを通す。
    pub(crate) fn returns_to_user_address(&self) -> bool {
        // 破壊テスト (2026-10-04, syscall-return-check-off-test): 確かめない。戻り先を正準でない番地にした形と
        // 組み合わせる。**本物の Intel の石では、出口の `iretq` がカーネルの中で `#GP` を起こす。** **QEMU の TCG は
        // そこでは止めず、Ring 3 へ移ってからページフォルトにする**（実測）ので、検査が見るのは「確かめが終わらせた
        // 記録（13）が出ない」ことである。
        if cfg!(feature = "syscall-return-check-off-test") {
            return true;
        }
        self.rip < USER_ADDRESS_LIMIT
    }
}

/// 戻り先がユーザーの番地でないフレームを断り、そのプロセスを終わらせる。**戻らない。**
///
/// **一般保護例外（ベクタ 13）として記録する**——確かめずに戻れば、`iretq` が同じ例外を起こす場面である。
///
/// # Safety
///
/// Ring 3 から入ったシステムコールの文脈で、遠征の中であること。**BKL は、呼ぶ前に解いておく**（longjmp で戻るので、
/// `Drop` が走らない。`crate::bkl` の `UNWINDLESS_RELEASE_ENTRIES`）。
pub unsafe fn refuse_system_call_return(context: &IrqContext, handler_rsp: u64) -> ! {
    // SAFETY: この関数の契約。
    unsafe {
        crate::arch::x86_64::ring3::record_and_fold(
            13,
            context.cs,
            context.rip,
            context.rsp,
            0,
            0,
            handler_rsp,
        )
    }
}

/// 入口ごとの、来た回数（`int 0x80`、`syscall` 命令、互換モードの `syscall` 命令の順）。
pub fn entrances() -> [u64; 3] {
    [0, 1, 2].map(|at| ENTRANCES[at].load(Ordering::Relaxed))
}

/// 入口ごとの、来た回数を 1 行に出す。
pub fn report_entrances(logger: &mut Logger<Serial>) {
    let [interrupt, instruction, compat] = entrances();
    logger.info(format_args!(
        "syscall-entry: so far {interrupt} system call(s) came in through int 0x80, {instruction} \
         through the syscall instruction, and {compat} from compatibility mode (refused)"
    ));
}

/// `cpu` 番の CPU の、`syscall` 命令の入口を決める MSR のあるべき値。
///
/// - `STAR`: 上位 16 ビットが `sysret` の基点（32 ビットのユーザーのコード区画。+8 がユーザーのデータ、+16 が 64 ビットの
///   ユーザーのコード）、その下の 16 ビットが `syscall` の基点（カーネルのコード。+8 がカーネルのデータ）。**GDT の
///   並びは、この形に合わせてある**（`ADR-0020`）。
/// - `LSTAR`・`CSTAR`: その CPU のスタブの番地。
/// - `SFMASK`: [`FLAGS_CLEARED_ON_ENTRY`]。
/// - `IA32_SYSENTER_CS`: 0（`sysenter` 命令は `#GP` になる）。
///
/// **`cpu` が範囲の外なら `None`。**
pub fn expected_msrs(cpu: usize) -> Option<SystemCallMsrs> {
    // `global_asm!` が定義した記号の番地を取るだけである（読まない）。
    let (lstar, cstar) = match cpu {
        0 => (
            addr_of!(zeikos_syscall_instruction_stub_0) as u64,
            addr_of!(zeikos_compat_syscall_stub_0) as u64,
        ),
        1 => (
            addr_of!(zeikos_syscall_instruction_stub_1) as u64,
            addr_of!(zeikos_compat_syscall_stub_1) as u64,
        ),
        _ => return None,
    };
    Some(SystemCallMsrs {
        star: (u64::from(gdt::USER_CODE32_SELECTOR.bits()) << 48)
            | (u64::from(gdt::KERNEL_CODE_SELECTOR.bits()) << 32),
        lstar,
        cstar,
        sfmask: FLAGS_CLEARED_ON_ENTRY,
        sysenter_cs: 0,
    })
}

/// この CPU で、`syscall` 命令を入口にする。**MSR を書き、`EFER.SCE` を立てる。**
///
/// **MSR を先に書き、SCE を最後に立てる**——立てた瞬間から `syscall` 命令が飛べるので、飛び先が決まってから立てる。
/// **この CPU に MSR が無ければ、何も書かずに偽を返す**（読み戻しの確かめが、名指しして止める）。
///
/// # Safety
///
/// - `cpu` が、実行中の CPU のスロットであること。その CPU の GDT と TSS が載っていること（スタブが TSS の RSP0 を読む）。
/// - NMI・機械チェック・デバッグ例外が IST に載っていること（このモジュールの doc の「入った直後の数命令」）。
/// - 起動の途中で、割り込みを禁じたまま、CPU ごとに 1 回だけ呼ぶこと。
pub unsafe fn enable_on_this_cpu(cpu_index: usize) -> bool {
    // 破壊テスト (2026-10-04, bsp-skips-syscall-entry-test / ap-skips-syscall-entry-test): BSP か AP で、据えない。
    // **読み戻しの確かめが、その CPU を名指しして止まる。**
    if (cfg!(feature = "bsp-skips-syscall-entry-test") && cpu_index == 0)
        || (cfg!(feature = "ap-skips-syscall-entry-test") && cpu_index != 0)
    {
        return false;
    }
    let Some(msrs) = expected_msrs(cpu_index) else {
        return false;
    };
    // SAFETY: MSR が在ることは `write_system_call_msrs` が CPUID で確かめる。値は、この CPU のスタブと GDT の区画を
    // 指す、あるべき値である。
    if !unsafe { cpu::write_system_call_msrs(msrs) } {
        return false;
    }
    // SAFETY: SCE だけを立てる（LME と NXE はそのまま）。飛び先は上で書いた。
    unsafe {
        cpu::write_efer(Efer::from_raw(
            cpu::read_efer().raw() | Efer::SYSCALL_ENABLE,
        ))
    };
    true
}
