//! `wl-close-queued-fd-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**受け取られない
//! まま接続が閉じられた `SCM_RIGHTS` の memfd が、閉じた後に手放されること**（2026-10-27。`ADR-0065` の Addendum）。
//!
//! # 何をするか
//!
//! 接続を作り、memfd を 1 つ `SCM_RIGHTS` で送り、送った側の memfd を閉じ、受ける側は受け取らないまま両端を閉じる。これを
//! 閉じる順の 3 つの形で、それぞれ 20 回くり返す（ZeikOS の共有メモリの上限 16 より多い）。
//!
//! 1. 受ける側が先に閉じる。
//! 2. 送った側が先に閉じ、後から受ける側が閉じる。
//! 3. 受ける側が閉じた後に送る（`sendmsg` は `EPIPE`。`MSG_NOSIGNAL` で `SIGPIPE` を送らせない）。
//!
//! 閉じた接続に残った memfd が手放されなければ、ZeikOS では共有メモリの上限で `memfd_create` が断られる。Linux は、閉じた
//! ソケットの受信の待ち行列を捨てるので、何回でも作れる。
//!
//! 行はどれも標準出力へ出す。終了ステータスは引数の数（4）。Linux でも同じ行を出す
//! （`linux-programs/reference/wl-close-queued-fd-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::os::fd::IntoRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

const NAME: &str = "wl-close-queued-fd-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/close-queued-fd.sock";
/// 1 つの形でくり返す回数（ZeikOS の共有メモリの上限 16 より多い）。
const ROUNDS: usize = 20;

const SYS_SENDMSG: i64 = 46;
const MFD_CLOEXEC: u32 = 1;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_NOSIGNAL: i64 = 0x4000;
const EPIPE: i64 = 32;

/// `struct iovec`。
#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

/// Linux の x86_64 の `struct msghdr`（56 バイト）。
#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    namelen: u32,
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

fn close_fd(fd: i32) {
    // SAFETY: このプログラムが開いた fd を閉じるだけである。
    unsafe { close(fd) };
}

/// `sendmsg` で 1 バイトと fd を送る。戻り値（失敗なら負の errno）。
fn send_with_fd(socket: i32, fd: i32) -> i64 {
    let mut byte = [b'q'];
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
    let ret = unsafe { syscall(SYS_SENDMSG, socket as i64, &msg as *const MsgHdr, MSG_NOSIGNAL) };
    if ret < 0 {
        -errno()
    } else {
        ret
    }
}

/// 閉じる順の形。
#[derive(Clone, Copy)]
enum Order {
    ReceiverFirst,
    SenderFirst,
    SendAfterReceiverClosed,
}

/// 1 回分。memfd を作れたか、`sendmsg` の戻り値。
fn round(listener: &UnixListener, order: Order) -> (bool, i64) {
    let client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    // SAFETY: NUL で終わる名前。
    let memfd = unsafe { memfd_create(b"close-queued\0".as_ptr(), MFD_CLOEXEC) };
    if memfd < 0 {
        return (false, 0);
    }
    let client = client.into_raw_fd();
    let server = server.into_raw_fd();
    let sent = match order {
        Order::ReceiverFirst => {
            let sent = send_with_fd(client, memfd);
            close_fd(memfd);
            close_fd(server);
            close_fd(client);
            sent
        }
        Order::SenderFirst => {
            let sent = send_with_fd(client, memfd);
            close_fd(memfd);
            close_fd(client);
            close_fd(server);
            sent
        }
        Order::SendAfterReceiverClosed => {
            close_fd(server);
            let sent = send_with_fd(client, memfd);
            close_fd(memfd);
            close_fd(client);
            sent
        }
    };
    (true, sent)
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));

    for (label, order, expected) in [
        ("the receiver closed first", Order::ReceiverFirst, 1),
        ("the sender closed first", Order::SenderFirst, 1),
        ("sent after the receiver closed", Order::SendAfterReceiverClosed, -EPIPE),
    ] {
        let mut made = 0;
        let mut sent_as_expected = 0;
        for _ in 0..ROUNDS {
            let (created, sent) = round(&listener, order);
            if !created {
                break;
            }
            made += 1;
            if sent == expected {
                sent_as_expected += 1;
            }
        }
        println!(
            "{NAME}: {label}: made {made} of {ROUNDS} memfds, and sendmsg returned {} in {sent_as_expected} of them",
            if expected < 0 { "EPIPE" } else { "1" }
        );
    }

    drop(listener);
    let _ = fs::remove_file(PATH);
    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
