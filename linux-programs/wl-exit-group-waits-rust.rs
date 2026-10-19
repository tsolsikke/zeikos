//! `wl-exit-group-waits-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**主スレッドが
//! パイプの `read` で、スレッド A が `epoll_pwait` で眠っている間に、スレッド B が `exit_group` でプロセスを終える形**
//! （2026-10-20。`ADR-0085` の Addendum）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、合図用の eventfd と、誰も書かない eventfd を見張る epoll を作ってから、スレッド A と B を作る。
//!    そのあと、標準入力（fd 0）を `read` する。**標準入力は、書き手が開いたままで何も書かないパイプである**——ZeikOS では
//!    起動時の確かめがパイプを作って読み端を据え（シェルの `|` と同じ道。ZeikOS は Linux 向けの `pipe`・`pipe2` をまだ持たない）、
//!    Linux では参照を取る道具が `sleep 3 |` で渡す。だから読みは眠る。
//! 2. スレッド A は、合図用の eventfd へ書いてから、誰も書かない eventfd を見張る epoll で `epoll_pwait(-1)` する（眠る）。
//! 3. スレッド B は、合図用の eventfd を読んで A が待ちへ入る所まで来たことを確かめ、さらに 300 ms 眠ってから 1 行出し、
//!    `std::process::exit(3)`（`exit_group(3)`）を打つ。Linux はプロセス全体を 3 で終える。
//! 4. **主スレッドの `read` の後の行と、A の `epoll_pwait` の後の行は、どちらの環境でも出ない。**
//!
//! # 眠ってから打つ順
//!
//! B が打つ前に、主スレッドと A が眠っていることは、次の 2 つで作る。A が待ちの直前に合図を書き、B はそれを読んでから眠る。
//! B が眠る 300 ms の間に走れるのは主スレッドと A だけで（ZeikOS はユーザーのタスクを全部 BSP で走らせる。`ADR-0085` の決定 5）、
//! どちらも数回のシステムコールで眠る。**ZeikOS の起動時の確かめは、眠った待ちを起こした数を種類ごとに見る**（`process-end:`
//! の行）。眠る前に B が打った回は、起こした数が 0 になって判定が落ちる（順のウィンドウが開いたことを見逃さない）。
//!
//! 終了ステータスは 3。Linux でも同じ行を出す（`linux-programs/reference/wl-exit-group-waits-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::thread;
use std::time::Duration;

const NAME: &str = "wl-exit-group-waits-rust";
const EPOLLIN: u32 = 0x001;
const EPOLL_CTL_ADD: i32 = 1;

extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn epoll_create1(flags: i32) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut u8) -> i32;
    fn epoll_pwait(epfd: i32, events: *mut u8, maxevents: i32, timeout: i32, sigmask: *const u8) -> i32;
    fn eventfd(initval: u32, flags: i32) -> i32;
}

fn main() {
    let argument_count = std::env::args().count();
    println!(
        "{NAME}: main starts with {argument_count} argument(s); the main thread sleeps in a pipe read, one thread in \
         epoll_pwait, and a third thread ends the process"
    );
    // SAFETY: 以下の FFI はどれも、この関数の中で生きている値だけを渡す。
    let (control, ep, efd) = unsafe {
        let control = eventfd(0, 0);
        let ep = epoll_create1(0);
        let efd = eventfd(0, 0);
        let mut event = [0u8; 12];
        event[..4].copy_from_slice(&EPOLLIN.to_le_bytes());
        epoll_ctl(ep, EPOLL_CTL_ADD, efd, event.as_mut_ptr());
        (control, ep, efd)
    };
    if control < 0 || ep < 0 || efd < 0 {
        println!("{NAME}: epoll_create1 or eventfd failed; exiting with 89");
        std::process::exit(89);
    }
    let _waiter = thread::spawn(move || {
        // SAFETY: 8 バイトの値を書くだけである。
        unsafe { write(control, 1u64.to_le_bytes().as_ptr(), 8) };
        let mut events = [0u8; 12];
        // SAFETY: `events` は 1 件分の大きさがある。
        unsafe { epoll_pwait(ep, events.as_mut_ptr(), 1, -1, std::ptr::null()) };
        println!("{NAME}: the epoll waiter returned; this line must not appear");
    });
    let _ender = thread::spawn(move || {
        let mut value = [0u8; 8];
        // SAFETY: `value` は 8 バイトである。
        unsafe { read(control, value.as_mut_ptr(), 8) };
        thread::sleep(Duration::from_millis(300));
        println!("{NAME}: the third thread calls exit_group(3) while the others sleep");
        std::process::exit(3);
    });
    let mut buf = [0u8; 1];
    // SAFETY: `buf` は 1 バイトである。標準入力の書き端は相手が持ったままなので、EOF にならずに眠る。
    unsafe { read(0, buf.as_mut_ptr(), 1) };
    println!("{NAME}: the pipe read returned; this line must not appear");
    std::process::exit(argument_count as i32);
}
