//! `m3a-exit-group-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが
//! `exit_group` でプロセスを終える形**（M3a の 3-1b。`ADR-0085` の Addendum）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作る。
//! 2. スレッドは 1 行出してから `std::process::exit(7)`（`exit_group(7)`）を打つ。Linux はプロセス全体を 7 で終える。
//!    ZeikOS は、印を主スレッドへ残してスレッドを終え、主スレッドが次のシステムコールの戻りで出て、プロセスが
//!    `Exited(7)` で終わる。
//! 3. 主スレッドは `sched_yield` を決まった回数（64 回）打つ。ZeikOS では、最初の譲りでスレッドが走って `exit_group` を
//!    打ち、その `sched_yield` の戻りでプロセスが終わる。**主スレッドのその後の行は、どちらの環境でも出ない。**
//!
//! # 主スレッドが譲るのは、スレッドが 1 行出した後（2026-10-20）
//!
//! **以前は、作ってすぐ譲り始めていた。** システムコールの数は 37〜40 に散った（残っていた 781 起動）。`clone` の戻りに
//! ティックが来ると、スレッドが先に `exit_group` まで進み、主スレッドは `pthread_create` の後始末の `rt_sigprocmask` で
//! 終わった（-1）。スレッドが 1 行出している間（`write` の戻り）にティックが来ると、主スレッドの譲りが 1 回増えた（+1）。
//!
//! **今は、決まった順で受け渡す。** すべて、システムコールを打たずに回って待つ（`m3a-exit-group-spin-rust` と同じ形）。
//!
//! 1. スレッドは、主スレッドが `spawn` から戻るのを待つ。
//! 2. スレッドは 1 行出してから「出した」フラグを立て、主スレッドの「進め」のフラグを待つ。
//! 3. 主スレッドは「出した」を見てから「進め」を立て、譲り始める。最初の譲りでスレッドが `exit_group` を打つ。
//!
//! 主スレッドの譲りは 1 回に決まる。ティックで変わりうるのは、スレッドが「進め」を見てから `exit_group` を打つまでの、
//! Rust と musl の終わりの処理の間だけである。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-group-rust: main starts with {argument_count} argument(s)");
    // 受け渡しのフラグ（モジュールの doc の「主スレッドが譲るのは、スレッドが 1 行出した後」）。
    let returned = Arc::new(AtomicBool::new(false));
    let printed = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let (returned_seen, printed_set, go_seen) = (Arc::clone(&returned), Arc::clone(&printed), Arc::clone(&go));
    let handle = std::thread::spawn(move || {
        while !returned_seen.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        println!("m3a-exit-group-rust: the thread calls exit_group(7) now");
        printed_set.store(true, Ordering::SeqCst);
        while !go_seen.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        std::process::exit(7);
    });
    returned.store(true, Ordering::SeqCst);
    while !printed.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    go.store(true, Ordering::SeqCst);
    for _ in 0..64 {
        std::thread::yield_now();
    }
    handle.join().unwrap_or(());
    println!("m3a-exit-group-rust: this line must not appear");
    std::process::exit(argument_count as i32);
}
