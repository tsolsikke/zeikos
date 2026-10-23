//! `m3a-exit-group-thread-spins-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッド
//! B がシステムコールを打たずに Ring 3 で回り続ける間に、スレッド A が `exit_group` を打つ形**（M3a。2026-10-18。ZEIKOS-242）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、スレッド A を作り、A の closure が始まるのを待ってからスレッド B を作る。B を作って spawn から
//!    戻ったら、そのことを示すフラグを立てて、B を `join` する（futex で眠る）。
//! 2. B は 1 行出してから、回り始めたことを示すフラグを立て、**その後はずっと回る**（`spin_loop`。システムコールを打たない）。
//!    B の起動の後始末（シグナルの代わりのスタックなど）のシステムコールは、フラグより前に済んでいる。
//! 3. A は、主スレッドのフラグと B のフラグの両方を待ってから、1 行出して `std::process::exit(7)`（`exit_group(7)`）を打つ。
//!    **このとき B は Ring 3 で回っている。** ZeikOS では、A が終わった後に B が選ばれ（輪の順で A の次が B）、Ring 3 を割り込んだ
//!    ティックで終わる（`thread: … was in ring 3 at a tick after the process ended; it is finished`）。主スレッドは、終わりの印が
//!    futex の待ちを起こした後、システムコールの戻りで印を見て出る。プロセスは `Exited(7)`。
//!
//! 行の順（主・B・A）は、フラグで決まる。Linux の参照と突き合わせられるのはそのためである。
//!
//! # 主スレッドは、A の closure が始まってから B を作る（2026-10-23）
//!
//! **以前は、A を作ってすぐ B を作っていた。** Rust のスレッドの立ち上げ（closure の前）は malloc のロック（musl の
//! `__malloc_lock`）を取る。主スレッドが B の `spawn` の中で malloc している間にティックが来ると、立ち上げ中の A がそのロックで
//! 眠り、主スレッドが起こした（`futex:` の行が「眠り 2・起こし 1」になる。約 36 回の起動で 2 回）。`m3a-channel-tls-rust` などで
//! 直したのと同じ形である。今は、A が closure の頭で「始まった」フラグを立て、主スレッドはシステムコールを打たずにそれを
//! 待ってから B を作る。眠るのは主スレッドの `join` の 1 回だけになる。
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
        "m3a-exit-group-thread-spins-rust: main starts with {argument_count} argument(s) and joins the spinning thread"
    );
    // 主スレッドが B の spawn から戻ったことを示すフラグと、B が回り始めたことを示すフラグ。
    let returned = Arc::new(AtomicBool::new(false));
    let spinning = Arc::new(AtomicBool::new(false));
    // A の closure が始まったことを示すフラグ（モジュールの doc の「主スレッドは、A の closure が始まってから B を作る」）。
    let started = Arc::new(AtomicBool::new(false));
    let (returned_seen, spinning_seen, started_set) =
        (Arc::clone(&returned), Arc::clone(&spinning), Arc::clone(&started));
    let _ender = std::thread::spawn(move || {
        started_set.store(true, Ordering::SeqCst);
        while !(returned_seen.load(Ordering::SeqCst) && spinning_seen.load(Ordering::SeqCst)) {
            std::hint::spin_loop();
        }
        println!("m3a-exit-group-thread-spins-rust: thread A calls exit_group(7) now");
        std::process::exit(7);
    });
    while !started.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    let spinning_set = Arc::clone(&spinning);
    let spinner = std::thread::spawn(move || {
        println!("m3a-exit-group-thread-spins-rust: thread B spins forever from now on");
        spinning_set.store(true, Ordering::SeqCst);
        loop {
            std::hint::spin_loop();
        }
    });
    returned.store(true, Ordering::SeqCst);
    spinner.join().unwrap_or(());
    println!("m3a-exit-group-thread-spins-rust: this line must not appear");
    std::process::exit(argument_count as i32);
}
