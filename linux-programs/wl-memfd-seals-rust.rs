//! `wl-memfd-seals-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**memfd のフラグと名前、
//! 封（`F_ADD_SEALS`・`F_GET_SEALS`）、memfd の `fstat`、作り方ごとの `FD_CLOEXEC` を、Linux と同じ答えになるかで見る**
//! （2026-10-20。`ADR-0065` の Addendum）。
//!
//! # 何をするか（引数が `/etc/motd a b` のとき）
//!
//! 1. `memfd_create` の答え: 知らないフラグ（0x100）は `EINVAL`、名前は 249 バイトまで通り、250 バイトは `EINVAL`。
//! 2. 封: `MFD_ALLOW_SEALING` 付きの memfd は封が 0 から始まり、`F_SEAL_SHRINK|F_SEAL_GROW` を足すと 6、`F_SEAL_SEAL` を
//!    足すと 7、その後の `F_SEAL_WRITE` は `EPERM`、知らないビット（0x40）は `EINVAL`、`F_SEAL_EXEC` は通る。
//!    **足した封が `F_GET_SEALS` に出なければ、名指しの行を出して 94 で終わる。**
//!    `MFD_ALLOW_SEALING` の無い memfd は、最初から `F_SEAL_SEAL`（1）を持ち、封を足すと `EPERM`。
//!    **そこで封が足せてしまったら、名指しの行を出して 93 で終わる。** memfd でない fd（eventfd）への封は `EINVAL`。
//! 3. `fstat`: 8192 バイトへ `ftruncate` して両方のページに書いた memfd は、種類が普通のファイルで許可が 0777、大きさ 8192、
//!    `st_blksize` 4096、`st_blocks` 16。
//! 4. `FD_CLOEXEC`: `open`（`O_CLOEXEC`）・`socket` と `accept4`（`SOCK_CLOEXEC`）・eventfd・timerfd・epoll・memfd の
//!    `*_CLOEXEC` と、`F_DUPFD_CLOEXEC`、`recvmsg` の `MSG_CMSG_CLOEXEC` で受け取った fd は `F_GETFD` が 1、どれも付けなければ 0。
//!    `F_SETFD` で立てて下ろせる。
//!
//! **memfd はすぐ閉じる**——ZeikOS の memfd はシステムで 2 つ、fd はプロセスで 16 までなので、同時に持つ数を抑える。
//!
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-memfd-seals-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

const NAME: &str = "wl-memfd-seals-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/mf.sock";

const MFD_CLOEXEC: u32 = 0x0001;
const MFD_ALLOW_SEALING: u32 = 0x0002;
const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_DUPFD_CLOEXEC: i32 = 1030;
const F_ADD_SEALS: i32 = 1033;
const F_GET_SEALS: i32 = 1034;
const F_SEAL_SEAL: i32 = 0x0001;
const F_SEAL_SHRINK: i32 = 0x0002;
const F_SEAL_GROW: i32 = 0x0004;
const F_SEAL_WRITE: i32 = 0x0008;
const F_SEAL_EXEC: i32 = 0x0020;
const O_CLOEXEC: i32 = 0o2000000;
const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
const SOCK_CLOEXEC: i32 = 0o2000000;
const EFD_CLOEXEC: i32 = 0o2000000;
const TFD_CLOEXEC: i32 = 0o2000000;
const EPOLL_CLOEXEC: i32 = 0o2000000;
const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;
const CLOCK_MONOTONIC: i32 = 1;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;

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
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn ftruncate(fd: i32, length: i64) -> i32;
    fn fstat(fd: i32, buf: *mut u8) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
    fn accept4(fd: i32, addr: *mut u8, len: *mut u32, flags: i32) -> i32;
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn timerfd_create(clockid: i32, flags: i32) -> i32;
    fn epoll_create1(flags: i32) -> i32;
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
    fn close(fd: i32) -> i32;
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// 戻り値が負なら errno の符号を変えた値、そうでなければ戻り値。
fn result(value: i32) -> i32 {
    if value < 0 {
        -errno()
    } else {
        value
    }
}

fn memfd(name: &[u8], flags: u32) -> i32 {
    // SAFETY: `name` は NUL で終わるバイト列で、この関数の中で生きている。
    result(unsafe { memfd_create(name.as_ptr(), flags) })
}

fn get_seals(fd: i32) -> i32 {
    // SAFETY: `F_GET_SEALS` は引数を読まない。
    result(unsafe { fcntl(fd, F_GET_SEALS) })
}

