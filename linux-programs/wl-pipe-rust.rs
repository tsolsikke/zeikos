//! `wl-pipe-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**`pipe2` と `pipe`、パイプの
//! `poll`・`ppoll`・`epoll`、`SCM_RIGHTS` で運ぶパイプの端**を通す（2026-11-04。`ADR-0063` と `ADR-0065` の Addendum）。
//!
//! # 何をするか
//!
//! 1. `pipe`（22）で作ったパイプで、書いた 1 バイトが読めることを見る。
//! 2. `pipe2` のフラグ: `O_CLOEXEC` で両端に `FD_CLOEXEC` が立つこと、`O_NONBLOCK` の `F_GETFL` の値と、空の読み・満杯の書きの
//!    errno、`F_SETFL` で後から立てた印、知らないフラグの errno を出す。`O_DIRECT` の答えは標準エラーへ出す（Linux は受け、
//!    ZeikOS は断る）。
//! 3. 書けない置き場への `pipe2` を 20 回打ち、毎回 `EFAULT` で、最小の空き fd が動かず、その後もパイプを作れることを見る。
//! 4. パイプの端への `poll` の `revents`（空の読み端・データのある読み端・書き端・書き手の居ない読み端・読み手の居ない書き端）、
//!    `ppoll`、待ち続ける `poll` が別のスレッドの書きで起きること、読み手の居ないパイプへの書きの errno を出す。
//! 5. パイプの端への `epoll_wait` の答えを出す。
//! 6. `SCM_RIGHTS` で書き端を運ぶ: 運ぶ途中に送り手が自分の書き端を閉じても、読み手は EOF を見ない（`EAGAIN`）。受けた書き端から
//!    書いたバイトが読め、受けた書き端を閉じると EOF になる。読み端を運んで、運ぶ途中に書いたバイトが受けた読み端から読める。
//!    書き端を運ぶ途中の接続を、受け取らずに閉じると、読み手が EOF を見る。
//! 7. `RLIMIT_NOFILE` を 32 に下げ（Linux。ZeikOS は表の大きさ 32 を答える）、受け手の表が満杯のとき運んだ書き端が捨てられて、
//!    `MSG_CTRUNC` が立ち、読み手が EOF を見ることを見る。空きが 1 つの表への `pipe2` が `EMFILE` で断られ、空きを残すこと
//!    （10 回）と、空きが 2 つなら作れることを見る。
//!
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-pipe-rust.txt`）。**Rust の std は起動時に
//! `SIGPIPE` を無視するので、Linux でも読み手の居ないパイプへの書きは `EPIPE` で戻る**（ZeikOS はシグナルを送らない）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` と `linux-pipe-test` のビルドだけである。**

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

const NAME: &str = "wl-pipe-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/pipe.sock";
/// 確かめる fd の上限（ZeikOS の表の大きさ。Linux はここまで下げる）。
const LIMIT: u64 = 32;

const O_WRONLY: i32 = 0o1;
const O_NONBLOCK: i32 = 0o4000;
const O_CLOEXEC: i32 = 0o2000000;
const O_DIRECT: i32 = 0o40000;
const F_DUPFD_CLOEXEC: i32 = 1030;
const F_GETFD: i32 = 1;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const EFAULT: i32 = 14;
const EMFILE: i32 = 24;
const RLIMIT_NOFILE: i32 = 7;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const EPOLLIN: u32 = 0x1;
const EPOLLOUT: u32 = 0x4;
const EPOLL_CTL_ADD: i32 = 1;
/// 書けない置き場（低いアドレスはマップされていない）。
const UNWRITABLE: usize = 0x10;
/// 書けない置き場への `pipe2` の回数（ZeikOS のパイプの表の 8 本より多く）。
const EFAULT_TRIES: usize = 20;
/// 空きが 1 つの表への `pipe2` の回数（同じ）。
const EMFILE_TRIES: usize = 10;
/// 表を埋める回数の上限（`EMFILE` が来なければ、名指しの行を出して終わる）。
const FILL_MAX: usize = 4096;

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

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// x86_64 の `struct epoll_event`（詰めた 12 バイト）。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

