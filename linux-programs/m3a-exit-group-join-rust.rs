//! `m3a-exit-group-join-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**主スレッドが
//! `join` の futex で眠っている間に、スレッドが `exit_group` でプロセスを終える形**（M3a の 3-1b。`ADR-0085` の Addendum）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作って、譲らずにすぐ `join` する（スレッドはまだ走っていないので、`join` は futex で眠る）。
//! 2. スレッドは 1 行出してから `std::process::exit(7)`（`exit_group(7)`）を打つ。Linux はプロセス全体を 7 で終える。
//!    ZeikOS は、印を立てたスレッドが同じスロットの futex の待ちを全部起こし、起きた主スレッドが `futex` の戻りで印を見て出る。
//!    **起こさなければ、主スレッドは永久に眠り、プロセスは終わらない**（この像が起動のたびに通る道である）。
//! 3. **主スレッドの `join` の後の行は、どちらの環境でも出ない。**
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスク像の
//! `/bin/linux` に置く。

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-group-join-rust: main starts with {argument_count} argument(s) and joins at once");
    let handle = std::thread::spawn(|| {
        println!("m3a-exit-group-join-rust: the thread calls exit_group(7) now");
        std::process::exit(7);
    });
    handle.join().unwrap_or(());
    println!("m3a-exit-group-join-rust: this line must not appear");
    std::process::exit(argument_count as i32);
}
