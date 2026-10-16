//! `m3a-exit-group-waiters-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**主スレッドと
//! 別のスレッドの両方が futex で眠っている間に、第 3 のスレッドが `exit_group` でプロセスを終える形**（M3a の 3-1b。
//! `ADR-0085` の Addendum）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが `std::sync::Mutex` を取ったまま、終えるスレッドを先に作り（池の添字 0）、次に待つスレッドを作る（添字 1）。
//! 2. 待つスレッドは 1 行出し、フラグを立ててから同じ `Mutex` を取ろうとして、必ず futex で眠る（主スレッドが持っている）。
//! 3. 主スレッドは待つスレッドを `join` して、futex で眠る（待つスレッドは戻らない）。
//! 4. 終えるスレッドはフラグが立つまで `sched_yield` で待ち、さらに 8 回譲ってから 1 行出し、`std::process::exit(7)`
//!    （`exit_group(7)`）を打つ。Linux はプロセス全体を 7 で終える。ZeikOS は、印を立てたスレッドが同じスロットの futex の
//!    待ちを 2 本とも起こす。**終えるスレッドの次に走るのは待つスレッドである**（池の巡回は添字の順で、主スレッドは
//!    その後）——起きた待つスレッドは `futex` の戻りで印を見て `Finished` になり、起きた主スレッドは `futex` の戻りで
//!    印を見て出る。
//! 5. **主スレッドの `join` の後の行は、どちらの環境でも出ない。**
//!
//! # 出力の順は決まっている
//!
//! `main starts` → `the waiter thread waits for the mutex now`（フラグを立てる前）→ `the ender thread calls exit_group(7) now`
//! （フラグを見てから）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-group-waiters-rust: main starts with {argument_count} argument(s)");
    let lock = Arc::new(Mutex::new(0u32));
    let held = lock.lock().expect("the lock is fresh");
    let printed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&printed);
    let ender = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        for _ in 0..8 {
            std::thread::yield_now();
        }
        println!("m3a-exit-group-waiters-rust: the ender thread calls exit_group(7) now");
        std::process::exit(7);
    });
    let shared = Arc::clone(&lock);
    let flag = Arc::clone(&printed);
    let waiter = std::thread::spawn(move || {
        println!("m3a-exit-group-waiters-rust: the waiter thread waits for the mutex now");
        flag.store(true, Ordering::SeqCst);
        let value = shared.lock().expect("the main thread does not poison the lock");
        println!("m3a-exit-group-waiters-rust: this line must not appear ({})", *value);
    });
    waiter.join().unwrap_or(());
    println!("m3a-exit-group-waiters-rust: this line must not appear");
    drop(held);
    ender.join().unwrap_or(());
    std::process::exit(argument_count as i32);
}
