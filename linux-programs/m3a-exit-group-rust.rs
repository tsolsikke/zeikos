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
//!    打ち、次の `sched_yield` の戻りでプロセスが終わる。**主スレッドのその後の行は、どちらの環境でも出ない。**
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-group-rust: main starts with {argument_count} argument(s)");
    let handle = std::thread::spawn(|| {
        println!("m3a-exit-group-rust: the thread calls exit_group(7) now");
        std::process::exit(7);
    });
    for _ in 0..64 {
        std::thread::yield_now();
    }
    handle.join().unwrap_or(());
    println!("m3a-exit-group-rust: this line must not appear");
    std::process::exit(argument_count as i32);
}
