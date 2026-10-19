//! `wl-nonblock-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**unix ドメインの
//! ソケットの待たない形（`O_NONBLOCK`・`FIONBIO`・`MSG_DONTWAIT`）と、待つ形の両方を、同じプロセスの中で通す**（2026-10-19）。
//! Seinas（wayland-server と calloop）は listener と接続を待たない形にして、`-EAGAIN` を「もう無い」の意味に使う。
//!
//! # 何をするか（引数が `/etc/motd a b` のとき）
//!
//! 1. `/tmp/seinas/nb.sock` に listener を作り、`set_nonblocking(true)`（`ioctl(FIONBIO)`）にする。スレッドが 500 ms 後に
//!    繋ぐ。主の `accept` は待たずに `WouldBlock` を返す。**繋がった接続を受けてしまったら、名指しの行を出して 95 で終わる**
//!    （待たない印を無視して待つ形。破壊テスト `socket-ignores-nonblocking`）。
//! 2. listener を待つ形に戻し、`accept` がスレッドの繋いだ接続を待って受ける（待つ側）。
//! 3. listener を待たない形にしたまま、繋いで `accept` する。**受けた fd は listener の印を引き継がない**
//!    （`fcntl(F_GETFL)` に `O_NONBLOCK` が無い）。
//! 4. 受けた fd を待たない形にして読むと `WouldBlock`。待つ形に戻すと、スレッドが 500 ms 後に書いたバイトを待って読む。
//! 5. 待つ形の fd へ `recv(MSG_DONTWAIT)` を打つと、`EAGAIN`。スレッドが 500 ms 後に書く。**待ってデータを受けてしまったら、
//!    名指しの行を出して 96 で終わる**（`MSG_DONTWAIT` を無視する形。破壊テスト `msg-dontwait-ignored`）。
//! 6. 送る側を待たない形にして、`WouldBlock` まで書いて満たす。満ちたソケットへ `SCM_RIGHTS` の memfd を付けた
//!    `sendmsg(MSG_DONTWAIT)` は `EAGAIN`。受ける側が `recvmsg` で全部を読むと、バイトは順のとおりで、fd は来ない。
//!    **fd が来たら、名指しの行を出して 94 で終わる**（`-EAGAIN` でも fd を渡す形。破壊テスト `eagain-passes-fd`）。
//!    空いた後の `sendmsg` は fd を渡し、受ける側が `recvmsg` で受けてから閉じる。
//! 7. 待つ形のソケットへ `writev` で 1,500 バイトと 10 バイトを書き、短く返った分は残りから書き直す。スレッドが 300 ms 後から
//!    読み、1,510 バイトが順のとおりに届くこと（後ろの iovec が先に入らないこと）を見る。
//!
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-nonblock-rust.txt`）。
//!
//! # `writev-pipe` で起こしたとき
//!
//! `zash` のパイプの左（fd 1 がパイプの書き端）から、`writev` で 300 バイトと 10 バイトを書き、短く返った分は残りから
//! 書き直す（パイプの輪は 256 バイトなので、1 本目は必ず短く返る）。右の `cat` が出す 1 行が、300 バイトの後に
//! `|tail-ok|` が続く形であることを、`pipe-test` が見る。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs::{self, File};
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/nb.sock";
const NAME: &str = "wl-nonblock-rust";

const F_GETFL: i32 = 3;
const O_NONBLOCK: i32 = 0o4000;
const MSG_DONTWAIT: i32 = 0x40;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MFD_CLOEXEC: u32 = 1;

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
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize;
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
}

fn byte_at(position: usize) -> u8 {
    (position % 251) as u8
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

fn has_nonblock(fd: i32) -> bool {
    // SAFETY: `fcntl(F_GETFL)` は引数を読まない。
    let flags = unsafe { fcntl(fd, F_GETFL) };
    flags >= 0 && flags & O_NONBLOCK != 0
}

/// `sendmsg` で 1 バイトと fd を送る。
fn send_with_fd(socket: i32, fd: i32, flags: i32) -> isize {
    let mut byte = [b'y'];
    let mut iov = IoVec {
        base: byte.as_mut_ptr(),
        len: 1,
    };
    let mut cmsg = CmsgOneFd {
        len: 20,
        pad: 0,
        level: SOL_SOCKET,
        kind: SCM_RIGHTS,
        fd,
        tail: 0,
    };
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
    unsafe { sendmsg(socket, &msg, flags) }
}

/// `recvmsg` で読む。**読んだバイト数と、届いた fd（無ければ `None`）を返す。** 制御の置き場は毎回 0 で埋める。
fn recv_with_fd(socket: i32, buf: &mut [u8]) -> (isize, Option<i32>) {
    let mut iov = IoVec {
        base: buf.as_mut_ptr(),
        len: buf.len(),
    };
    let mut cmsg = CmsgOneFd {
        len: 0,
        pad: 0,
        level: 0,
        kind: 0,
        fd: -1,
        tail: 0,
    };
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
    let fd = (cmsg.level == SOL_SOCKET && cmsg.kind == SCM_RIGHTS).then_some(cmsg.fd);
    (got, fd)
}

/// `writev` で `slices` を全部書く。**短く返ったら、書けた分だけ進めて書き直す。**
fn writev_all(file: &mut impl Write, slices: &mut [IoSlice<'_>]) -> std::io::Result<()> {
    let mut slices = slices;
    while !slices.is_empty() {
        let wrote = file.write_vectored(slices)?;
        IoSlice::advance_slices(&mut slices, wrote);
    }
    Ok(())
}

fn writev_pipe() {
    let head: Vec<u8> = (0..300).map(|i| b'a' + (i % 26) as u8).collect();
    let tail = b"|tail-ok|\n";
    // SAFETY: fd 1 はパイプの書き端で、このプロセスが終わるまで開いている。
    let mut out = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let mut slices = [IoSlice::new(&head), IoSlice::new(tail)];
    if writev_all(&mut *out, &mut slices).is_err() {
        std::process::exit(93);
    }
    std::process::exit(0);
}

fn main() {
    let argument_count = std::env::args().count();
    if std::env::args().nth(1).as_deref() == Some("writev-pipe") {
        writev_pipe();
    }
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let _ = fs::create_dir_all(DIR);
    let _ = fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));

    // 1. 待たない listener の accept は WouldBlock。
    listener.set_nonblocking(true).unwrap_or_else(|_| fail("set_nonblocking on the listener failed", 90));
    let late = thread::spawn(|| {
        thread::sleep(Duration::from_millis(500));
        UnixStream::connect(PATH)
    });
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            println!("{NAME}: accept on a listener that does not wait returned WouldBlock = true");
        }
        Ok(_) => fail("accept on a listener that does not wait took a connection instead of WouldBlock", 95),
        Err(error) => fail(&format!("accept on a listener that does not wait failed: {error}"), 90),
    }
    println!("{NAME}: the listener has O_NONBLOCK = {}", has_nonblock(listener.as_raw_fd()));

    // 2. 待つ listener の accept は、後から繋がった接続を受ける。
    listener.set_nonblocking(false).unwrap_or_else(|_| fail("set_nonblocking(false) on the listener failed", 90));
    let accepted_late = listener.accept().is_ok();
    let late_client = late.join().ok().and_then(Result::ok);
    println!(
        "{NAME}: accept on a listener that waits got the connection made later = {}",
        accepted_late && late_client.is_some()
    );
    drop(late_client);

    // 3. 待たない listener から受けた fd は、印を引き継がない。
    listener.set_nonblocking(true).unwrap_or_else(|_| fail("set_nonblocking on the listener failed", 90));
    let mut client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (mut server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    println!(
        "{NAME}: the fd accepted from a listener that does not wait has O_NONBLOCK = {}",
        has_nonblock(server.as_raw_fd())
    );

    // 4. 待たない読みは WouldBlock、待つ読みは後から書かれたバイトを受ける。
    server.set_nonblocking(true).unwrap_or_else(|_| fail("set_nonblocking on the stream failed", 90));
    let mut buf = [0u8; 4096];
    let would_block = matches!(server.read(&mut buf), Err(error) if error.kind() == ErrorKind::WouldBlock);
    println!("{NAME}: a read on a stream that does not wait returned WouldBlock = {would_block}");
    server.set_nonblocking(false).unwrap_or_else(|_| fail("set_nonblocking(false) on the stream failed", 90));
    // **スレッドへは fd を貸す**（`try_clone` でコピーすると、コピーを閉じたときに ZeikOS では端そのものが閉じる）。
    let (got, wrote) = thread::scope(|scope| {
        let late = scope.spawn(|| {
            thread::sleep(Duration::from_millis(500));
            (&client).write_all(b"late").is_ok()
        });
        let got = server.read(&mut buf).unwrap_or(0);
        (got, late.join().unwrap_or(false))
    });
    println!(
        "{NAME}: a read on a stream that waits got the bytes written later = {}",
        if wrote { String::from_utf8_lossy(&buf[..got]).into_owned() } else { String::from("(no write)") }
    );

    // 5. 待つ fd への recv(MSG_DONTWAIT) は EAGAIN。
    let (got, wrote) = thread::scope(|scope| {
        let late = scope.spawn(|| {
            thread::sleep(Duration::from_millis(500));
            (&client).write_all(b"later").is_ok()
        });
        // SAFETY: `buf` はこの関数の中で生きている。
        let dontwait = unsafe { recv(server.as_raw_fd(), buf.as_mut_ptr(), buf.len(), MSG_DONTWAIT) };
        if dontwait > 0 {
            fail("recv with MSG_DONTWAIT on an empty stream waited and got bytes instead of EAGAIN", 96);
        }
        let again = std::io::Error::last_os_error().kind() == ErrorKind::WouldBlock;
        println!("{NAME}: recv with MSG_DONTWAIT on a stream that waits returned EAGAIN = {}", dontwait < 0 && again);
        let got = server.read(&mut buf).unwrap_or(0);
        (got, late.join().unwrap_or(false))
    });
    println!(
        "{NAME}: the read after it waited for the bytes written later = {}",
        if wrote { String::from_utf8_lossy(&buf[..got]).into_owned() } else { String::from("(no write)") }
    );

    // 6. 満たしたソケットへの sendmsg(MSG_DONTWAIT) は fd を渡さずに EAGAIN。
    client.set_nonblocking(true).unwrap_or_else(|_| fail("set_nonblocking on the client failed", 90));
    let mut chunk = vec![0u8; 1500];
    let mut total = 0usize;
    let mut first = None;
    loop {
        for (offset, byte) in chunk.iter_mut().enumerate() {
            *byte = byte_at(total + offset);
        }
        match client.write(&chunk) {
            Ok(wrote) => {
                first.get_or_insert(wrote);
                total += wrote;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => fail(&format!("writing to fill the stream failed: {error}"), 90),
        }
    }
    println!("{NAME}: the first write to a stream that does not wait took some bytes = {}", first.is_some_and(|n| n > 0));
    println!("{NAME}: filling a stream that does not wait ended with WouldBlock = true");
    // SAFETY: 名前は NUL で終わる。
    let memfd = unsafe { memfd_create(b"nb\0".as_ptr(), MFD_CLOEXEC) };
    if memfd < 0 {
        fail("memfd_create failed", 90);
    }
    let sent = send_with_fd(client.as_raw_fd(), memfd, MSG_DONTWAIT);
    let again = sent < 0 && std::io::Error::last_os_error().kind() == ErrorKind::WouldBlock;
    println!("{NAME}: sendmsg with an fd on a full stream returned EAGAIN = {again}");
    let mut read = 0usize;
    let mut in_order = true;
    while read < total {
        let (got, fd) = recv_with_fd(server.as_raw_fd(), &mut buf);
        if let Some(fd) = fd {
            // SAFETY: 届いた fd は、このプロセスの表に置かれたものである。
            drop(unsafe { File::from_raw_fd(fd) });
            fail("an fd arrived with the bytes sent before it, though sendmsg had returned EAGAIN", 94);
        }
        if got <= 0 {
            fail("recvmsg while draining the stream returned nothing", 90);
        }
        for (offset, byte) in buf[..got as usize].iter().enumerate() {
            in_order &= *byte == byte_at(read + offset);
        }
        read += got as usize;
    }
    println!("{NAME}: the reader got every byte in order and no fd = {in_order}");
    let sent = send_with_fd(client.as_raw_fd(), memfd, MSG_DONTWAIT);
    let (got, fd) = recv_with_fd(server.as_raw_fd(), &mut buf);
    println!(
        "{NAME}: after the drain, sendmsg passed the fd and the reader took it = {}",
        sent == 1 && got == 1 && fd.is_some()
    );
    if let Some(fd) = fd {
        // SAFETY: 届いた fd は、このプロセスの表に置かれたものである。
        drop(unsafe { File::from_raw_fd(fd) });
    }
    // SAFETY: `memfd` はこのプロセスが作った fd で、ここで手放す。
    drop(unsafe { File::from_raw_fd(memfd) });

    // 7. writev が短く返っても、後ろの iovec を先に書かない。
    client.set_nonblocking(false).unwrap_or_else(|_| fail("set_nonblocking(false) on the client failed", 90));
    let head: Vec<u8> = (0..1500).map(byte_at).collect();
    let tail: Vec<u8> = (1500..1510).map(byte_at).collect();
    let (wrote_all, in_order) = thread::scope(|scope| {
        let check = scope.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            let mut all = vec![0u8; 1510];
            (&server).read_exact(&mut all).is_ok() && all.iter().enumerate().all(|(at, byte)| *byte == byte_at(at))
        });
        let mut slices = [IoSlice::new(&head), IoSlice::new(&tail)];
        let wrote_all = writev_all(&mut &client, &mut slices).is_ok();
        (wrote_all, check.join().unwrap_or(false))
    });
    println!(
        "{NAME}: a writev that came back short did not put the later iovec first on the stream = {}",
        wrote_all && in_order
    );

    drop(client);
    drop(server);
    drop(listener);
    let _ = fs::remove_file(PATH);
    std::process::exit(argument_count as i32);
}
