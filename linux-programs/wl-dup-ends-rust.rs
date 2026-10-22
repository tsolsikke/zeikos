//! `wl-dup-ends-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**`F_DUPFD` で複製した
//! パイプの端・ソケットの端・listener・memfd が、元を閉じた後も使えること**と、**最後の 1 つを閉じたときだけ相手が EOF・
//! `EPIPE` を見ること**を通す（2026-10-22。`ADR-0064` の Addendum）。
//!
//! # 何をするか
//!
//! fd 0 と fd 1 は同じパイプの読み端と書き端である（ZeikOS では起動時の確かめが据え、Linux では参照を取る道具が名前付き
//! パイプを両方へ開いて渡す。ZeikOS は Linux 向けの `pipe`・`pipe2` をまだ持たない）。どちらの形でも、まず次を通す。
//!
//! 1. 読み端と書き端を `F_DUPFD_CLOEXEC` で複製し、元の 0 と 1 を閉じる。書き端をもう 1 つ複製してスレッドへ渡し、スレッドが
//!    1 バイト書いて閉じる。主スレッドが読み端の複製で読む。
//!
//! 3 つ目の引数で、残りを分ける（ZeikOS のパイプは同時に 1 本しか作れないので、起動時の確かめが 2 回起こす）。
//!
//! - `b`: fd の表を `EMFILE` まで埋めてから、書き端の複製を断らせる。埋めた分を閉じ、最後の書き端を閉じると、読み手は EOF を
//!   見る。**断った複製の数が戻っていなければ、読み手が眠る**——見張り役のスレッドが、眠ったことを見たら名指しの行を出して
//!   93 で終わる（眠ったことの見方は `wl-fault-after-wait-rust` と同じ）。続けて、listener とソケットの端と memfd を通す——
//!   listener の複製が元を閉じた後も `accept` でき、クライアントの端の複製をスレッドへ渡して書け、最後の端を閉じるとサーバーが
//!   EOF と `EPIPE` を見ること。memfd の複製が元を閉じた後もマップして読み書きでき、`SCM_RIGHTS` で送った memfd を、送り手が
//!   閉じた後も受け手がマップして読み書きできること。受け手の fd はプロセスの終わりまで持つ（マッピングが残ったまま fd を全部
//!   閉じる形は、別の課題の範囲である）。
//! - `c`: 同じく埋めてから、読み端の複製を断らせる。埋めた分を閉じ、最後の読み端を閉じると、書き手は `EPIPE` を見る。
//!
//! fd の表の上限は、Linux では `RLIMIT_NOFILE` を 64 に下げてから埋める（ZeikOS は上限を変える求めを断るので、表の大きさまで
//! 埋める）。行はどれも**標準エラーへ**出す（標準出力はパイプなので）。終了ステータスは引数の数（4）。Linux でも同じ行を出す
//! （`linux-programs/reference/wl-dup-ends-rust.txt`。`b` と `c` の 2 回の行を順に控える）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const NAME: &str = "wl-dup-ends-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/dup-ends.sock";

const F_DUPFD_CLOEXEC: i32 = 1030;
const EMFILE: i32 = 24;
const EPIPE: i32 = 32;
const RLIMIT_NOFILE: i32 = 7;
const PAGE: usize = 4096;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 0x01;
const MFD_CLOEXEC: u32 = 1;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SYS_READ: i64 = 0;
const SYS_GETTID: i64 = 186;
/// ZeikOS の確かめ用の番号（`linux-socket-test` のビルドだけが持つ。Linux では打たない）。
const ZEIKOS_SLEEPS_READING: i64 = 0x100A;
/// 眠ったことを見直す回数の上限。
const LOOKS: usize = 10_000;
/// 表を埋める回数の上限（`EMFILE` が来なければ、名指しの行を出して終わる）。
const FILL_MAX: usize = 4096;

/// musl の `struct iovec`。
#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

/// musl の x86_64 の `struct msghdr`（56 バイト。Linux と同じ配置）。
#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    namelen: u32,
    iov: *mut IoVec,
    iovlen: i32,
    pad1: i32,
    control: *mut u8,
    controllen: u32,
    pad2: u32,
    flags: i32,
}

