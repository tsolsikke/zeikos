//! `futex-wait`: `FUTEX_WAIT` で、本当なら待つ場面に入るユーザープログラム（2026-10-06）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独でリンクし、
//! ディスク像の `/bin` に置く。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! スタックの 1 語に 1 を書き、`futex(その番地, FUTEX_WAIT|FUTEX_PRIVATE_FLAG, 1, NULL)` を打つ。**値が同じなので、
//! Linux なら、誰かが起こすまで眠る。** このカーネルにはスレッドが 1 本しか無く、起こす者は居ないので、カーネルは
//! 偽りの戻り値を返さず、名指しして、このプロセスを終わらせる（終了状態 137。`kernel/src/syscall.rs` の
//! `FUTEX_DEADLOCK_STATUS`。`ADR-0081`）。
//!
//! **ここから戻ってきたら、間違いである**——戻ってきたときは 1 で終わる。`syscall-test` が `spawn` で起こし、
//! `spawn` の戻り値が 137 であることを検算する（93 番）。

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
    "  sub rsp, 16",
    "  mov dword ptr [rsp], 1",
    "  mov eax, {sys_futex}",
    "  mov rdi, rsp",
    "  mov esi, {futex_wait_private}",
    "  mov edx, 1",
    "  xor r10d, r10d",
    "  int 0x80",
    // **戻ってきたら間違いである。** 1 で終わる。
    "  mov eax, {sys_exit}",
    "  mov edi, 1",
    "  int 0x80",
    "  ud2",

    sys_futex = const 202,
    futex_wait_private = const 0 | 128,
    sys_exit = const 60,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