extern "C" {
    fn pipe(fds: *mut [i32; 2]) -> i32;
    fn pipe2(fds: *mut [i32; 2], flags: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    fn ppoll(fds: *mut PollFd, nfds: u64, timeout: *const Timespec, sigmask: *const u8) -> i32;
    fn epoll_create1(flags: i32) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> i32;
    fn getrlimit(resource: i32, limit: *mut [u64; 2]) -> i32;
    fn setrlimit(resource: i32, limit: *const [u64; 2]) -> i32;
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i32 {
    // SAFETY: musl はスレッドごとの errno の置き場を返す。
    unsafe { *__errno_location() }
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// `pipe2` で作る。失敗したら名指しして終わる。
fn make_pipe(flags: i32) -> (i32, i32) {
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` は 8 バイトの置き場である。
    if unsafe { pipe2(&mut fds, flags) } != 0 {
        fail(&format!("pipe2({flags:#x}) failed with errno {}", errno()), 90);
    }
    (fds[0], fds[1])
}

fn close_fd(fd: i32) {
    // SAFETY: このプログラムが開いた fd を閉じるだけである。
    unsafe { close(fd) };
}

fn put(fd: i32, byte: u8) -> isize {
    // SAFETY: 1 バイトの置き場を渡す。
    unsafe { write(fd, &byte, 1) }
}

/// 1 バイト読む。戻り値と、読めたバイト。
fn take(fd: i32) -> (isize, u8) {
    let mut byte = 0u8;
    // SAFETY: 1 バイトの置き場を渡す。
    let got = unsafe { read(fd, &mut byte, 1) };
    (got, byte)
}

/// 失敗したときの errno（成功なら 0）。
fn errno_of(result: isize) -> i32 {
    if result < 0 {
        errno()
    } else {
        0
    }
}

fn dup(fd: i32) -> i32 {
    // SAFETY: `fcntl` は整数だけを受ける。
    unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 3) }
}

/// 最小の空き fd（複製して、すぐ閉じる）。
fn lowest_free_fd() -> i32 {
    let fd = dup(0);
    if fd >= 0 {
        close_fd(fd);
    }
    fd
}

/// 1 つの fd を `poll` し、戻り値と `revents` を返す。
fn poll_one(fd: i32, events: i16, timeout: i32) -> (i32, i16) {
    let mut entry = PollFd { fd, events, revents: 0 };
    // SAFETY: 1 つの `pollfd` を渡す。
    let got = unsafe { poll(&mut entry, 1, timeout) };
    (got, entry.revents)
}

/// 1 つの fd の `poll` の `revents`。失敗したら名指しして終わる（直す前の形は `EBADF`）。
fn poll_revents(fd: i32, events: i16) -> i16 {
    let (got, revents) = poll_one(fd, events, 0);
    if got < 0 {
        fail(&format!("poll on a pipe end failed with errno {}", errno()), 94);
    }
    revents
}

/// その fd だけを見張る epoll で、待たずに 1 回 `epoll_wait` し、件数と最初の印を返す。
fn epoll_once(fd: i32, events: u32) -> (i32, u32) {
    // SAFETY: 整数だけを受ける。
    let ep = unsafe { epoll_create1(O_CLOEXEC) };
    let mut event = EpollEvent { events, data: 7 };
    // SAFETY: 1 つの `epoll_event` を渡す。
    if ep < 0 || unsafe { epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut event) } != 0 {
        fail(&format!("epoll on a pipe end failed with errno {}", errno()), 95);
    }
    let mut out = [EpollEvent { events: 0, data: 0 }; 2];
    // SAFETY: 2 件分の置き場を渡す。
    let got = unsafe { epoll_wait(ep, out.as_mut_ptr(), 2, 0) };
    close_fd(ep);
    let first = out[0].events;
    (got, if got > 0 { first } else { 0 })
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

/// 1 回の `recvmsg` で戻ったもの。
struct Received {
    got: isize,
    /// 届いた fd（無ければ -1）。
    fd: i32,
    flags: i32,
    controllen: u32,
}

/// `recvmsg` で 1 バイトと fd を受ける。
fn recv_fd(socket: i32) -> Received {
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
    let fd = if got == 1 && msg.controllen >= 20 && cmsg.level == SOL_SOCKET && cmsg.kind == SCM_RIGHTS {
        cmsg.fd
    } else {
        -1
    };
    Received { got, fd, flags: msg.flags, controllen: msg.controllen }
}

/// `EMFILE` が来るまで `source` の複製で表を埋め、埋めた fd を返す。
fn fill_table(source: i32) -> Vec<i32> {
    let mut fillers = Vec::new();
    for _ in 0..FILL_MAX {
        let fd = dup(source);
        if fd >= 0 {
            fillers.push(fd);
            continue;
        }
        if errno() != EMFILE {
            fail(&format!("filling the fd table failed with errno {}", errno()), 91);
        }
        return fillers;
    }
    fail("the fd table never filled up", 91)
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");

    // 1. pipe（22）
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` は 8 バイトの置き場である。
    if unsafe { pipe(&mut fds) } != 0 {
        fail(&format!("pipe failed with errno {}", errno()), 90);
    }
    let wrote = put(fds[1], b'p');
    let (got, byte) = take(fds[0]);
    println!("{NAME}: a byte written to a pipe() write end came out of its read end = {}", wrote == 1 && got == 1 && byte == b'p');
    close_fd(fds[0]);
    close_fd(fds[1]);

    // 2. pipe2 のフラグ
    let (r, w) = make_pipe(O_CLOEXEC);
    // SAFETY: 整数だけを受ける。
    let marks = unsafe { (fcntl(r, F_GETFD), fcntl(w, F_GETFD)) };
    println!("{NAME}: pipe2(O_CLOEXEC) answers F_GETFD {} for the read end and {} for the write end", marks.0, marks.1);
    close_fd(r);
    close_fd(w);
    let (r, w) = make_pipe(O_NONBLOCK);
    // SAFETY: 整数だけを受ける。
    let modes = unsafe { (fcntl(r, F_GETFL), fcntl(w, F_GETFL)) };
    let empty = errno_of(take(r).0);
    let mut full = 0;
    for _ in 0..1_000_000 {
        let result = put(w, b'f');
        if result < 0 {
            full = errno();
            break;
        }
    }
    println!(
        "{NAME}: pipe2(O_NONBLOCK) answers F_GETFL {:#x} for the read end and {:#x} for the write end; reading it empty failed with errno {empty} and writing it full with errno {full}",
        modes.0, modes.1
    );
    close_fd(r);
    close_fd(w);
    let (r, w) = make_pipe(0);
    // SAFETY: 整数だけを受ける。
    let set = unsafe { fcntl(r, F_SETFL, O_NONBLOCK) };
    // SAFETY: 整数だけを受ける。
    let mode = unsafe { fcntl(r, F_GETFL) };
    let empty = errno_of(take(r).0);
    println!("{NAME}: F_SETFL(O_NONBLOCK) on a read end returned {set}, F_GETFL then answers {mode:#x}, and reading it empty failed with errno {empty}");
    close_fd(r);
    close_fd(w);
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` は 8 バイトの置き場である。
    let refused = unsafe { pipe2(&mut fds, O_WRONLY) };
    println!("{NAME}: pipe2 with the unknown flag O_WRONLY returned {refused} with errno {}", errno());
    // SAFETY: 同上。
    let direct = unsafe { pipe2(&mut fds, O_DIRECT) };
    eprintln!("{NAME}: pipe2(O_DIRECT) returned {direct} with errno {} (Linux accepts it; ZeikOS refuses it)", errno_of(direct as isize));
    if direct == 0 {
        close_fd(fds[0]);
        close_fd(fds[1]);
    }

    // 3. 書けない置き場への pipe2
    let lowest_before = lowest_free_fd();
    let mut errnos = Vec::new();
    for _ in 0..EFAULT_TRIES {
        // SAFETY: わざと書けない置き場を渡す。カーネルは書く前に確かめる。
        let result = unsafe { pipe2(UNWRITABLE as *mut [i32; 2], 0) };
        errnos.push(if result == 0 { 0 } else { errno() });
    }
    let lowest_after = lowest_free_fd();
    if errnos.iter().any(|&e| e != EFAULT) || lowest_after != lowest_before {
        fail(
            &format!(
                "pipe2 into an unwritable address left something behind (errnos {errnos:?}, the lowest free fd was {lowest_before} and is now {lowest_after})"
            ),
            92,
        );
    }
    let (r, w) = make_pipe(0);
    println!("{NAME}: pipe2 into an unwritable address failed with errno {EFAULT} all {EFAULT_TRIES} times, left the lowest free fd where it was, and a pipe could still be made = true");
    close_fd(r);
    close_fd(w);

    // 4. poll と ppoll
    let (r, w) = make_pipe(0);
    let empty = poll_revents(r, POLLIN);
    put(w, b'a');
    let with_data = poll_revents(r, POLLIN);
    let writable = poll_revents(w, POLLOUT);
    let mut timeout = Timespec { sec: 0, nsec: 0 };
    let mut entry = PollFd { fd: r, events: POLLIN, revents: 0 };
    // SAFETY: 1 つの `pollfd` と、待たない長さを渡す。
    let ppolled = unsafe { ppoll(&mut entry, 1, &mut timeout, std::ptr::null()) };
    close_fd(w);
    take(r);
    let no_writer = poll_revents(r, POLLIN);
    close_fd(r);
    let (r, w) = make_pipe(0);
    close_fd(r);
    let no_reader = poll_revents(w, POLLOUT);
    let epipe = errno_of(put(w, b'x'));
    close_fd(w);
    println!(
        "{NAME}: poll on pipe ends answers revents {empty:#x} for an empty read end, {with_data:#x} with data, {writable:#x} for the write end, {no_writer:#x} for a read end without writers and {no_reader:#x} for a write end without readers"
    );
    println!("{NAME}: ppoll on a read end with data returned {ppolled} with revents {:#x}", entry.revents);
    println!("{NAME}: writing to a pipe without readers failed with errno {epipe}");
    let (r, w) = make_pipe(0);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        put(w, b't');
        w
    });
    let (woke, revents) = poll_one(r, POLLIN, -1);
    let w = writer.join().unwrap_or_else(|_| fail("the writer thread panicked", 90));
    println!("{NAME}: poll with no timeout woke when another thread wrote = {}", woke == 1 && revents == POLLIN);
    close_fd(r);
    close_fd(w);

    // 5. epoll
    let (r, w) = make_pipe(0);
    let empty = epoll_once(r, EPOLLIN);
    put(w, b'e');
    let with_data = epoll_once(r, EPOLLIN);
    close_fd(w);
    take(r);
    let no_writer = epoll_once(r, EPOLLIN);
    close_fd(r);
    let (r, w) = make_pipe(0);
    close_fd(r);
    let no_reader = epoll_once(w, EPOLLOUT);
    close_fd(w);
    println!(
        "{NAME}: epoll_wait on pipe ends answers {} event(s) for an empty read end, {:#x} with data, {:#x} for a read end without writers and {:#x} for a write end without readers",
        empty.0, with_data.1, no_writer.1, no_reader.1
    );

    // 6. SCM_RIGHTS
    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));
    let client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    let (r, w) = make_pipe(O_NONBLOCK);
    let sent = send_with_fd(client.as_raw_fd(), w);
    close_fd(w);
    let (got, _) = take(r);
    if got == 0 {
        fail("the reader saw end of file while the write end was in flight", 93);
    }
    println!(
        "{NAME}: while the passed write end was in flight, reading the pipe returned {got} with errno {} (sendmsg returned {sent})",
        errno_of(got)
    );
    let received = recv_fd(server.as_raw_fd());
    let wrote = put(received.fd, b'w');
    let (got, byte) = take(r);
    println!("{NAME}: a byte written through the passed write end came out of the read end = {}", wrote == 1 && got == 1 && byte == b'w');
    close_fd(received.fd);
    let (got, _) = take(r);
    println!("{NAME}: after the passed write end was closed, reading the pipe returned {got}");
    close_fd(r);
    let (r, w) = make_pipe(0);
    send_with_fd(client.as_raw_fd(), r);
    close_fd(r);
    let wrote = put(w, b'r');
    let received = recv_fd(server.as_raw_fd());
    let (got, byte) = take(received.fd);
    println!("{NAME}: a byte written while the read end was in flight came out of the passed read end = {}", wrote == 1 && got == 1 && byte == b'r');
    close_fd(received.fd);
    close_fd(w);
    let other = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (other_server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    let (r, w) = make_pipe(O_NONBLOCK);
    send_with_fd(other.as_raw_fd(), w);
    close_fd(w);
    drop(other_server);
    let (got, _) = take(r);
    println!("{NAME}: after the connection carrying the write end was closed unread, reading the pipe returned {got}");
    close_fd(r);
    drop(other);

    // 7. 満杯の表
    let mut limit = [0u64; 2];
    // SAFETY: `limit` は `struct rlimit` と同じ 16 バイトである。
    unsafe {
        if getrlimit(RLIMIT_NOFILE, &mut limit) == 0 && limit[0] > LIMIT {
            setrlimit(RLIMIT_NOFILE, &[LIMIT, LIMIT]);
        }
    }
    let (r, w) = make_pipe(O_NONBLOCK);
    send_with_fd(client.as_raw_fd(), w);
    close_fd(w);
    let fillers = fill_table(r);
    let full = recv_fd(server.as_raw_fd());
    let (got, _) = take(r);
    println!(
        "{NAME}: recvmsg into a full table returned {} with msg_flags {:#x} and msg_controllen {}, handed over no fd = {}, and reading the pipe then returned {got}",
        full.got, full.flags, full.controllen, full.fd == -1
    );
    let mut fillers = fillers;
    close_fd(fillers.pop().unwrap_or_else(|| fail("nothing was filled", 91)));
    let free_before = lowest_free_fd();
    let mut errnos = Vec::new();
    let mut fds = [-1i32; 2];
    for _ in 0..EMFILE_TRIES {
        // SAFETY: `fds` は 8 バイトの置き場である。
        let result = unsafe { pipe2(&mut fds, 0) };
        errnos.push(if result == 0 { 0 } else { errno() });
    }
    let free_after = lowest_free_fd();
    println!(
        "{NAME}: with one free fd, pipe2 failed with errno {EMFILE} all {EMFILE_TRIES} times and left the free fd free = {}",
        errnos.iter().all(|&e| e == EMFILE) && free_before >= 0 && free_after == free_before
    );
    close_fd(fillers.pop().unwrap_or_else(|| fail("nothing was filled", 91)));
    // SAFETY: 同上。
    let made = unsafe { pipe2(&mut fds, 0) };
    println!("{NAME}: with two free fds, pipe2 succeeded = {}", made == 0);
    if made == 0 {
        close_fd(fds[0]);
        close_fd(fds[1]);
    }
    for fd in fillers {
        close_fd(fd);
    }
    close_fd(r);
    drop(server);
    drop(client);
    drop(listener);
    let _ = fs::remove_file(PATH);
    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
