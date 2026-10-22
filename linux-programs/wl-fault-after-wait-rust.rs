//! `wl-fault-after-wait-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**主スレッドが
//! パイプの `read` か `poll` で眠っている間に、別のスレッドが書き先のページを `munmap` するか、読むだけに `mprotect` する形**
//! （2026-10-22。`ADR-0063` の Addendum）。カーネルが眠る前に確かめた範囲へ、起きた後にそのまま書くと、Ring 0 のページフォルトで
//! 止まる。Linux は `-EFAULT` を返し、データは取らずに残す。
//!
//! # 何をするか
//!
//! 4 つの形を順に通す。どれも、主スレッドが 1 ページを `mmap` してから、見張り役のスレッドを作って待ちへ入る。
//!
//! 1. パイプの `read` の書き先を `munmap` する。
//! 2. パイプの `read` の書き先を、読むだけに `mprotect` する。
//! 3. `poll` の `struct pollfd` の並びを `munmap` する（ソケットが読めるのを待つ）。
//! 4. `poll` の並びを、読むだけに `mprotect` する。
//!
//! 見張り役は、主スレッドが眠ったことを確かめてからページを外し、その後で 1 バイト書いて主スレッドを起こす。主スレッドの
//! `read`・`poll` は `EFAULT` で戻る。その後、主スレッドが良い置き場で読み直し、書いた 1 バイトが残っていることを見る。
//!
//! # 眠ったことの確かめ（経過時間に頼らない）
//!
//! - Linux では `/proc/self/task/<tid>/stat` の状態が `S` で、`/proc/self/task/<tid>/syscall` の番号が `read`（0）か
//!   `poll`（7）であること。
//! - ZeikOS には `/proc` が無いので、`linux-socket-test` のビルドだけが持つ確かめ用の番号（`0x100A`。その fd が読めるのを
//!   待って眠っているタスクが居れば 1）で見る。
//!
//! どちらも、確かめるまで `sched_yield` で譲って見直す。見直しの回数に上限を置き、越えたら名指しの行を出して 91 で終わる。
//!
//! # 標準入出力
//!
//! **fd 0 と fd 1 は同じパイプの読み端と書き端である。** ZeikOS では起動時の確かめが据え（ZeikOS は Linux 向けの `pipe`・
//! `pipe2` をまだ持たない）、Linux では参照を取る道具が名前付きパイプを両方へ開いて渡す。行はどれも**標準エラーへ**出す
//! （標準出力はパイプなので）。終了ステータスは引数の数（4）。Linux でも同じ行を出す
//! （`linux-programs/reference/wl-fault-after-wait-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

const NAME: &str = "wl-fault-after-wait-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/fault-after-wait.sock";

const PAGE: usize = 4096;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const POLLIN: i16 = 1;
const EFAULT: i32 = 14;
const SYS_READ: i64 = 0;
const SYS_POLL: i64 = 7;
const SYS_GETTID: i64 = 186;
/// ZeikOS の確かめ用の番号（`linux-socket-test` のビルドだけが持つ。Linux では打たない）。
const ZEIKOS_SLEEPS_READING: i64 = 0x100A;
/// 眠ったことを見直す回数の上限。
const LOOKS: usize = 10_000;

extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn poll(fds: *mut u8, nfds: u64, timeout: i32) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn sched_yield() -> i32;
    fn syscall(number: i64, ...) -> i64;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i32 {
    // SAFETY: musl はスレッドごとの errno の置き場を返す。
    unsafe { *__errno_location() }
}

fn fail(what: &str, code: i32) -> ! {
    eprintln!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// ページの外し方。
#[derive(Clone, Copy)]
enum Removal {
    Unmap,
    ReadOnly,
}

impl Removal {
    /// 「〜された」の形（`was <これ>`）。
    fn done(self) -> &'static str {
        match self {
            Removal::Unmap => "unmapped",
            Removal::ReadOnly => "made read-only",
        }
    }

    /// 置き場に付ける形（`the <これ> buffer`）。
    fn adjective(self) -> &'static str {
        match self {
            Removal::Unmap => "unmapped",
            Removal::ReadOnly => "read-only",
        }
    }
}

/// 主スレッド（`tid`）が、`fd` が読めるのを待って眠っているか。
fn sleeps_reading(tid: i64, fd: i32, number: i64) -> bool {
    let base = format!("/proc/self/task/{tid}");
    match (fs::read_to_string(format!("{base}/stat")), fs::read_to_string(format!("{base}/syscall"))) {
        (Ok(stat), Ok(syscall)) => {
            // 状態は、`comm` を囲む括弧の後の最初の欄である。
            let state = stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next());
            let current = syscall.split_whitespace().next().and_then(|n| n.parse::<i64>().ok());
            state == Some('S') && current == Some(number)
        }
        // SAFETY: 確かめ用の番号は fd を 1 つ取り、ポインタを読まない。
        _ => (unsafe { syscall(ZEIKOS_SLEEPS_READING, fd as i64) }) == 1,
    }
}

