//! `tkill-self`: `tkill(gettid(), SIGABRT)` で自分へシグナルを送って、終わらせられるユーザープログラム（2026-10-06）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独でリンクし、
//! ディスク像の `/bin` に置く。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! musl の `abort` と同じ形——`gettid` で自分の番号を取り、`tkill(tid, SIGABRT)` を打つ。**カーネルはシグナルを配送
//! しない**ので、名指しの行を出してこのプロセスを終わらせ、終了状態は `128 + 6 = 134` になる。`syscall-test` が
//! `spawn` で起こし、`spawn` の戻り値が 134 であることを検算する（119 番）。
//!
//! **戻ってきて走り続けたら、間違いである**——そのときは 1 で終わる。`tkill` が失敗したら 2 で終わる。

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
    // tid = gettid()
    "  mov eax, {sys_gettid}",
    "  int 0x80",
    "  mov rdi, rax",
    // tkill(tid, SIGABRT)
    "  mov eax, {sys_tkill}",
    "  mov esi, {sigabrt}",
    "  int 0x80",
    // **ここへは戻ってこないはずである**（終わらせられている）。
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

    sys_gettid = const 186,
    sys_tkill = const 200,
    sigabrt = const 6,
    sys_exit = const 60,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
