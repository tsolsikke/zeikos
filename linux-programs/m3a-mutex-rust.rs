//! `m3a-mutex-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが必ず futex で
//! 待つ形**（M3a の `clone` と futex の最小。`ADR-0081` の Addendum・`ADR-0085`）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが `std::sync::Mutex` を取ったまま、スレッドを作る。
//! 2. スレッドは同じ `Mutex` を取ろうとして、必ず futex で待つ（`FUTEX_WAIT_BITSET`。主スレッドが持っているので取れない）。
//! 3. 主スレッドは `sched_yield` を決まった回数（64 回）打ち、1 行出してから `Mutex` を解く（`FUTEX_WAKE`）。
//! 4. スレッドは `Mutex` を取って 1 行出し、戻る。主スレッドは `join` して（スレッドがまだ終わっていなければ、ここでも
//!    futex で待つ）、戻り値を出す。
//! 5. 終了の状態は引数の数（`exit_group`）。`m1-rust`・`m3a-rust` と同じ形で、起動時の確かめが引数 4 つで起こす。
//!
//! # 出力の順は決まっている
//!
//! `main starts` → `unlocking`（主スレッド。解く前）→ `the thread took the lock`（スレッド。解かれた後）→ `joined`
//! （主スレッド。`join` の後）。**Linux でも ZeikOS でも同じ順になる**——スレッドの行は解かれるまで出ず、`joined` は
//! スレッドが戻るまで出ない。ZeikOS では、スレッドの `FUTEX_WAIT` → 主スレッドの `FUTEX_WAKE` → スレッドの終わり、の順を
//! 毎回通る（眠った回数は起動ログの `futex:` の行に出る）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::{Arc, Mutex};

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-mutex-rust: main starts with {argument_count} argument(s)");
    let lock = Arc::new(Mutex::new(0u32));
    let held = lock.lock().expect("the lock is fresh");
    let shared = Arc::clone(&lock);
    let handle = std::thread::spawn(move || {
        let mut value = shared.lock().expect("the main thread does not poison the lock");
        *value += 7;
        println!("m3a-mutex-rust: the thread took the lock and set the value to {}", *value);
        *value
    });
    for _ in 0..64 {
        std::thread::yield_now();
    }
    println!("m3a-mutex-rust: unlocking after 64 yields");
    drop(held);
    let value = handle.join().expect("the thread did not panic");
    println!(
        "m3a-mutex-rust: joined, the thread returned {value}; the value is now {}",
        *lock.lock().expect("the thread does not poison the lock")
    );
    std::process::exit(argument_count as i32);
}
