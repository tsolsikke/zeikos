//! `m3a-exit-while-spinning-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが
//! `sched_yield` で回り続けている間に、主スレッドがプロセスを終える形**（M3a の 3-2。`ADR-0085` の決定 3）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作る。スレッドは 1 行出して旗を立て、以後ずっと `sched_yield` を打ち続ける。
//! 2. 主スレッドは旗が立つまで譲り、さらに 8 回譲ってから 1 行出し、`std::process::exit` で終わる（`exit_group`）。
//!    Linux はスレッドをその場で止める。ZeikOS は、主スレッドの `exit` が待たずに、残っているスレッドを `Finished` にして
//!    刈り取ってから終える（起動ログの `thread: 1 thread(s) of … were still runnable when the process ended` の行）。
//!    次のイメージの `clone` が通ることで、刈り取りが池を空けたことが分かる。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-while-spinning-rust: main starts with {argument_count} argument(s)");
    let printed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&printed);
    std::thread::spawn(move || {
        println!("m3a-exit-while-spinning-rust: the thread spins on sched_yield from now on");
        flag.store(true, Ordering::SeqCst);
        loop {
            std::thread::yield_now();
        }
    });
    while !printed.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    for _ in 0..8 {
        std::thread::yield_now();
    }
    println!("m3a-exit-while-spinning-rust: main exits while the thread is still runnable");
    std::process::exit(argument_count as i32);
}