/// fd を 1 つ運ぶ `cmsghdr`（`CMSG_LEN(4)` が 20、`CMSG_SPACE(4)` が 24）。
#[repr(C)]
struct CmsgOneFd {
    len: u32,
    pad: u32,
    level: i32,
    kind: i32,
    fd: i32,
    tail: u32,
}

extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn getrlimit(resource: i32, limit: *mut [u64; 2]) -> i32;
    fn setrlimit(resource: i32, limit: *const [u64; 2]) -> i32;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn ftruncate(fd: i32, len: i64) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
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

/// `F_DUPFD_CLOEXEC` で 3 以上の最小の番号へ複製する（Rust の `try_clone` と同じ形）。
fn dup(fd: i32) -> i32 {
    // SAFETY: `fcntl` は整数だけを受ける。
    unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 3) }
}

fn close_fd(fd: i32) {
    // SAFETY: このプログラムが開いた fd を閉じるだけである。
    unsafe { close(fd) };
}

/// Linux では `RLIMIT_NOFILE` を 64 に下げる（ZeikOS は上限を変える求めを断るので、何もしない）。
fn lower_the_fd_limit() {
    let mut limit = [0u64; 2];
    // SAFETY: `limit` は `struct rlimit` と同じ 16 バイトである。
    if unsafe { getrlimit(RLIMIT_NOFILE, &mut limit) } == 0 && limit[0] > 64 {
        limit[0] = 64;
        // SAFETY: 同上。
        unsafe { setrlimit(RLIMIT_NOFILE, &limit) };
    }
}

/// `source` の複製で表を `EMFILE` まで埋め、埋めた fd を返す。
fn fill_the_table(source: i32) -> Vec<i32> {
    let mut fillers = Vec::new();
    for _ in 0..FILL_MAX {
        let fd = dup(source);
        if fd >= 0 {
            fillers.push(fd);
            continue;
        }
        if errno() != EMFILE {
            fail(&format!("filling the fd table failed with errno {}", errno()), 90);
        }
        return fillers;
    }
    fail("the fd table did not fill up", 90)
}

/// 主スレッド（`tid`）が、`fd` が読めるのを待って眠っているか（`wl-fault-after-wait-rust` と同じ見方）。
fn sleeps_reading(tid: i64, fd: i32) -> bool {
    let base = format!("/proc/self/task/{tid}");
    match (fs::read_to_string(format!("{base}/stat")), fs::read_to_string(format!("{base}/syscall"))) {
        (Ok(stat), Ok(syscall)) => {
            let state = stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next());
            let current = syscall.split_whitespace().next().and_then(|n| n.parse::<i64>().ok());
            state == Some('S') && current == Some(SYS_READ)
        }
        // SAFETY: 確かめ用の番号は fd を 1 つ取り、ポインタを読まない。
        _ => (unsafe { syscall(ZEIKOS_SLEEPS_READING, fd as i64) }) == 1,
    }
}

/// 両方の形で通す最初の部分。読み端と書き端の複製を返す（元の 0 と 1 は閉じた）。
fn copies_work() -> (i32, i32) {
    let reader = dup(0);
    let writer = dup(1);
    if reader < 0 || writer < 0 {
        fail("duplicating the pipe ends failed", 89);
    }
    close_fd(0);
    close_fd(1);
    let moved = dup(writer);
    let wrote = thread::spawn(move || {
        // SAFETY: 1 バイトを書くだけである。
        let wrote = unsafe { write(moved, b"a".as_ptr(), 1) } == 1;
        close_fd(moved);
        wrote
    })
    .join()
    .unwrap_or(false);
    let mut byte = [0u8; 1];
    // SAFETY: `byte` は 1 バイトである。
    let got = unsafe { read(reader, byte.as_mut_ptr(), 1) };
    eprintln!(
        "{NAME}: copies of both pipe ends work after the originals were closed, and a copy moved to a thread wrote = {}",
        wrote && got == 1 && byte[0] == b'a'
    );
    (reader, writer)
}

