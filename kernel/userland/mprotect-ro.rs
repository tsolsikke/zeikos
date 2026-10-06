//! `mprotect-ro`: 書けなくしたページへ書いて、ページフォルトで終わらせられるユーザープログラム（2026-10-06）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独でリンクし、
//! ディスク像の `/bin` に置く。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! 無名の 1 ページを `mmap` で取って書き、`mprotect(PROT_READ)` で書けなくしてから、もう 1 度書く。**書けなくなって
//! いれば、2 度目の書きがページフォルト（ベクタ 14）になり、カーネルがこのプロセスを畳む。** `syscall-test` が
//! `spawn` で起こし、`spawn` の戻り値が「畳まれた・ベクタ 14」であることを検算する（110 番）。
//!
//! **書けてしまったら、間違いである**——そのときは 1 で終わる。`mmap` や `mprotect` が失敗したら 2 で終わる。

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
    // mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, 4096",
    "  mov edx, 3",
    "  mov r10d, 0x22",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  test rax, rax",
    "  js 8f",
    "  mov r12, rax",
    "  mov qword ptr [r12], 7",
    // mprotect(page, 4096, PROT_READ)
    "  mov eax, {sys_mprotect}",
    "  mov rdi, r12",
    "  mov esi, 4096",
    "  mov edx, 1",
    "  int 0x80",
    "  test rax, rax",
    "  jnz 8f",
    // **ここで書く。** 書けなくなっていれば、戻ってこない。
    "  mov qword ptr [r12], 8",
    "  mov eax, {sys_exit}",
    "  mov edi, 1",
    "  int 0x80",
    "  ud2",
    "8:",
    "  mov eax, {sys_exit}",
    "  mov edi, 2",
    "  int 0x80",
    "  ud2",

    sys_mmap = const 9,
    sys_mprotect = const 10,
    sys_exit = const 60,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