/// 主スレッドが眠るまで譲って見直してから、`page` を外し、`writer` へ 1 バイト書いて起こす。眠ったことを確かめたかを返す。
fn remove_after_sleep(tid: i64, fd: i32, number: i64, page: usize, removal: Removal, writer: i32) -> bool {
    let mut slept = false;
    for _ in 0..LOOKS {
        if sleeps_reading(tid, fd, number) {
            slept = true;
            break;
        }
        // SAFETY: 引数を取らない。
        unsafe { sched_yield() };
    }
    // SAFETY: `page` は主スレッドが `mmap` した 1 ページで、主スレッドは待ちの中に居て触らない。
    let removed = unsafe {
        match removal {
            Removal::Unmap => munmap(page as *mut u8, PAGE),
            Removal::ReadOnly => mprotect(page as *mut u8, PAGE, PROT_READ),
        }
    };
    if removed != 0 {
        fail("removing the page failed", 92);
    }
    // SAFETY: 1 バイトを書くだけである。
    unsafe { write(writer, b"x".as_ptr(), 1) };
    slept
}

fn map_page() -> usize {
    // SAFETY: 無名の 1 ページを新しく作る。
    let page = unsafe {
        mmap(std::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
    };
    if page as isize == -1 {
        fail("mmap failed", 89);
    }
    page as usize
}

/// 読むだけにしたページは、次の形の前に外す。外したページは何もしない。
fn release_page(page: usize, removal: Removal) {
    if let Removal::ReadOnly = removal {
        // SAFETY: 主スレッドが `mmap` したページで、もう誰も使わない。
        unsafe { munmap(page as *mut u8, PAGE) };
    }
}

/// パイプの `read`（fd 0）で眠る間に、書き先を外す。
fn pipe_read(tid: i64, removal: Removal) {
    let page = map_page();
    let watcher = thread::spawn(move || remove_after_sleep(tid, 0, SYS_READ, page, removal, 1));
    // SAFETY: `page` は 1 ページの置き場である。眠っている間に外されるのを確かめる。
    let got = unsafe { read(0, page as *mut u8, 1) };
    let error = errno();
    let slept = watcher.join().unwrap_or(false);
    eprintln!(
        "{NAME}: the main thread slept in a pipe read before its buffer was {} = {slept}",
        removal.done()
    );
    eprintln!(
        "{NAME}: the pipe read into the {} buffer failed with EFAULT = {}",
        removal.adjective(),
        got == -1 && error == EFAULT
    );
    let mut byte = [0u8; 1];
    // SAFETY: `byte` は 1 バイトである。
    let again = unsafe { read(0, byte.as_mut_ptr(), 1) };
    eprintln!(
        "{NAME}: the byte stayed in the pipe and a later read took it = {}",
        again == 1 && byte[0] == b'x'
    );
    release_page(page, removal);
}

/// ソケットが読めるのを `poll` で待つ間に、`struct pollfd` の並びを外す。
fn poll_socket(tid: i64, removal: Removal, server: &mut UnixStream, client: &UnixStream) {
    let page = map_page();
    let fd = server.as_raw_fd();
    // SAFETY: `page` は書ける 1 ページで、`struct pollfd`（8 バイト）が入る。
    unsafe {
        let slot = page as *mut u8;
        slot.cast::<i32>().write(fd);
        slot.add(4).cast::<i16>().write(POLLIN);
        slot.add(6).cast::<i16>().write(0);
    }
    let writer = client.as_raw_fd();
    let watcher = thread::spawn(move || remove_after_sleep(tid, fd, SYS_POLL, page, removal, writer));
    // SAFETY: `page` は `struct pollfd` を 1 つ持つ。眠っている間に外されるのを確かめる。
    let got = unsafe { poll(page as *mut u8, 1, -1) };
    let error = errno();
    let slept = watcher.join().unwrap_or(false);
    eprintln!(
        "{NAME}: the main thread slept in poll before its pollfd array was {} = {slept}",
        removal.done()
    );
    eprintln!(
        "{NAME}: poll with the {} pollfd array failed with EFAULT = {}",
        removal.adjective(),
        got == -1 && error == EFAULT
    );
    let mut byte = [0u8; 1];
    let again = server.read(&mut byte).unwrap_or(0);
    eprintln!(
        "{NAME}: the byte stayed in the socket and a later read took it = {}",
        again == 1 && byte[0] == b'x'
    );
    release_page(page, removal);
}

fn main() {
    let argument_count = std::env::args().count();
    eprintln!("{NAME}: main starts with {argument_count} argument(s)");
    // SAFETY: 引数を取らない。
    let tid = unsafe { syscall(SYS_GETTID) };
    pipe_read(tid, Removal::Unmap);
    pipe_read(tid, Removal::ReadOnly);
    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));
    let mut client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (mut server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    poll_socket(tid, Removal::Unmap, &mut server, &client);
    poll_socket(tid, Removal::ReadOnly, &mut server, &client);
    let _ = client.flush();
    drop(server);
    drop(client);
    drop(listener);
    let _ = fs::remove_file(PATH);
    eprintln!("{NAME}: all four shapes are done");
    std::process::exit(argument_count as i32);
}
