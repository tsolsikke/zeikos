//! `compat-syscall`: Ring 3 で 32 ビットのコード区画へ移り、そこから `syscall` 命令を打つユーザープログラム
//! （2026-10-04）。
//!
//! # crate ではない
//!
//! `debug-trap.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独で
//! リンクし、できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何を見るためか
//!
//! **ZeikOS は 32 ビットの呼び出しを受け付けない。** ただし GDT には 32 ビットのユーザーのコード区画が在る
//! （`sysret` の並びのために空けられない）ので、Ring 3 はそこへ移って `syscall` 命令を打てる。そのとき何が起きるかは、
//! CPU の製造元で違う。
//!
//! - **Intel の石**: 互換モードの `syscall` は無効な命令で、`#UD` になる。今までどおり畳まれる。
//! - **AMD の石**: `CSTAR` の飛び先へ来る。そこに置いたスタブが、スタックを切り替えてから、このプロセスを終わらせる
//!   （無効な命令の例外として記録する）。
//!
//! **どちらでも、このプロセスだけが終わり、カーネルは止まらない。** カーネルの側は、無効な命令の例外で、32 ビットの
//! 区画から来て、`syscall` 命令の位置（Intel）か、その次の位置（AMD）で止まったことを突き合わせる。
//! **Linux は 32 ビットの呼び出しを受け付けるので、ここは Linux との差である。**
//!
//! # 移り方
//!
//! 遠い戻り（`retfq`）で移る。スタックに、32 ビットのコード区画のセレクタ（`0x1b`）と、移る先の番地を積む。
//! プログラムは 4 GiB より下に在るので、32 ビットの区画からそのまま続きを実行できる。

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
    "  lea rax, [rip + 2f]",
    "  push 0x1b",
    "  push rax",
    "  retfq",
    // ここから 32 ビットの区画で走る。**`syscall` 命令の位置は、entry から `0x20` である**（`kernel/src/main.rs` の
    // `COMPAT_SYSCALL_OFFSET` と対になっている）。
    ".org 0x20",
    "2:",
    ".code32",
    "  syscall",
    // 断られずに戻ってきたときの受け皿。
    "  ud2",
    ".code64",
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
