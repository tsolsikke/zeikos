//! `m3a-nested-clone-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが
//! さらにスレッドを作る形**（M3a の 3-2。`ADR-0085` の Addendum——スレッドからの `clone` は受ける）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、外のスレッドを作る。
//! 2. 外のスレッドは、主スレッドが `spawn` から戻るのを待ってから 1 行出し、内のスレッドを作る。`sched_yield` を 8 回打って
//!    から内のスレッドを `join` する。内のスレッドは、外のスレッドが `spawn` から戻るのを待ってから 1 行出して 3 を返す。
//! 3. 主スレッドは `sched_yield` を 64 回打ってから外のスレッドを `join` し、戻り値（3 + 4）を出して、引数の数で終わる。
//!    ZeikOS では、内のスレッドの `thread: task 7 started … like its parent task 6` の行が、池のタスクを親にした `clone` を示す。
//!
//! # 出力の順は決まっている
//!
//! `main starts` → `the outer thread starts an inner thread`（内を作る前）→ `the inner thread runs` → `joined`
//! （外が内を `join` してから戻るので、主スレッドの `join` はその後）。
//!
//! # 待つ所は決まっている（2026-10-20）
//!
//! **以前は、作ったスレッドがすぐ走れ、外のスレッドは内のスレッドをすぐ `join` していた。** システムコールの数は 119〜122 と
//! 124 に散った（残っていた 769 起動）。`clone` の戻りにティックが来ると、作られたスレッドが
//! 先に走った。内のスレッドが `pthread_exit` まで進むと、作った側が `pthread_create` の中で持っている musl のスレッドの一覧の
//! ロックを待って眠り、作った側がそれを起こした（+2）。内のスレッドが外の `join` より先に終わると、`join` が眠らなかった（-1）。
//!
//! **今は、作られたスレッドは、作った側が `spawn` から戻ったことを示すフラグを、システムコールを打たずに待つ**
//! （`m3a-exit-group-spin-rust` と同じ形）。ロックはそのときには放されている。**`join` はどちらも、決まった回数譲ってから
//! 打つ**——外のスレッドは 8 回、主スレッドは 64 回で、待たれる側はその間に `exit` まで進み終える（`m3a-mutex-rust` の
//! 「join は眠らない」と同じ形）。どの `join` も眠らない。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-nested-clone-rust: main starts with {argument_count} argument(s)");
    // 作った側が spawn から戻ったことを示すフラグ（モジュールの doc の「待つ所は決まっている」）。
    let main_returned = Arc::new(AtomicBool::new(false));
    let main_returned_seen = Arc::clone(&main_returned);
    let outer = std::thread::spawn(move || {
        while !main_returned_seen.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        println!("m3a-nested-clone-rust: the outer thread starts an inner thread");
        let outer_returned = Arc::new(AtomicBool::new(false));
        let outer_returned_seen = Arc::clone(&outer_returned);
        let inner = std::thread::spawn(move || {
            while !outer_returned_seen.load(Ordering::SeqCst) {
                std::hint::spin_loop();
            }
            println!("m3a-nested-clone-rust: the inner thread runs");
            3u32
        });
        outer_returned.store(true, Ordering::SeqCst);
        for _ in 0..8 {
            std::thread::yield_now();
        }
        inner.join().unwrap_or(0) + 4
    });
    main_returned.store(true, Ordering::SeqCst);
    for _ in 0..64 {
        std::thread::yield_now();
    }
    let value = outer.join().unwrap_or(0);
    println!("m3a-nested-clone-rust: joined, the outer thread returned {value}");
    std::process::exit(argument_count as i32);
}
