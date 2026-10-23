//! `wl-iovec-threads-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**パイプの
//! `writev`・`readv` で眠っている間に、同じプロセスの別のスレッドが `writev` しても、眠っていた側の iovec がすり替わらないこと**
//! （2026-10-23。`ADR-0085` の Addendum）。
//!
//! # 何をするか
//!
//! 2 つの形を、この順に通す。
//!
//! 1. `readv` の形: 読み手のスレッドが、空のパイプで `readv(0, {4 バイト, 4 バイト})` を打って眠る。主スレッドは眠ったことを
//!    確かめてから `writev(2, …)` で 1 行出し、パイプへ `ccccdddd` を書く。読み手の 2 つの置き場が `cccc` と `dddd` で、主スレッドの
//!    置き場が書き換わっていないことを見る。
//! 2. `writev` の形: fd 1 のパイプを 256 バイト書いて満たす（ZeikOS の輪は 256 バイトなので満杯になる）。書き手のスレッドが
//!    `writev(1, {"aaaa", "bbbb"})` を打つ。主スレッドは書き手が始まったのを見て譲った後、`writev(2, …)` で 1 行出す。それから
//!    パイプを 256 バイト読んで空け、続く 8 バイトが `aaaabbbb` であることを見る。
//!
//! `readv` の形を先に通すのは、`writev` の形で 2 本目がすり替わると、余ったバイトがパイプに残り、続く `readv` が眠らなく
//! なるためである。
//!
//! Linux のパイプは 64 KiB なので、`writev` の形の書き手は眠らない。行は同じになる（Linux は iovec を呼び出しごとにカーネルへ取り込む）。
//!
//! # 眠ったことの確かめ（`readv` の形。経過時間に頼らない）
//!
//! - Linux では `/proc/self/task/<tid>/stat` の状態が `S` で、`/proc/self/task/<tid>/syscall` の番号が `readv`（19）であること。
//! - ZeikOS には `/proc` が無いので、`linux-socket-test` のビルドだけが持つ確かめ用の番号（`0x100A`。その fd が読めるのを
//!   待って眠っているタスクが居れば 1）で見る。
//!
//! # 標準入出力
//!
//! **fd 0 と fd 1 は同じパイプの読み端と書き端である**（`wl-fault-after-wait-rust` と同じ形）。行はどれも**標準エラーへ**出す。
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-iovec-threads-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

const NAME: &str = "wl-iovec-threads-rust";
const SYS_READV: i64 = 19;
const SYS_GETTID: i64 = 186;
/// ZeikOS の確かめ用の番号（`linux-socket-test` のビルドだけが持つ。Linux では打たない）。
const ZEIKOS_SLEEPS_READING: i64 = 0x100A;
/// 眠ったことを見直す回数の上限。
const LOOKS: usize = 10_000;
/// `writev` の形で、書き手が眠るまで主スレッドが譲る回数。
const YIELDS: usize = 200;

#[repr(C)]
struct IoVec {
    base: *const u8,
    len: usize,
}

extern "C" {
    fn writev(fd: i32, iov: *const IoVec, count: i32) -> isize;
    fn readv(fd: i32, iov: *const IoVec, count: i32) -> isize;
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn sched_yield() -> i32;
    fn syscall(number: i64, ...) -> i64;
}

fn fail(what: &str, code: i32) -> ! {
    eprintln!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// 主スレッドから、ほかの流れの iovec を上書きしうる `writev` を 1 回打つ（端末へ 1 行出す）。2 本目は `second` を指す。
fn another_writev(second: &[u8]) {
    let first = format!("{NAME}: another thread ");
    let iov = [
        IoVec { base: first.as_ptr(), len: first.len() },
        IoVec { base: second.as_ptr(), len: second.len() },
    ];
    // SAFETY: 2 本とも生きている置き場。端末へ書くので眠らない。
    unsafe { writev(2, iov.as_ptr(), 2) };
}

/// `fd` から `len` バイトを読み切る（足りなければ読めた分）。
fn read_exact(fd: i32, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut have = 0usize;
    for _ in 0..LOOKS {
        if have == len {
            break;
        }
        // SAFETY: 残りの置き場。
        let n = unsafe { read(fd, buf[have..].as_mut_ptr(), len - have) };
        if n <= 0 {
            break;
        }
        have += n as usize;
    }
    buf.truncate(have);
    buf
}

/// tid のスレッドが、fd が読めるのを待って `readv` で眠っているか。
fn sleeps_in_readv(tid: i64, fd: i32) -> bool {
    let base = format!("/proc/self/task/{tid}");
    match (fs::read_to_string(format!("{base}/stat")), fs::read_to_string(format!("{base}/syscall"))) {
        (Ok(stat), Ok(syscall)) => {
            let state = stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next());
            let current = syscall.split_whitespace().next().and_then(|n| n.parse::<i64>().ok());
            state == Some('S') && current == Some(SYS_READV)
        }
        // SAFETY: 確かめ用の番号は fd を 1 つ取り、ポインタを読まない。
        _ => (unsafe { syscall(ZEIKOS_SLEEPS_READING, fd as i64) }) == 1,
    }
}

