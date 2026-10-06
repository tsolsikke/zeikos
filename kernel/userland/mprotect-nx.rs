//! `mprotect-nx`: 自分のコードのページを `mprotect(PROT_READ)` で実行できない形にして、ページフォルトで終わらせられる
//! ユーザープログラム（2026-10-06）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独でリンクし、
//! ディスク像の `/bin` に置く。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! `_start` の在るページへ `mprotect(PROT_READ)` を打つ。**`PROT_EXEC` を付けない `mprotect` はページを実行できない形に
//! する**（W^X。実行を外す向き）ので、システムコールから戻った次の命令の取り出しがページフォルト（ベクタ 14）になり、
//! カーネルがこのプロセスを畳む。`syscall-test` が `spawn` で起こし、`spawn` の戻り値が「畳まれた・ベクタ 14」である
//! ことを検算する（112 番）。
//!
//! **戻ってきて走り続けたら、間違いである**——そのときは 1 で終わる。`mprotect` が失敗したら 2 で終わる。

#![no_std]
#![no_main]

core::arch::global_asm!(
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // mprotect(page_of(_start), 4096, PROT_READ)
    "  lea rdi, [rip + _start]",
    "  and rdi, -4096",
    "  mov eax, {sys_mprotect}",
    "  mov esi, 4096",
    "  mov edx, 1",
    "  int 0x80",
    // **ここへは戻ってこないはずである**（このページは、もう実行できない）。
    "  test rax, rax",
    "  jnz 8f",
    "  mov eax, {sys_exit}",
    "  mov edi, 1",
    "  int 0x80",
    "  ud2",
    "8:",
    "  mov eax, {sys_exit}",
    "  mov edi, 2",
    "  int 0x80",
    "  ud2",

    sys_mprotect = const 10,
    sys_exit = const 60,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
