//! `m3a-nested-clone-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが
//! さらにスレッドを作る形**（M3a の 3-2。`ADR-0085` の Addendum——スレッドからの `clone` は受ける）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、外のスレッドを作る。
//! 2. 外のスレッドは 1 行出してから内のスレッドを作り、`join` する。内のスレッドは 1 行出して 3 を返す。
//! 3. 主スレッドは `sched_yield` を 64 回打ってから外のスレッドを `join` し、戻り値（3 + 4）を出して、引数の数で終わる。
//!    ZeikOS では、内のスレッドの `thread: task 7 started … like its parent task 6` の行が、池のタスクを親にした `clone` を示す。
//!
//! # 出力の順は決まっている
//!
//! `main starts` → `the outer thread starts an inner thread`（内を作る前）→ `the inner thread runs` → `joined`
//! （外が内を `join` してから戻るので、主スレッドの `join` はその後）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-nested-clone-rust: main starts with {argument_count} argument(s)");
    let outer = std::thread::spawn(|| {
        println!("m3a-nested-clone-rust: the outer thread starts an inner thread");
        let inner = std::thread::spawn(|| {
            println!("m3a-nested-clone-rust: the inner thread runs");
            3u32
        });
        inner.join().unwrap_or(0) + 4
    });
    for _ in 0..64 {
        std::thread::yield_now();
    }
    let value = outer.join().unwrap_or(0);
    println!("m3a-nested-clone-rust: joined, the outer thread returned {value}");
    std::process::exit(argument_count as i32);
}
