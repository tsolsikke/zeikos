//! `m3a-exit-group-main-spins-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**主スレッドが
//! システムコールを打たずに Ring 3 で回り続ける間に、スレッドが `exit_group` を打つ形**（M3a。2026-10-17。ZEIKOS-242）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッドを作る。spawn から戻ったことを示すフラグを立て、**その後はずっと回る**（`spin_loop`。
//!    システムコールを打たない）。
//! 2. スレッドは、主スレッドが spawn から戻ったフラグを待ってから（`m3a-exit-group-spin-rust` の 2026-10-17 の形と同じ理由。
//!    主スレッドの `rt_sigprocmask` より先に `exit_group` へ進まない）、1 行出して `std::process::exit(7)`（`exit_group(7)`）
//!    を打つ。
//! 3. **主スレッドは、Ring 3 を割り込んだティックで終わる。** プロセスは `Exited(7)`。ZeikOS がティックで終わりの印を見なければ、
//!    主スレッドは回り続け、起動はこのイメージで止まる（破壊テスト `tick-ignores-process-end`）。Linux では、`exit_group` が主
//!    スレッドも終わらせる。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    let argument_count = std::env::args().count();
    println!(
        "m3a-exit-group-main-spins-rust: main starts with {argument_count} argument(s) and then spins forever"
    );
    // 主スレッドが spawn から戻ったことを示すフラグ。
    let returned = Arc::new(AtomicBool::new(false));
    let returned_seen = Arc::clone(&returned);
    let _handle = std::thread::spawn(move || {
        while !returned_seen.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        println!("m3a-exit-group-main-spins-rust: the thread calls exit_group(7) now");
        std::process::exit(7);
    });
    returned.store(true, Ordering::SeqCst);
    loop {
        std::hint::spin_loop();
    }
}
