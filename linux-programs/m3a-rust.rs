//! `m3a-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドを 1 つ作って
//! 終わる**（M3a の `clone`。`ADR-0085`）。
//!
//! # 何のためのものか
//!
//! `clone` の入口と、スレッドが走ること、スレッドの `exit` が `CLONE_CHILD_CLEARTID` の番地に 0 を書くことを、musl の本物の
//! 道（`pthread_create` → `clone(0x7d0f00, …)`、TLS、`__thread_list_lock`）で確かめる。**Linux 上でも ZeikOS 上でも、同じ
//! 出力と同じ終了の状態になる**ことを、控えた参照（`linux-programs/reference/`）と突き合わせる。
//!
//! # futex が無い段階でも通る形
//!
//! musl の `pthread_join` は、スレッドがもう終わっていれば futex を打たない（`detach_state` が `DT_EXITED`、
//! `__thread_list_lock` がカーネルの書いた 0）。**そこで主スレッドは、`join` の前に `sched_yield` を決まった回数（64 回）
//! 打つ。** ZeikOS では、譲るたびに輪で池のタスクが選ばれるので、スレッドは最初の数回で終わる。終わっていなければ `join` が
//! futex で待つ場面になり、プロセスが 137 で終わる——通らないことが隠れない。**回数を旗で変えない**——システムコールの
//! 数が起動ごとに同じになり、`spawn` の結果の行（起動ログの参照に入る）が揺れない。**stdout のロックを持ったまま待たない**
//! （持つと、スレッドが書けずに止まる。Linux で実測した）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出す。
//! 2. スレッドを作る。スレッドは 1 行出して旗を立て、7 を返す（`mmap` のスタック、`mprotect` の見張り、`sigaltstack`、
//!    `rt_sigprocmask`、`gettid`、`write`、`munmap`、`futex(FUTEX_WAKE)`、`exit` を打つ。2026-10-14 の Linux 上の `strace`）。
//! 3. 主スレッドは 64 回譲り、`join` して戻り値と旗を出す。**譲りの後に行を出さない**——Linux では 64 回の譲りの間に
//!    スレッドがまだ走っていないことがあり、行の順が決まらない。スレッドの行は `main starts` と `joined` の間に必ず入る。
//! 4. 終了の状態は引数の数（`exit_group`）。`m1-rust` と同じ形で、起動時の確かめが引数 4 つで起こす。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};

static DONE: AtomicBool = AtomicBool::new(false);

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-rust: main starts with {argument_count} argument(s)");
    let handle = std::thread::spawn(|| {
        println!("m3a-rust: the thread runs");
        DONE.store(true, Ordering::SeqCst);
        7u32
    });
    for _ in 0..64 {
        std::thread::yield_now();
    }
    let value = handle.join().expect("the thread did not panic");
    println!(
        "m3a-rust: joined, the thread returned {value} and had set the flag = {}",
        DONE.load(Ordering::SeqCst)
    );
    std::process::exit(argument_count as i32);
}
