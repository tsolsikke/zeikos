//! `m3a-spawn-from-thread-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドを
//! 持つプロセスが ZeikOS の `spawn`（番号 0x1004）を打つ形**（M3a の 3-2。`ADR-0085` の決定 3——スレッドを持つ間は断る）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作る。スレッドは旗を待つ。
//! 2. 主スレッドは、スレッドが生きている間に `spawn` を打つ。ZeikOS は `-EAGAIN`（起動ログの `thread: spawn … refused
//!    with -EAGAIN: ThreadsAlive`）、Linux は番号を知らないので `-ENOSYS`。どちらも負なので、同じ行を出す。
//! 3. 主スレッドが旗を立て、スレッドが `spawn` を打つ。ZeikOS は `-EAGAIN`（`FromThread`）、Linux は `-ENOSYS`。
//! 4. 主スレッドはスレッドを `join` して、引数の数で終わる。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// ZeikOS の `spawn`（`kernel/src/abi/private.rs`）。Linux には無い番号。
const SYS_SPAWN: u64 = 0x1004;

/// `syscall` 命令を直に打つ（3 引数）。
fn raw_syscall3(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    // SAFETY: Linux の x86_64 の呼び出し規約どおりにレジスタを渡し、壊れる RCX と R11 を申告する。番号が未知でも
    // カーネルは負の errno を返すだけで、メモリには触れない。
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn try_spawn(who: &str) {
    let path = b"/bin/ls\0";
    let argv: [*const u8; 2] = [path.as_ptr(), std::ptr::null()];
    let envp: [*const u8; 1] = [std::ptr::null()];
    let result = raw_syscall3(
        SYS_SPAWN,
        path.as_ptr() as u64,
        argv.as_ptr() as u64,
        envp.as_ptr() as u64,
    );
    if result < 0 {
        println!("m3a-spawn-from-thread-rust: spawn from {who} was refused");
    } else {
        println!("m3a-spawn-from-thread-rust: spawn from {who} returned {result}");
    }
}

/// `join` の後の `spawn`。無いパスを渡す（通れば `-ENOENT`、断られれば `-EAGAIN`。Linux は `-ENOSYS`）。**行の文言は
/// 結果に依らない**——Linux の参照と突き合わせるため。
fn try_spawn_after_join() {
    let path = b"/bin/none\0";
    let argv: [*const u8; 2] = [path.as_ptr(), std::ptr::null()];
    let envp: [*const u8; 1] = [std::ptr::null()];
    let _ = raw_syscall3(
        SYS_SPAWN,
        path.as_ptr() as u64,
        argv.as_ptr() as u64,
        envp.as_ptr() as u64,
    );
    println!("m3a-spawn-from-thread-rust: spawn from the main thread after the join was attempted");
}

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-spawn-from-thread-rust: main starts with {argument_count} argument(s)");
    let go = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&go);
    let handle = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        try_spawn("the thread");
    });
    try_spawn("the main thread while the thread is alive");
    go.store(true, Ordering::SeqCst);
    handle.join().unwrap_or(());
    // **`join` の後は断られない**（2026-10-16）。無いパスを渡すので、ZeikOS では `-ENOENT`（断りではない）、Linux では
    // `-ENOSYS` で、どちらも負の値になる。行は同じ文言で出し、断られなかったことはカーネルの側が数えて見る
    // （`verify_linux_programs`）。
    try_spawn_after_join();
    std::process::exit(argument_count as i32);
}
