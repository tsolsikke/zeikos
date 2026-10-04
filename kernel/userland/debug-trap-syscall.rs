//! `debug-trap-syscall`: Ring 3 で TF を立ててから `syscall` 命令を打つユーザープログラム（2026-10-04）。
//!
//! # crate ではない
//!
//! `debug-trap.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独で
//! リンクし、できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何を見るためか
//!
//! **TF（単発の実行の旗）を立てたまま `syscall` 命令でカーネルへ入ると、何もしなければ、入口の最初の命令で
//! デバッグ例外が起きる**——Ring 0 のまま、まだユーザーの RSP で走っている所である。**入るときに CPU が TF を落とす
//! ようにしてある**（SFMASK）ので、デバッグ例外はカーネルの中では起きない。システムコールは普通に戻り、**戻った後の
//! Ring 3 の側で**デバッグ例外が起きて、このプロセスだけが終わる。
//!
//! カーネルの側は、ベクタ（1）と、止まった番地と、Ring 3 から来たことを突き合わせる（`kernel/src/main.rs` の
//! `USER_PROGRAMS`）。**カーネルの中で起きていれば、畳まれずにカーネルが止まる**（Ring 0 の例外は畳まない）。
//!
//! # 止まる番地
//!
//! `popfq` で TF が立つ。次の命令が `syscall` である。`syscall` は入るときに TF を落とし、戻りの `iretq` が TF を
//! 戻す。**その次の命令（`nop`）を 1 つ実行した所で、デバッグ例外が起きる**（QEMU の実測）。止まった番地は、`nop` の
//! 次の `ud2` である。デバッグ例外が起きなければ、そこで無効な命令の例外になる。

#![no_std]
#![no_main]

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // 実装していない番号を呼ぶ（-ENOSYS が返るだけで、何も変えない）。**番号は、TF を立てる前に置く。**
    "  mov eax, 0x999",
    "  pushfq",
    "  or qword ptr [rsp], 0x100",
    "  popfq",
    "  syscall",
    "  nop",
    // 止まった番地として報告される位置（`kernel/src/main.rs` の `DEBUG_TRAP_SYSCALL_STOP_OFFSET` と対になっている）。
    "  ud2",
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
