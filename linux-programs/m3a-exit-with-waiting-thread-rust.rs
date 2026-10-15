//! `m3a-exit-with-waiting-thread-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。
//! **終わらないスレッドがチャネルで待っている状態で主スレッドが終わり、プロセスが決めた終了コードで終わる形**
//! （スレッドの検証の2。`ADR-0085` の決定 3）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、チャネルを作り、スレッドを作る。スレッドは 1 行出して旗を立て、届かない値を `recv` で待ち続ける
//!    （musl の futex で眠る）。
//! 2. 主スレッドは旗が立つまで譲り、さらに 8 回譲ってから 1 行出し、送り手を持ったまま `std::process::exit(5)` で終わる。
//!    Linux はスレッドをその場で止めて 5 で終える。ZeikOS は、主スレッドの `exit` が待たずに、眠っているスレッドを
//!    `Finished` にして reap してから終える（起動ログの `thread: 1 thread(s) of … were still runnable when the process ended
//!    (1 of them parked inside the kernel at a wait)` の行）。終わり方は `Exited(5)`。
//! 3. **スレッドの `recv` の後の行は、どちらの環境でも出ない。**
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-with-waiting-thread-rust: main starts with {argument_count} argument(s)");
    let (sender, receiver) = mpsc::channel::<u32>();
    let printed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&printed);
    std::thread::spawn(move || {
        println!("m3a-exit-with-waiting-thread-rust: the thread waits on the channel from now on");
        flag.store(true, Ordering::SeqCst);
        let value = receiver.recv();
        println!("m3a-exit-with-waiting-thread-rust: this line must not appear ({value:?})");
    });
    while !printed.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    for _ in 0..8 {
        std::thread::yield_now();
    }
    println!("m3a-exit-with-waiting-thread-rust: main exits with 5 while the thread is waiting on the channel");
    // 送り手は閉じない——閉じるとスレッドの `recv` が `Err` で返ってしまう。
    std::mem::forget(sender);
    std::process::exit(5);
}
