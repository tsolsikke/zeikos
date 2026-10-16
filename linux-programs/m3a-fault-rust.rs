//! `m3a-fault-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドが Ring 3 で
//! 例外（ページフォルト）を起こして、プロセスが終わる形**（M3a の 3-1b。`ADR-0085` の Addendum）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作る。
//! 2. スレッドは 1 行出してから、アドレス 8 へ書く（`black_box` で定数の畳み込みを防ぐ。アドレス 8 は写されていないので #PF になる）。
//!    Linux は SIGSEGV でプロセスを終える（終了の状態 139）。ZeikOS は、スレッドを終えてプロセスを `Folded(14)` で終える
//!    （主スレッドが同じ例外で終わるときと同じ符号）。
//! 3. 主スレッドは `sched_yield` を決まった回数（64 回）打つ。ZeikOS では、最初の譲りでスレッドが走って落ち、次の
//!    `sched_yield` の戻りでプロセスが終わる。**主スレッドのその後の行は、どちらの環境でも出ない。**
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-fault-rust: main starts with {argument_count} argument(s)");
    let handle = std::thread::spawn(|| {
        println!("m3a-fault-rust: the thread writes to address 8 now");
        let bad = std::hint::black_box(8usize) as *mut u32;
        // SAFETY: 書けないアドレスへわざと書く（このプログラムの目的）。プロセスはここで終わる。
        unsafe { bad.write_volatile(1) };
        0u32
    });
    for _ in 0..64 {
        std::thread::yield_now();
    }
    let value = handle.join().unwrap_or(1);
    println!("m3a-fault-rust: this line must not appear ({value})");
    std::process::exit(argument_count as i32);
}
