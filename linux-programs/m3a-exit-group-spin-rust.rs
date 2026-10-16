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
//! 3. 主スレッドはフラグを見た後、**システムコールを打たずに、もう少し回ってから** `join` する（下の「フラグの後に回る長さ」）。
//!    `join` の `futex` の入口で印を見て、実行せずに出る——プロセスは `Exited(7)`。
//!    入口で見なければ、`FUTEX_WAIT` の行き詰まりの判定が 137 で上書きする（3-1b で実測した形。`docs/troubleshooting.md`）。
//! 4. **主スレッドの `join` の後の行は、どちらの環境でも出ない。**
//!
//! # フラグの後に回る長さ（2026-10-17）
//!
//! **スレッドがフラグを立ててから `exit_group` に入るまでの間（`process::exit` の後始末。数マイクロ秒）に
//! ティックが来ると、主スレッドが先に `join` の `futex` で眠っていた**（-smp 1 で 247 回中 3 回。`futex:` の行が
//! slept 1・woke 0・roused 1 になり、入口で印を見る道を通らない）。**フラグを見た後にすぐ `join` せず、ティックの
//! 境を 2 回以上またぐ長さを回ってから `join` する。**
//!
//! - 長さは、スレッドを作る前に `thread::sleep(30 ms)`（`nanosleep`）が実際にかかった TSC の数である。ZeikOS の
//!   ティックは 10 ms（100 Hz）で、`nanosleep` は 30 ms を 3 ティックへ切り上げ、「今のティックの数 + 3」まで眠る。
//!   眠る長さは 2 ティックより長く、3 ティック（と起きるまでの遅れ）以下である。**回る長さも 2 ティックより長いので、
//!   回る間にティックの境を少なくとも 2 回またぐ。**
//! - ZeikOS のスケジューラは、ティックごとに、同じ CPU の輪の次の走れるタスクへ切り替える。このイメージが走る間、
//!   CPU 0 で選ばれうるのは主スレッドとスレッドだけである（ワーカーは止まり、BSP のアイドルは主スレッドが待つときの
//!   落ち先でしかない。2026-10-17 に一時的な計測で、選ばれた先が 2 本だけであることを確かめた）。**だから、主スレッドが
//!   フラグを見た時にスレッドが `exit_group` の手前に居ても、次のティックでスレッドへ戻り、スレッドが先に
//!   `exit_group` へ入る。**
//! - Linux でも長さは同じ形で測れる（`rdtsc` と `nanosleep`）。出力と終了ステータスは変わらない。
//! - **別の道が 1 つある**（-smp 1 で約 7%）。`clone` の直後にティックが来ると、スレッドが先に走って `exit_group` まで
//!   進み、主スレッドは `pthread_create` の後始末の `rt_sigprocmask` の入口で終わる（`join` にも `futex` にも届かない。
//!   システムコールの数が 1 本少ない）。こちらも `futex` では眠らない。2026-10-17 に一時的な計測で、システムコールの
//!   並びを見て確かめた。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// TSC を読む（システムコールを打たない）。
fn tsc() -> u64 {
    // SAFETY: `rdtsc` は引数もメモリも触らない。ZeikOS も Linux も CR4.TSD を立てておらず、Ring 3 で読める。
    #[allow(unused_unsafe)]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
}

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-exit-group-spin-rust: main starts with {argument_count} argument(s) and spins without system calls");
    // フラグの後に回る長さ（モジュールの doc の「フラグの後に回る長さ」）。スレッドを作る前に測る。
    let before_sleep = tsc();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let spin_after_flag = tsc().wrapping_sub(before_sleep);
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
    let flag_seen = tsc();
    while tsc().wrapping_sub(flag_seen) < spin_after_flag {
        std::hint::spin_loop();
    }
    handle.join().unwrap_or(());
    println!("m3a-exit-group-spin-rust: this line must not appear");
    std::process::exit(argument_count as i32);
}