fn writev_shape() {
    let fill = [b'f'; 256];
    // SAFETY: 256 バイトの置き場。
    if unsafe { write(1, fill.as_ptr(), fill.len()) } != 256 {
        fail("filling the pipe failed", 89);
    }
    let started = Arc::new(AtomicBool::new(false));
    let started_set = Arc::clone(&started);
    let writer = std::thread::spawn(move || {
        let first = *b"aaaa";
        let second = *b"bbbb";
        let iov = [
            IoVec { base: first.as_ptr(), len: 4 },
            IoVec { base: second.as_ptr(), len: 4 },
        ];
        started_set.store(true, Ordering::SeqCst);
        // SAFETY: 2 本とも生きている置き場。
        unsafe { writev(1, iov.as_ptr(), 2) }
    });
    while !started.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    for _ in 0..YIELDS {
        // SAFETY: 引数を取らない。
        unsafe { sched_yield() };
    }
    another_writev(b"wrote this line with writev\n");
    let drained = read_exact(0, 256);
    let got = read_exact(0, 8);
    let written = writer.join().unwrap_or(-1);
    eprintln!(
        "{NAME}: the writev on a full pipe sent its own two iovecs (aaaabbbb, 8 bytes) = {}",
        drained.len() == 256 && got == b"aaaabbbb" && written == 8
    );
}

fn readv_shape() {
    let tid = Arc::new(AtomicI64::new(0));
    let tid_set = Arc::clone(&tid);
    let reader = std::thread::spawn(move || {
        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
        let iov = [
            IoVec { base: first.as_mut_ptr(), len: 4 },
            IoVec { base: second.as_mut_ptr(), len: 4 },
        ];
        // SAFETY: 引数を取らない。
        tid_set.store(unsafe { syscall(SYS_GETTID) }, Ordering::SeqCst);
        // SAFETY: 2 本とも生きている置き場で、読む間は触らない。
        let got = unsafe { readv(0, iov.as_ptr(), 2) };
        (got, first, second)
    });
    let mut slept = false;
    for _ in 0..LOOKS {
        let reader_tid = tid.load(Ordering::SeqCst);
        if reader_tid != 0 && sleeps_in_readv(reader_tid, 0) {
            slept = true;
            break;
        }
        // SAFETY: 引数を取らない。
        unsafe { sched_yield() };
    }
    // 主スレッドの置き場。眠っている読み手の 2 本目がこれにすり替わると、パイプの `dddd` がここへ書かれる。
    let mine = *b"wrote this line with writev again\n";
    another_writev(&mine);
    // SAFETY: 8 バイトの置き場。
    if unsafe { write(1, b"ccccdddd".as_ptr(), 8) } != 8 {
        fail("writing to the pipe failed", 90);
    }
    let (got, first, second) = reader.join().unwrap_or((-1, [0; 4], [0; 4]));
    // SAFETY: 生きている配列を読むだけである。カーネルが `writev` の後に書き換えた場合も見えるように、読み直す。
    let mine = unsafe { std::ptr::read_volatile(&mine) };
    let mut total = got.max(0) as usize;
    let mut rest = Vec::new();
    if total < 8 {
        rest = read_exact(0, 8 - total);
        total += rest.len();
    }
    eprintln!("{NAME}: the reader slept in readv before another thread called writev = {slept}");
    eprintln!(
        "{NAME}: the readv filled its own two buffers (cccc and dddd) and left the main thread's alone = {}",
        got == 8
            && &first == b"cccc"
            && &second == b"dddd"
            && &mine == b"wrote this line with writev again\n"
            && total == 8
            && rest.is_empty()
    );
}

fn main() {
    let argument_count = std::env::args().count();
    eprintln!("{NAME}: main starts with {argument_count} argument(s)");
    // `readv` の形を先に通す（モジュールの doc）。
    readv_shape();
    writev_shape();
    eprintln!("{NAME}: both shapes are done");
    std::process::exit(argument_count as i32);
}
