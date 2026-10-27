//! `wl-recvmsg-name-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**`recvmsg` が
//! `msg_name`・`msg_namelen`・`msg_flags`・`msg_controllen` に何を書き戻すか**（2026-10-26。`ADR-0065` の Addendum）。
//!
//! # 何をするか
//!
//! `recvmsg` は libc を通さず、`syscall` で打つ（musl の `recvmsg` は `msghdr` をコピーして詰め物を 0 にするので、カーネルが
//! 書いた欄だけを見るため。Seinas は rustix を通して同じ形で打つ）。listener を作り、クライアントが繋ぎ、`accept` する。
//!
//! 1. `msg_name` の置き場を渡す。
//!    - 繋いだ側（名前を持たない）から受けると、`msg_namelen` は 0 で、置き場は書かれない。
//!    - 受け付けた側（listener の名前を受け継ぐ）から受けると、`msg_namelen` は 2 + 名前 + NUL で、置き場に `sockaddr_un` が
//!      入る。
//!    - 置き場が短いと、書くのは置き場の長さまでで、`msg_namelen` は切り詰める前の長さになる。0 なら何も書かない。
//!    - 置き場があって `msg_namelen` が負なら `EINVAL` で、バイトは次の `recvmsg` に残る。置き場が NULL なら、負でも受けて、
//!      `msg_namelen` は書かない。
//!    - 読めるものが無くて `EAGAIN` のときは、どの欄も書かない。
//! 2. `SCM_RIGHTS` で memfd を 1 つ受ける。
//!    - 補助データの置き場が 24 バイト以上なら `msg_controllen` は 24（`CMSG_SPACE(4)`）、20 なら 20。`msg_flags` は 0。
//!    - `MSG_CMSG_CLOEXEC` を渡すと、`msg_flags` にもその印が戻り、受けた fd に `FD_CLOEXEC` が立つ。
//!    - 置き場が NULL か、20 バイトに足りないと、バイトは受け、fd は捨て、`msg_controllen` は 0、`msg_flags` は `MSG_CTRUNC`。
//!    - fd を運ばないときも、`msg_controllen` は 0 に、`msg_flags` は 0 に書き戻す（渡した値を残さない）。
//!
//! 行はどれも標準出力へ出す。fd の番号は出さない（Linux と ZeikOS で違う）。終了ステータスは引数の数（4）。Linux でも同じ行を
//! 出す（`linux-programs/reference/wl-recvmsg-name-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

const NAME: &str = "wl-recvmsg-name-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/recvmsg-name.sock";

const SYS_SENDMSG: i64 = 46;
const SYS_RECVMSG: i64 = 47;
const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const FD_CLOEXEC: i32 = 1;
const MFD_CLOEXEC: u32 = 1;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_DONTWAIT: i64 = 0x40;
const MSG_CMSG_CLOEXEC: i64 = 0x4000_0000;
const EINVAL: i64 = 22;
const EAGAIN: i64 = 11;
/// 渡す前に置き場を埋めておく値（書かれたかどうかを見分ける）。
const FILL: u8 = 0xAA;
/// 渡す前に `msg_flags` に入れておく値（書き戻されたかどうかを見分ける）。
const FLAGS_BEFORE: i32 = 0x7fff;

/// `struct iovec`。
#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

/// Linux の x86_64 の `struct msghdr`（56 バイト）。`msg_namelen` は 8、`msg_controllen` は 40、`msg_flags` は 48 に在る。
#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    namelen: i32,
    pad1: u32,
    iov: *mut IoVec,
    iovlen: usize,
    control: *mut u8,
    controllen: usize,
    flags: i32,
    pad2: u32,
}