/// 形 `b`: 書き端の複製を断らせてから、最後の書き端を閉じると、読み手が EOF を見る。
fn the_last_write_end(tid: i64, reader: i32, writer: i32) {
    let fillers = fill_the_table(reader);
    let refused = dup(writer) == -1 && errno() == EMFILE;
    for fd in fillers {
        close_fd(fd);
    }
    eprintln!("{NAME}: the dup of the write end into a full table failed with EMFILE = {refused}");
    close_fd(writer);
    let done = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&done);
    let watcher = thread::spawn(move || {
        for _ in 0..LOOKS {
            if seen.load(Ordering::SeqCst) {
                return;
            }
            if sleeps_reading(tid, reader) {
                fail("the reader slept although every write end was closed", 93);
            }
            // SAFETY: 引数を取らない。
            unsafe { sched_yield() };
        }
    });
    let mut byte = [0u8; 1];
    // SAFETY: `byte` は 1 バイトである。
    let got = unsafe { read(reader, byte.as_mut_ptr(), 1) };
    done.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    eprintln!("{NAME}: after the refused dup, closing the last write end gave the reader EOF = {}", got == 0);
    close_fd(reader);
}

/// 形 `c`: 読み端の複製を断らせてから、最後の読み端を閉じると、書き手が `EPIPE` を見る。
fn the_last_read_end(reader: i32, writer: i32) {
    let fillers = fill_the_table(writer);
    let refused = dup(reader) == -1 && errno() == EMFILE;
    for fd in fillers {
        close_fd(fd);
    }
    eprintln!("{NAME}: the dup of the read end into a full table failed with EMFILE = {refused}");
    close_fd(reader);
    // SAFETY: 1 バイトを書くだけである。
    let wrote = unsafe { write(writer, b"b".as_ptr(), 1) };
    let error = errno();
    eprintln!(
        "{NAME}: after the refused dup, closing the last read end gave the writer EPIPE = {}",
        wrote == -1 && error == EPIPE
    );
    close_fd(writer);
}

/// listener とソケットの端の複製。
fn socket_copies() -> UnixListener {
    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 91));
    let copy = listener.try_clone().unwrap_or_else(|error| fail(&format!("try_clone of the listener failed: {error}"), 91));
    drop(listener);
    let client = UnixStream::connect(PATH)
        .unwrap_or_else(|error| fail(&format!("connect after closing the original listener failed: {error}"), 94));
    let (mut server, _) = copy
        .accept()
        .unwrap_or_else(|error| fail(&format!("accept on the copy of the listener failed: {error}"), 94));
    eprintln!("{NAME}: a copy of the listener accepts after the original was closed = true");
    let moved = client.try_clone().unwrap_or_else(|error| fail(&format!("try_clone of the client failed: {error}"), 91));
    drop(client);
    let (wrote, moved) = thread::spawn(move || {
        let mut moved = moved;
        (moved.write_all(b"x").is_ok(), moved)
    })
    .join()
    .unwrap_or_else(|_| fail("the writing thread panicked", 91));
    let mut byte = [0u8; 1];
    let got = server.read(&mut byte).unwrap_or(0);
    eprintln!(
        "{NAME}: a copy of the client end moved to a thread wrote after the original was closed = {}",
        wrote && got == 1 && byte[0] == b'x'
    );
    drop(moved);
    let eof = server.read(&mut byte).map(|n| n == 0).unwrap_or(false);
    eprintln!("{NAME}: closing the last client end gave the server EOF = {eof}");
    let epipe = matches!(server.write(b"y"), Err(error) if error.raw_os_error() == Some(EPIPE));
    eprintln!("{NAME}: and the server's write after that failed with EPIPE = {epipe}");
    copy
}