fn add_seals(fd: i32, seals: i32) -> i32 {
    // SAFETY: `F_ADD_SEALS` は整数の引数を読むだけである。
    result(unsafe { fcntl(fd, F_ADD_SEALS, seals) })
}

fn close_all(fds: &[i32]) {
    for fd in fds {
        if *fd >= 0 {
            // SAFETY: このプロセスが開いた fd を閉じるだけである。
            unsafe { close(*fd) };
        }
    }
}

fn cloexec(fd: i32) -> i32 {
    // SAFETY: `F_GETFD` は引数を読まない。
    result(unsafe { fcntl(fd, F_GETFD) })
}

/// `sendmsg` で 1 バイトと fd を送る。
fn send_with_fd(socket: i32, fd: i32) -> isize {
    let mut byte = [b'y'];
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

/// `recvmsg` で 1 バイトと fd を受ける。**届いた fd（無ければ -1）を返す。**
fn recv_fd(socket: i32, flags: i32) -> i32 {
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
    unsafe { recvmsg(socket, &mut msg, flags) };
    if cmsg.level == SOL_SOCKET && cmsg.kind == SCM_RIGHTS {
        cmsg.fd
    } else {
        -1
    }
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");

    // 1. memfd_create の答え。
    println!("{NAME}: memfd_create with an unknown flag (0x100) gives {}", memfd(b"x\0", 0x100));
    let mut long = vec![b'a'; 249];
    long.push(0);
    let fits = memfd(&long, 0);
    println!("{NAME}: a 249-byte name is taken = {}", fits >= 0);
    close_all(&[fits]);
    let mut too_long = vec![b'a'; 250];
    too_long.push(0);
    println!("{NAME}: a 250-byte name gives {}", memfd(&too_long, 0));

    // 2. 封。
    let sealable = memfd(b"sealable\0", MFD_ALLOW_SEALING);
    println!("{NAME}: a memfd with MFD_ALLOW_SEALING starts with seals {}", get_seals(sealable));
    let added = add_seals(sealable, F_SEAL_SHRINK | F_SEAL_GROW);
    let after = get_seals(sealable);
    if added == 0 && after != F_SEAL_SHRINK | F_SEAL_GROW {
        fail(&format!("F_GET_SEALS returned {after} after F_SEAL_SHRINK|F_SEAL_GROW were added"), 94);
    }
    println!("{NAME}: adding F_SEAL_SHRINK|F_SEAL_GROW gives {added} and the seals become {after}");
    println!(
        "{NAME}: adding F_SEAL_SEAL gives {} and the seals become {}",
        add_seals(sealable, F_SEAL_SEAL),
        get_seals(sealable)
    );
    println!("{NAME}: adding F_SEAL_WRITE after F_SEAL_SEAL gives {}", add_seals(sealable, F_SEAL_WRITE));
    close_all(&[sealable]);
    let fresh = memfd(b"fresh\0", MFD_ALLOW_SEALING);
    println!("{NAME}: adding an unknown seal bit (0x40) gives {}", add_seals(fresh, 0x40));
    println!(
        "{NAME}: adding F_SEAL_EXEC gives {} and the seals become {}",
        add_seals(fresh, F_SEAL_EXEC),
        get_seals(fresh)
    );
    close_all(&[fresh]);
    let unsealable = memfd(b"unsealable\0", 0);
    let starts = get_seals(unsealable);
    let refused = add_seals(unsealable, F_SEAL_SHRINK);
    if refused == 0 {
        fail("a memfd made without MFD_ALLOW_SEALING took F_SEAL_SHRINK", 93);
    }
    println!("{NAME}: a memfd without MFD_ALLOW_SEALING starts with seals {starts} and adding a seal gives {refused}");
    close_all(&[unsealable]);
    // SAFETY: 引数は整数だけである。
    let efd = unsafe { eventfd(0, 0) };
    println!(
        "{NAME}: sealing an eventfd gives {} and asking its seals gives {}",
        add_seals(efd, F_SEAL_SHRINK),
        get_seals(efd)
    );

    // 3. fstat。
    let sized = memfd(b"sized\0", 0);
    let mut stat = [0u8; 144];
    // SAFETY: `stat` は 144 バイトの `struct stat` の置き場で、mmap の範囲は ftruncate の大きさの内である。
    unsafe {
        ftruncate(sized, 8192);
        let base = mmap(std::ptr::null_mut(), 8192, PROT_READ | PROT_WRITE, MAP_SHARED, sized, 0);
        if base as isize == -1 {
            fail("mmap of the memfd failed", 89);
        }
        base.write(1);
        base.add(4096).write(2);
        fstat(sized, stat.as_mut_ptr());
    }
    let u64_at = |at: usize| u64::from_le_bytes(stat[at..at + 8].try_into().unwrap_or([0; 8]));
    let mode = u32::from_le_bytes(stat[24..28].try_into().unwrap_or([0; 4]));
    println!(
        "{NAME}: fstat of an 8192-byte memfd gives mode {mode:o}, size {}, blksize {}, blocks {}",
        u64_at(48),
        u64_at(56),
        u64_at(64)
    );

    // 4. FD_CLOEXEC。
    let path = std::ffi::CString::new(std::env::args().nth(1).unwrap_or_default()).unwrap_or_default();
    // SAFETY: 以下の FFI はどれも、この関数の中で生きている値と整数だけを渡す。
    let (opened, plain_open, sock, plain_sock, efd_cloexec, tfd, ep, mfd) = unsafe {
        (
            open(path.as_ptr().cast(), O_CLOEXEC),
            open(path.as_ptr().cast(), 0),
            socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0),
            socket(AF_UNIX, SOCK_STREAM, 0),
            eventfd(0, EFD_CLOEXEC),
            timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC),
            epoll_create1(EPOLL_CLOEXEC),
            memfd_create(b"cloexec\0".as_ptr(), MFD_CLOEXEC),
        )
    };
    println!(
        "{NAME}: F_GETFD of fds made with O_CLOEXEC, SOCK_CLOEXEC, EFD_CLOEXEC, TFD_CLOEXEC, EPOLL_CLOEXEC and MFD_CLOEXEC \
         gives {} {} {} {} {} {}",
        cloexec(opened),
        cloexec(sock),
        cloexec(efd_cloexec),
        cloexec(tfd),
        cloexec(ep),
        cloexec(mfd)
    );
    println!(
        "{NAME}: F_GETFD of fds made without them gives {} {} {}",
        cloexec(plain_open),
        cloexec(plain_sock),
        cloexec(efd)
    );
    // SAFETY: 引数は整数だけである。
    let (dup_cloexec, dup_plain) = unsafe { (fcntl(mfd, F_DUPFD_CLOEXEC, 0), fcntl(mfd, F_DUPFD, 0)) };
    println!(
        "{NAME}: F_DUPFD_CLOEXEC gives a copy with {} and F_DUPFD a copy with {}",
        cloexec(dup_cloexec),
        cloexec(dup_plain)
    );
    close_all(&[opened, plain_open, sock, efd_cloexec, tfd, ep, mfd, dup_cloexec, dup_plain, efd]);
    // SAFETY: 引数は整数だけである。
    let set = unsafe { fcntl(plain_sock, F_SETFD, 1) };
    let raised = cloexec(plain_sock);
    // SAFETY: 引数は整数だけである。
    let cleared = unsafe { fcntl(plain_sock, F_SETFD, 0) };
    println!("{NAME}: F_SETFD 1 gives {set} and then {raised}; F_SETFD 0 gives {cleared} and then {}", cloexec(plain_sock));
    close_all(&[plain_sock]);

    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 88));
    let client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 88));
    // SAFETY: 相手の名前は受けない（NULL）。
    let server = unsafe { accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), SOCK_CLOEXEC) };
    println!("{NAME}: F_GETFD of a socket from accept4 with SOCK_CLOEXEC gives {}", cloexec(server));
    // **送っては受ける**（ZeikOS のソケットは、渡す fd を向きごとに 1 つまでしか待たせない）。
    send_with_fd(client.as_raw_fd(), sized);
    let with_flag = recv_fd(server, MSG_CMSG_CLOEXEC);
    send_with_fd(client.as_raw_fd(), sized);
    let without_flag = recv_fd(server, 0);
    println!(
        "{NAME}: F_GETFD of fds received with MSG_CMSG_CLOEXEC and without it gives {} {}",
        cloexec(with_flag),
        cloexec(without_flag)
    );
    close_all(&[sized, server, with_flag, without_flag]);
    drop(client);
    drop(listener);
    let _ = fs::remove_file(PATH);
    std::process::exit(argument_count as i32);
}
