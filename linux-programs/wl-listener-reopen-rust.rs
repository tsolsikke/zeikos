//! `wl-listener-reopen-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**受け付ける前の
//! 接続を残して listener を閉じた後、別の名前で作った listener に、その古い接続が見えないこと**（2026-10-23。`ADR-0064` の
//! Addendum）。
//!
//! # 何をするか
//!
//! 1. `old.sock` を `bind`・`listen` し、クライアントが `connect` する。`accept` せずに listener を閉じる。
//! 2. `new.sock` で listener を作り直す（ZeikOS では、閉じた listener と同じ欄を取る）。待たない印を立てる。
//! 3. `poll`（timeout 0）で、新しい listener が「読める」にならないことを見る。
//! 4. `accept` が `EAGAIN`（`WouldBlock`）で何も返さないことを見る。
//! 5. 新しいクライアントが `new.sock` へ繋ぎ、1 バイト書く。`accept` が返す接続から、そのバイトが読めることを見る。
//!
//! 古いクライアントが何を見るかは行に出さない——Linux は `ECONNRESET`、ZeikOS は EOF で、ここでは比べない。
//!
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-listener-reopen-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

const NAME: &str = "wl-listener-reopen-rust";
const DIR: &str = "/tmp/seinas";
const OLD: &str = "/tmp/seinas/listener-old.sock";
const NEW: &str = "/tmp/seinas/listener-new.sock";
const POLLIN: i16 = 1;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let _ = std::fs::create_dir_all(DIR);
    let _ = std::fs::remove_file(OLD);
    let _ = std::fs::remove_file(NEW);

    let old = UnixListener::bind(OLD).unwrap_or_else(|error| fail(&format!("bind old failed: {error}"), 90));
    let waiting = UnixStream::connect(OLD).unwrap_or_else(|error| fail(&format!("connect old failed: {error}"), 90));
    drop(old);

    let new = UnixListener::bind(NEW).unwrap_or_else(|error| fail(&format!("bind new failed: {error}"), 91));
    new.set_nonblocking(true).unwrap_or_else(|error| fail(&format!("nonblocking failed: {error}"), 91));

    let mut slot = PollFd { fd: new.as_raw_fd(), events: POLLIN, revents: 0 };
    // SAFETY: `slot` は 1 つの `struct pollfd` である。待たない（timeout 0）。
    let ready = unsafe { poll(&mut slot, 1, 0) };
    println!(
        "{NAME}: the new listener did not look readable after the old one closed with a waiting connection = {}",
        ready == 0 && slot.revents & POLLIN == 0
    );

    let accepted = new.accept();
    println!(
        "{NAME}: accept on the new listener found nothing (EAGAIN) = {}",
        matches!(&accepted, Err(error) if error.kind() == ErrorKind::WouldBlock)
    );

    let mut fresh = UnixStream::connect(NEW).unwrap_or_else(|error| fail(&format!("connect new failed: {error}"), 92));
    fresh.write_all(b"k").unwrap_or_else(|error| fail(&format!("write failed: {error}"), 92));
    let (mut server, _) = new.accept().unwrap_or_else(|error| fail(&format!("accept new failed: {error}"), 93));
    server.set_nonblocking(false).unwrap_or_else(|error| fail(&format!("blocking failed: {error}"), 93));
    let mut byte = [0u8; 1];
    let got = server.read(&mut byte).unwrap_or(0);
    println!(
        "{NAME}: accept then returned the new client's connection (its byte came through) = {}",
        got == 1 && byte[0] == b'k'
    );

    drop(server);
    drop(fresh);
    drop(waiting);
    drop(new);
    let _ = std::fs::remove_file(OLD);
    let _ = std::fs::remove_file(NEW);
    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