extern "C" {
    fn syscall(number: i64, ...) -> i64;
    fn close(fd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i64 {
    // SAFETY: musl はスレッドごとの errno の置き場を返す。
    (unsafe { *__errno_location() }) as i64
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// 空いている最も小さい fd（`F_DUPFD` で取ってすぐ閉じる）。
fn lowest_free_fd() -> i32 {
    // SAFETY: `fcntl` は整数だけを受ける。取った fd はすぐ閉じる。
    unsafe {
        let fd = fcntl(0, F_DUPFD, 0);
        close(fd);
        fd
    }
}

fn new_memfd() -> i32 {
    // SAFETY: NUL で終わる名前。
    let fd = unsafe { memfd_create(b"recvmsg-name\0".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        fail("memfd_create failed", 89);
    }
    fd
}

/// `sendmsg` で 1 バイトと fd を送る。
fn send_with_fd(socket: i32, fd: i32) {
    let mut byte = [b'f'];
    let mut iov = IoVec { base: byte.as_mut_ptr(), len: 1 };
    let mut control = [0u8; 24];
    control[0..8].copy_from_slice(&20u64.to_le_bytes());
    control[8..12].copy_from_slice(&SOL_SOCKET.to_le_bytes());
    control[12..16].copy_from_slice(&SCM_RIGHTS.to_le_bytes());
    control[16..20].copy_from_slice(&fd.to_le_bytes());
    let msg = MsgHdr {
        name: std::ptr::null_mut(),
        namelen: 0,
        pad1: 0,
        iov: &mut iov,
        iovlen: 1,
        control: control.as_mut_ptr(),
        controllen: 24,
        flags: 0,
        pad2: 0,
    };
    // SAFETY: 渡すポインタは、どれもこの関数の中で生きている。
    if unsafe { syscall(SYS_SENDMSG, socket as i64, &msg as *const MsgHdr, 0i64) } != 1 {
        fail("sendmsg with an fd failed", 90);
    }
}

/// 1 回の `recvmsg` に渡すもの。
struct Ask {
    /// `msg_name` の置き場を渡すか。
    with_name: bool,
    namelen: i32,
    /// `msg_control` の置き場を渡すか。
    with_control: bool,
    controllen: usize,
    flags: i64,
}

/// 1 回の `recvmsg` で戻ったもの。
struct Got {
    /// 戻り値（失敗なら負の errno）。
    ret: i64,
    byte: u8,
    name: [u8; 128],
    namelen: i32,
    control: [u8; 64],
    controllen: usize,
    flags: i32,
}

fn recv(socket: i32, ask: &Ask) -> Got {
    let mut byte = [0u8; 1];
    let mut iov = IoVec { base: byte.as_mut_ptr(), len: 1 };
    let mut name = [FILL; 128];
    let mut control = [FILL; 64];
    let mut msg = MsgHdr {
        name: if ask.with_name { name.as_mut_ptr() } else { std::ptr::null_mut() },
        namelen: ask.namelen,
        pad1: 0,
        iov: &mut iov,
        iovlen: 1,
        control: if ask.with_control { control.as_mut_ptr() } else { std::ptr::null_mut() },
        controllen: ask.controllen,
        flags: FLAGS_BEFORE,
        pad2: 0,
    };
    // SAFETY: 渡すポインタは、どれもこの関数の中で生きている。置き場の長さは `namelen`（128 まで）と `controllen`（64 まで）より
    // 長い。
    let ret = unsafe { syscall(SYS_RECVMSG, socket as i64, &mut msg as *mut MsgHdr, ask.flags) };
    // musl の `syscall` は、失敗のとき -1 を返して errno を置く。
    let ret = if ret < 0 { -errno() } else { ret };
    Got {
        ret,
        byte: byte[0],
        name,
        namelen: msg.namelen,
        control,
        controllen: msg.controllen,
        flags: msg.flags,
    }
}

fn untouched(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == FILL)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// 受けた `SCM_RIGHTS` の fd（`cmsg_len`・`cmsg_level`・`cmsg_type` が合わなければ `None`）。
fn received_fd(got: &Got) -> Option<i32> {
    let len = u64::from_le_bytes(got.control[0..8].try_into().unwrap());
    (got.controllen >= 20 && len == 20 && u32_at(&got.control, 8) == 1 && u32_at(&got.control, 12) == 1)
        .then(|| u32_at(&got.control, 16) as i32)
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));
    let mut client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (mut server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    let name_ask = |namelen| Ask { with_name: true, namelen, with_control: true, controllen: 64, flags: 0 };

    // 1. msg_name
    client.write_all(b"a").unwrap_or_else(|_| fail("write failed", 91));
    let got = recv(server.as_raw_fd(), &name_ask(128));
    println!(
        "{NAME}: from the connecting side: returned {}, msg_namelen {}, the name untouched = {}, msg_flags {:#x}, msg_controllen {}",
        got.ret,
        got.namelen,
        untouched(&got.name),
        got.flags,
        got.controllen
    );

    server.write_all(b"b").unwrap_or_else(|_| fail("write failed", 91));
    let got = recv(client.as_raw_fd(), &name_ask(128));
    let family = u16::from_le_bytes([got.name[0], got.name[1]]);
    let path_len = got.name[2..].iter().position(|byte| *byte == 0).unwrap_or(0);
    let path = String::from_utf8_lossy(&got.name[2..2 + path_len]).into_owned();
    println!(
        "{NAME}: from the accepted side: returned {}, msg_namelen {}, sun_family {}, sun_path {:?}, untouched after the NUL = {}, msg_flags {:#x}, msg_controllen {}",
        got.ret,
        got.namelen,
        family,
        path,
        untouched(&got.name[got.namelen.max(0) as usize..]),
        got.flags,
        got.controllen
    );

    server.write_all(b"c").unwrap_or_else(|_| fail("write failed", 91));
    let got = recv(client.as_raw_fd(), &name_ask(4));
    println!(
        "{NAME}: with a 4-byte name: returned {}, msg_namelen {}, wrote the first 4 bytes = {}, untouched after them = {}",
        got.ret,
        got.namelen,
        got.name[0..4] == [1, 0, b'/', b't'],
        untouched(&got.name[4..])
    );

    server.write_all(b"d").unwrap_or_else(|_| fail("write failed", 91));
    let got = recv(client.as_raw_fd(), &name_ask(0));
    println!(
        "{NAME}: with a 0-byte name: returned {}, msg_namelen {}, the name untouched = {}",
        got.ret,
        got.namelen,
        untouched(&got.name)
    );

    server.write_all(b"e").unwrap_or_else(|_| fail("write failed", 91));
    let refused = recv(client.as_raw_fd(), &name_ask(-1));
    let next = recv(client.as_raw_fd(), &name_ask(128));
    println!(
        "{NAME}: with msg_namelen -1: returned EINVAL = {}, msg_namelen {}, the byte stayed for the next recvmsg = {}",
        refused.ret == -EINVAL,
        refused.namelen,
        next.ret == 1 && next.byte == b'e'
    );

    server.write_all(b"f").unwrap_or_else(|_| fail("write failed", 91));
    let got = recv(
        client.as_raw_fd(),
        &Ask { with_name: false, namelen: -1, with_control: true, controllen: 64, flags: 0 },
    );
    println!("{NAME}: with no name and msg_namelen -1: returned {}, msg_namelen {}", got.ret, got.namelen);

    let got = recv(
        client.as_raw_fd(),
        &Ask { with_name: true, namelen: 128, with_control: true, controllen: 64, flags: MSG_DONTWAIT },
    );
    println!(
        "{NAME}: with nothing to read: returned EAGAIN = {}, msg_namelen {}, msg_flags {:#x}, msg_controllen {}, the name untouched = {}",
        got.ret == -EAGAIN,
        got.namelen,
        got.flags,
        got.controllen,
        untouched(&got.name)
    );

    // 2. SCM_RIGHTS
    let memfd = new_memfd();
    let fd_ask = |with_control, controllen, flags| Ask { with_name: true, namelen: 128, with_control, controllen, flags };
    for (label, with_control, controllen, flags) in [
        ("64 bytes of control", true, 64, 0),
        ("64 bytes of control and MSG_CMSG_CLOEXEC", true, 64, MSG_CMSG_CLOEXEC),
        ("20 bytes of control", true, 20, 0),
        ("no control", false, 0, 0),
        ("16 bytes of control", true, 16, 0),
        ("19 bytes of control", true, 19, 0),
    ] {
        let lowest = lowest_free_fd();
        send_with_fd(client.as_raw_fd(), memfd);
        let got = recv(server.as_raw_fd(), &fd_ask(with_control, controllen, flags));
        let fd = received_fd(&got);
        let cloexec = fd.map(|fd| {
            // SAFETY: `fcntl` は整数だけを受ける。
            (unsafe { fcntl(fd, F_GETFD) }) & FD_CLOEXEC != 0
        });
        if let Some(fd) = fd {
            // SAFETY: いま受けた fd を閉じるだけである。
            unsafe { close(fd) };
        }
        println!(
            "{NAME}: an fd with {label}: returned {}, msg_flags {:#x}, msg_controllen {}, received an fd = {}, FD_CLOEXEC = {}, a new fd was left open = {}, the control untouched past msg_controllen = {}",
            got.ret,
            got.flags,
            got.controllen,
            fd.is_some(),
            cloexec.unwrap_or(false),
            lowest_free_fd() != lowest,
            untouched(&got.control[got.controllen.min(64)..])
        );
    }

    // SAFETY: このプログラムが開いた fd を閉じるだけである。
    unsafe { close(memfd) };
    drop(client);
    drop(server);
    drop(listener);
    let _ = fs::remove_file(PATH);
    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
