//! `m3a-exit-group-spin-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**主スレッドが
//! Ring 3 で回っている間にスレッドが `exit_group` を打ち、その後に主スレッドがシステムコールへ入る形**（M3a の 3-2。
//! 3-1b の「入口で印を見る」道を、ティック任せにせず毎回通す）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作って、フラグが立つまでシステムコールを打たずに回る（`spin_loop`）。
//!    ZeikOS では単一の CPU なので、スレッドはティックが主スレッドを止めたときに走る。
//! 2. スレッドは 1 行出し、フラグを立ててから `std::process::exit(7)`（`exit_group(7)`）を打つ。主スレッドは眠っていないので、
//!    起こす相手は無い（`futex:` の行の roused は 0）。
//! 3. 主スレッドはフラグを見て `join` する。`futex` の入口で印を見て、実行せずに出る——プロセスは `Exited(7)`。
//!    入口で見なければ、`FUTEX_WAIT` の行き詰まりの判定が 137 で上書きする（3-1b で実測した形。`docs/troubleshooting.md`）。
//! 4. **主スレッドの `join` の後の行は、どちらの環境でも出ない。**
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-group-spin-rust: main starts with {argument_count} argument(s) and spins without system calls");
    let flagged = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&flagged);
    let handle = std::thread::spawn(move || {
        println!("m3a-exit-group-spin-rust: the thread calls exit_group(7) now");
        flag.store(true, Ordering::SeqCst);
        std::process::exit(7);
    });
    while !flagged.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    handle.join().unwrap_or(());
    println!("m3a-exit-group-spin-rust: this line must not appear");
    std::process::exit(argument_count as i32);
}
