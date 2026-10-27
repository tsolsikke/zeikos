//! `wl-fd-limit-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**fd の上限ちょうどまで
//! 開けて、1 つ越えると断られること**を、`F_DUPFD`・`accept`・`recvmsg` の `SCM_RIGHTS` のそれぞれで通す（2026-10-22。`ADR-0091`）。
//!
//! # 何をするか
//!
//! 1. `RLIMIT_NOFILE` を 32 に下げる（Linux。ZeikOS は上限を変える求めを断り、表の大きさ 32 を答える）。答えを 1 行出す。
//! 2. listener と、それへ繋いだクライアント 2 本と、memfd を開き、listener の複製で fd の表を `EMFILE` まで埋める。埋まった
//!    ときに開いている fd の数が上限と同じことを見る。
//! 3. 1 つ空けて `F_DUPFD` が最後の空きに置け、次の `F_DUPFD` が `EMFILE` で断られることを見る。
//! 4. 1 つ空けて `accept` が最後の空きに置け、次の `accept` が `EMFILE` で断られることを見る。
//! 5. 1 つ空けて、クライアントから `SCM_RIGHTS` で送った memfd を `recvmsg` が最後の空きに置け、表が埋まった後にもう 1 度送った
//!    memfd は渡らないことを見る。**このときも `recvmsg` は受けたバイトを返し、`msg_flags` は `MSG_CTRUNC`、`msg_controllen` は 0
//!    である**（2026-10-27 に行へ足した。Linux の形）。
//!
//! # Linux との差（`ADR-0091`）
//!
//! 行は、両方で同じになる所（最後の空きには置ける、次は断られる・fd が渡らない）だけを言う。表が埋まったときの答え方には、
//! 差が 1 つある。
//!
//! - **`accept`**: Linux は先に fd の空きを見て `EMFILE` を返し、接続は待ち行列に残る（1 つ空ければ次の `accept` で受けられる）。
//!   ZeikOS は待ち行列から接続を取ってから置けずに閉じるので、接続は失われ、相手は EOF を見る。
//!
//! `recvmsg` の `SCM_RIGHTS` の差（ZeikOS は `-EMFILE` を返し、バイトはソケットに残っていた）は、2026-10-27 に Linux の形へ直した。
//!
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-fd-limit-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

const NAME: &str = "wl-fd-limit-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/fd-limit.sock";
/// 確かめる上限（ZeikOS の表の大きさ。Linux はここまで下げる）。
const LIMIT: u64 = 32;

const F_DUPFD_CLOEXEC: i32 = 1030;
const EMFILE: i32 = 24;
const RLIMIT_NOFILE: i32 = 7;
const MFD_CLOEXEC: u32 = 1;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
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
    fn close(fd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn getrlimit(resource: i32, limit: *mut [u64; 2]) -> i32;
    fn setrlimit(resource: i32, limit: *const [u64; 2]) -> i32;
    fn accept(fd: i32, addr: *mut u8, len: *mut u32) -> i32;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
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

fn dup(fd: i32) -> i32 {
    // SAFETY: `fcntl` は整数だけを受ける。
    unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 3) }
}

fn close_fd(fd: i32) {
    // SAFETY: このプログラムが開いた fd を閉じるだけである。
    unsafe { close(fd) };
}

fn accept_on(listener: i32) -> i32 {
    // SAFETY: 相手のアドレスは受け取らない。
    unsafe { accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) }
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
    /// 戻り値（受けたバイト数か -1）。
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

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let mut limit = [0u64; 2];
    // SAFETY: `limit` は `struct rlimit` と同じ 16 バイトである。
    unsafe {
        if getrlimit(RLIMIT_NOFILE, &mut limit) == 0 && limit[0] > LIMIT {
            setrlimit(RLIMIT_NOFILE, &[LIMIT, LIMIT]);
        }
        getrlimit(RLIMIT_NOFILE, &mut limit);
    }
    println!("{NAME}: RLIMIT_NOFILE answers {} for the soft limit and {} for the hard limit", limit[0], limit[1]);

    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));
    let first = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let _second = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    // SAFETY: NUL で終わる名前を渡す。
    let memfd = unsafe { memfd_create(b"fd-limit\0".as_ptr(), MFD_CLOEXEC) };
    if memfd < 0 {
        fail("memfd_create failed", 90);
    }
    let source = listener.as_raw_fd();

    // 表を埋める。開いている数は 0・1・2 と、listener・クライアント 2 本・memfd と、埋めた分である。
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
        break;
    }
    let open = 3 + 4 + fillers.len() as u64;
    println!("{NAME}: the table filled up with exactly as many open fds as the limit = {}", open == limit[0]);

    // F_DUPFD
    close_fd(fillers.pop().unwrap_or_else(|| fail("nothing was filled", 91)));
    let last = dup(source);
    let refused = dup(source) == -1 && errno() == EMFILE;
    println!("{NAME}: a dup into the last free slot succeeded and the next one failed with EMFILE = {}", last >= 0 && refused);
    close_fd(last);

    // accept
    let accepted = accept_on(source);
    let refused = accept_on(source) == -1 && errno() == EMFILE;
    println!(
        "{NAME}: an accept into the last free slot succeeded and the next one failed with EMFILE = {}",
        accepted >= 0 && refused
    );
    if accepted < 0 {
        fail("the first accept failed", 92);
    }

    // recvmsg の SCM_RIGHTS
    close_fd(fillers.pop().unwrap_or_else(|| fail("nothing was filled", 91)));
    let sent = send_with_fd(first.as_raw_fd(), memfd) == 1;
    let received = recv_fd(accepted).fd;
    println!(
        "{NAME}: recvmsg put a passed memfd into the last free slot = {}",
        sent && received >= 0
    );
    let sent_again = send_with_fd(first.as_raw_fd(), memfd) == 1;
    let full = recv_fd(accepted);
    println!("{NAME}: recvmsg into a full table handed over no fd = {}", sent_again && full.fd == -1);
    println!(
        "{NAME}: recvmsg into a full table returned {} (the byte), msg_flags {:#x}, msg_controllen {}",
        full.got, full.flags, full.controllen
    );

    for fd in fillers {
        close_fd(fd);
    }
    close_fd(received);
    close_fd(accepted);
    close_fd(memfd);
    drop(listener);
    let _ = fs::remove_file(PATH);
    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
