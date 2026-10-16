//! `wl-socket-lock-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**Seinas
//! （wayland-server）が起動時に通す一連——ロックファイルを `O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC` で開き、`flock(LOCK_EX|LOCK_NB)`
//! で取り、`fstat` で確かめ、`socket` → `bind(パス)` → `listen` と進み、クライアントが同じパスへ `connect` する——を、
//! `std` の `UnixListener`・`UnixStream`・`File::try_lock` で通す**（`ADR-0087`。2026-10-16）。
//!
//! # 何をするか
//!
//! 1. `/tmp/seinas` の下に、`s`・`s-shell`・`s-ime` の 3 組（`.lock` と `.sock`。Seinas の 3 本の listen と同じ形）を置く。
//!    組ごとに、ロックファイルを開いて `try_lock`（`flock(LOCK_EX|LOCK_NB)`）で取り、同じファイルをもう 1 度開いた側の
//!    `try_lock` が `WouldBlock` で断られること、`fstat` が普通のファイルを返すことを見て、`UnixListener::bind` する。
//!    3 本の listener を持ったまま次へ進む（`MAX_LISTENERS` が 3 本以上であること）。
//! 2. 組ごとに、スレッドが `UnixStream::connect` で同じパスへ繋いで 1 行送り、主が `accept` して読み、返す。
//! 3. 組ごとに、1 本目の fd を閉じると 2 本目が取れること（`close` で放す）、`unlock` が通ることを見て、ファイルを消す。
//!    終了の状態は引数の数。**破壊テストが作る形では、名指しの行を出して 97（2 本目が取れてしまった）か 98（閉じても
//!    放されない）で終わる**（起動時の確かめが `expected Exited(4)` と名指しして止まる）。
//!
//! Linux でも同じ行を出す（`linux-programs/reference/wl-socket-lock-rust.txt`）。`/tmp/seinas` は無ければ作る。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである**（`--linux-socket-test` と、flock の
//! 破壊テスト 2 つ）。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};

const DIR: &str = "/tmp/seinas";
const NAMES: [&str; 3] = ["s", "s-shell", "s-ime"];

fn open_lock(name: &str) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(format!("{DIR}/{name}.lock"))
        .unwrap_or_else(|error| {
            println!("wl-socket-lock-rust: opening the lock file for {name} failed: {error}");
            std::process::exit(96)
        })
}

fn try_lock(file: &File) -> Result<bool, std::io::Error> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn main() {
    let argument_count = std::env::args().count();
    println!("wl-socket-lock-rust: main starts with {argument_count} argument(s)");
    let _ = fs::create_dir_all(DIR);
    // 1. ロックと listen を 3 組。
    let mut held = Vec::new();
    for name in NAMES {
        let first = open_lock(name);
        let taken = try_lock(&first).unwrap_or_else(|error| {
            println!("wl-socket-lock-rust: flock on {name} failed: {error}");
            std::process::exit(96)
        });
        println!("wl-socket-lock-rust: took the lock for {name} = {taken}");
        let second = open_lock(name);
        let refused = match try_lock(&second) {
            Ok(false) => true,
            Ok(true) => false,
            Err(error) => {
                println!("wl-socket-lock-rust: the second flock on {name} failed: {error}");
                std::process::exit(96)
            }
        };
        println!(
            "wl-socket-lock-rust: a second open of the lock for {name} is refused with EWOULDBLOCK = {refused}"
        );
        if !refused {
            println!("wl-socket-lock-rust: the second open took the lock for {name} while the first held it; exiting with 97");
            std::process::exit(97);
        }
        let regular = first.metadata().map(|m| m.is_file()).unwrap_or(false);
        println!("wl-socket-lock-rust: the lock file for {name} is a regular file = {regular}");
        let sock = format!("{DIR}/{name}.sock");
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap_or_else(|error| {
            println!("wl-socket-lock-rust: bind on {name} failed: {error}");
            std::process::exit(96)
        });
        println!("wl-socket-lock-rust: bound and listening on {name} = true");
        held.push((name, first, second, listener, sock));
    }
    println!("wl-socket-lock-rust: {} listeners are held at once", held.len());
    // 2. 組ごとに、スレッドが繋いで 1 往復。
    for (name, _, _, listener, sock) in &held {
        let path = sock.clone();
        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&path).expect("connect to the listener");
            stream.write_all(b"ping").expect("send ping");
            let mut reply = [0u8; 4];
            stream.read_exact(&mut reply).expect("read the reply");
            reply == *b"pong"
        });
        let (mut server, _peer) = listener.accept().expect("accept the client");
        let mut request = [0u8; 4];
        server.read_exact(&mut request).expect("read the request");
        server.write_all(b"pong").expect("send the reply");
        drop(server);
        let replied = client.join().unwrap_or(false);
        println!(
            "wl-socket-lock-rust: a client connected to {name}, sent ping and got pong = {}",
            request == *b"ping" && replied
        );
    }
    // 3. 閉じると放されること。
    for (name, first, second, listener, sock) in held {
        drop(first);
        let taken = match try_lock(&second) {
            Ok(taken) => taken,
            Err(error) => {
                println!("wl-socket-lock-rust: the flock after the close on {name} failed: {error}");
                std::process::exit(96)
            }
        };
        println!(
            "wl-socket-lock-rust: after closing the first, the second takes the lock for {name} = {taken}"
        );
        if !taken {
            println!("wl-socket-lock-rust: the lock for {name} stayed held after its holder closed; exiting with 98");
            std::process::exit(98);
        }
        let unlocked = second.unlock().is_ok();
        println!("wl-socket-lock-rust: unlock on {name} = {unlocked}");
        drop(second);
        drop(listener);
        let _ = fs::remove_file(&sock);
        let _ = fs::remove_file(format!("{DIR}/{name}.lock"));
    }
    std::process::exit(argument_count as i32);
}