fn map(fd: i32) -> *mut u8 {
    // SAFETY: 共有の 1 ページをマップする。
    let page = unsafe { mmap(std::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
    if page as isize == -1 {
        std::ptr::null_mut()
    } else {
        page
    }
}

/// `sendmsg` で 1 バイトと fd を送る。
fn send_with_fd(socket: i32, fd: i32) -> isize {
    let mut byte = [b'm'];
    let mut iov = IoVec { base: byte.as_mut_ptr(), len: 1 };
    let mut cmsg = CmsgOneFd { len: 20, pad: 0, level: SOL_SOCKET, kind: SCM_RIGHTS, fd, tail: 0 };
    let msg = MsgHdr {
        name: std::ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        pad1: 0,
        control: (&mut cmsg as *mut CmsgOneFd).cast(),
        controllen: 24,
        pad2: 0,
        flags: 0,
    };
    // SAFETY: 渡すポインタは、どれもこの関数の中で生きている。
    unsafe { sendmsg(socket, &msg, 0) }
}

/// `recvmsg` で 1 バイトと fd を受ける。届いた fd（無ければ -1）。
fn recv_fd(socket: i32) -> i32 {
    let mut byte = [0u8; 1];
    let mut iov = IoVec { base: byte.as_mut_ptr(), len: 1 };
    let mut cmsg = CmsgOneFd { len: 0, pad: 0, level: 0, kind: 0, fd: -1, tail: 0 };
    let mut msg = MsgHdr {
        name: std::ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        pad1: 0,
        control: (&mut cmsg as *mut CmsgOneFd).cast(),
        controllen: 24,
        pad2: 0,
        flags: 0,
    };
    // SAFETY: 渡すポインタは、どれもこの関数の中で生きている。
    let got = unsafe { recvmsg(socket, &mut msg, 0) };
    if got == 1 && cmsg.level == SOL_SOCKET && cmsg.kind == SCM_RIGHTS {
        cmsg.fd
    } else {
        -1
    }
}

/// memfd の複製と、`SCM_RIGHTS` で送った memfd。受け手の fd は閉じずに返す（プロセスの終わりまで持つ）。
fn memfd_copies(listener: &UnixListener) -> i32 {
    // SAFETY: NUL で終わる名前を渡す。
    let original = unsafe { memfd_create(b"dup-ends\0".as_ptr(), MFD_CLOEXEC) };
    // SAFETY: 整数だけを受ける。
    if original < 0 || unsafe { ftruncate(original, PAGE as i64) } != 0 {
        fail("memfd_create or ftruncate failed", 92);
    }
    let copy = dup(original);
    close_fd(original);
    let sender_page = map(copy);
    let mapped = !sender_page.is_null();
    if mapped {
        // SAFETY: 書けるようにマップした 1 ページの先頭である。
        unsafe {
            sender_page.write_volatile(0x5a);
        }
    }
    // SAFETY: 同上。マップできたときだけ読む。
    let read_back = mapped && unsafe { sender_page.read_volatile() } == 0x5a;
    eprintln!("{NAME}: a copy of a memfd maps and reads back after the original was closed = {read_back}");
    let sender = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect for SCM_RIGHTS failed: {error}"), 92));
    let (receiver, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept for SCM_RIGHTS failed: {error}"), 92));
    let sent = send_with_fd(sender.as_raw_fd(), copy) == 1;
    close_fd(copy);
    let received = recv_fd(receiver.as_raw_fd());
    let receiver_page = if received >= 0 { map(received) } else { std::ptr::null_mut() };
    let shared = !receiver_page.is_null() && mapped && {
        // SAFETY: どちらも同じ memfd を書けるようにマップした 1 ページの先頭である。
        unsafe {
            let saw = receiver_page.read_volatile() == 0x5a;
            receiver_page.write_volatile(0xa5);
            saw && sender_page.read_volatile() == 0xa5
        }
    };
    eprintln!(
        "{NAME}: after the sender closed its memfd, the receiver mapped the received fd, read what the sender wrote, and its \
         write showed through = {}",
        sent && shared
    );
    received
}

fn main() {
    let argument_count = std::env::args().count();
    let shape = std::env::args().nth(3).unwrap_or_default();
    eprintln!("{NAME}: main starts with {argument_count} argument(s), shape {shape}");
    lower_the_fd_limit();
    // SAFETY: 引数を取らない。
    let tid = unsafe { syscall(SYS_GETTID) };
    let (reader, writer) = copies_work();
    match shape.as_str() {
        "b" => {
            the_last_write_end(tid, reader, writer);
            let listener = socket_copies();
            let _kept = memfd_copies(&listener);
            drop(listener);
            let _ = fs::remove_file(PATH);
        }
        "c" => the_last_read_end(reader, writer),
        _ => fail("the third argument is neither b nor c", 88),
    }
    eprintln!("{NAME}: shape {shape} is done");
    std::process::exit(argument_count as i32);
}
