//! `debug-trap`: Ring 3 で TF を立て、デバッグ例外（#DB）で終了させられるユーザープログラム（2026-10-04）。
//!
//! # crate ではない
//!
//! `fault-test.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独で
//! リンクし、できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! **RFLAGS の TF（単発の実行の旗）を自分で立てる。** TF は Ring 3 から `popfq` で立てられる。立てた次の命令
//! （`nop`）を 1 つ実行した所で、CPU がデバッグ例外（ベクタ 1）を起こす。`exit` は呼ばない。**このプロセスは、
//! 例外による終了処理で終わる。**
//!
//! # 何を見るためか
//!
//! **デバッグ例外は IST のスタック（IST5）で配送される**（`kernel/src/arch/x86_64/gdt/mod.rs` の
//! `DEBUG_IST_INDEX`）。カーネルは、Ring 3 から来た例外を畳む前に、処理が「そのベクタのゲートが指すスタック」の
//! 上で走っていることを確かめる（`idt` の `exception_frame_is_trustworthy`）。**このプログラムが畳まれて終われば、
//! デバッグ例外が実際に IST5 の上で配送されたことになる。** IST に載っていなければ、処理は遠征のスタックの上で走る
//! ——ゲートと TSS の確かめは起動時に別に在るので、そちらが先に止める。
//!
//! カーネルの側は、ベクタ（1）と止まった番地を、決まった値と突き合わせる（`kernel/src/main.rs` の
//! `USER_PROGRAMS`）。**デバッグ例外はトラップなので、止まった番地は「実行し終えた命令の次」である**——`nop` の次の
//! `ud2` の番地になる。
//!
//! # 例外が起きなかったときの行き先
//!
//! **`nop` の次に `ud2` を置いてある。** デバッグ例外が起きなければ、そこで #UD になり、ベクタが 1 ではなく 6 に
//! なる。止まった番地は同じなので、カーネルの側の判定行はベクタの食い違いとして止める。

#![no_std]
#![no_main]

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。entry と最初の `PT_LOAD` の先頭を一致させない。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // RFLAGS を積み、TF（ビット 8）を立てて、戻す。
    "  pushfq",
    "  or qword ptr [rsp], 0x100",
    "  popfq",
    // TF が立った後の最初の命令。**これを実行し終えた所でデバッグ例外が起きる。**
    "  nop",
    // 止まった番地として報告される位置（entry から 11 バイト目。`pushfq` 1・`or` 8・`popfq` 1・`nop` 1 の後。
    // `kernel/src/main.rs` の `DEBUG_TRAP_STOP_OFFSET` と対になっている）。**デバッグ例外が起きなかったときの
    // 受け皿でもある。** **間を詰め物で空けない**——空けると、`nop` の次に詰め物を実行してしまう。
    "  ud2",
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
