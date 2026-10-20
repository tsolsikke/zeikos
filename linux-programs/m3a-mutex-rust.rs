//! `m3a-mutex-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが必ず futex で
//! 待つ形**（M3a の `clone` と futex の最小。`ADR-0081` の Addendum・`ADR-0085`）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが `std::sync::Mutex` を取ったまま、スレッドを作る。
//! 2. スレッドは同じ `Mutex` を取ろうとして、必ず futex で待つ（`FUTEX_WAIT_BITSET`。主スレッドが持っているので取れない）。
//! 3. 主スレッドは `sched_yield` を決まった回数（64 回）打ち、1 行出してから `Mutex` を解く（`FUTEX_WAKE`）。
//! 4. スレッドは `Mutex` を取って 1 行出し、戻る。主スレッドは `sched_yield` をもう 64 回打ってから `join` し、戻り値を出す。
//! 5. 終了ステータスは引数の数（`exit_group`）。`m1-rust`・`m3a-rust` と同じ形で、起動時の確かめが引数 4 つで起こす。
//!
//! # 出力の順は決まっている
//!
//! `main starts` → `unlocking`（主スレッド。解く前）→ `the thread took the lock`（スレッド。解かれた後）→ `joined`
//! （主スレッド。`join` の後）。**Linux でも ZeikOS でも同じ順になる**——スレッドの行は解かれるまで出ず、`joined` は
//! スレッドが戻るまで出ない。ZeikOS では、スレッドの `FUTEX_WAIT` → 主スレッドの `FUTEX_WAKE` → スレッドの終わり、の順を
//! 毎回通る（眠った回数は起動ログの `futex:` の行に出る）。
//!
//! # join は眠らない（2026-10-20）
//!
//! **以前は、解いてすぐ `join` していた。** そのため、眠った回数がティックの位置で 1・2・3 に揺れた（残っていた 785 起動で
//! 31・748・6 回）。`FUTEX_WAKE` の戻りにティックが来ると、スレッドが先に終わり、`join` が眠らなかった（1）。3 は、musl の
//! 作りから読むと、スレッドが `pthread_exit` で `join` を起こした後、`exit` を打つまでの間にティックが来た回である——起きた
//! 主スレッドが `__tl_sync` でスレッドの一覧のロックを待って、もう 1 回眠る（musl は `clone` の clear-tid のアドレスにこの
//! ロックを渡すので、スレッドが `exit` を打つまで放されない）。
//!
//! **解いた後、決まった回数（64 回）譲ってから `join` する。** 起こされたスレッドは最初の譲りで走り、`exit` まで進む
//! （途中でティックが来ても、主スレッドの次の譲りで戻る）。`join` を打つときにはスレッドが終わり切っているので、`join` も
//! ロックの待ちも眠らない。眠るのはスレッドの `Mutex` の 1 回だけになる。
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
    // スレッドが終わり切るまで譲る（モジュールの doc の「join は眠らない」）。
    for _ in 0..64 {
        std::thread::yield_now();
    }
    let value = handle.join().expect("the thread did not panic");
    println!(
        "m3a-mutex-rust: joined, the thread returned {value}; the value is now {}",
        *lock.lock().expect("the thread does not poison the lock")
    );
    std::process::exit(argument_count as i32);
}
