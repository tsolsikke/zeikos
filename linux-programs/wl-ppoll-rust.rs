//! `wl-ppoll-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**`poll`（7 番）と `ppoll`
//! （271 番）の待つ長さ、`ppoll` のシグナルのマスク・確かめの順・残りの長さの書き戻し**（2026-10-24。`ADR-0066` の Addendum）。
//!
//! # 何をするか
//!
//! unix ソケットの接続を 1 本作り、server 側の fd を見張る（ZeikOS の `poll` はパイプを受けないので、ソケットを使う）。番号は
//! `syscall` で直に打ち、引数をそのまま渡す。
//!
//! 1. 読める fd を、`tsp` が NULL・`sigmask` が NULL・`events` が `POLLIN | POLLERR`（Seinas のタスクバーと同じ形）で待つ。1 が返り、
//!    `revents` に `POLLIN` が立つ。
//! 2. 読めない fd を、0 秒 0 ナノ秒で待つ。0 が返る。
//! 3. 読めない fd を、30 ミリ秒で待つ。0 が返り、`tsp` に 0 秒 0 ナノ秒が書き戻される。
//! 4. 読める fd を、5 秒で待つ。1 が返り、`tsp` の残りは 5 秒以下で、ナノ秒は範囲の内にある。
//! 5. 別のスレッドが少し後に書く fd を、`tsp` が NULL で待つ。1 が返る（読めるまで眠り、起こされる）。
//! 6. 範囲の外の長さ（ナノ秒が 10 億、秒が -1）は `EINVAL`。
//! 7. 読めない `tsp`（外したページ）は `EFAULT`。`sigsetsize` も不正（4）にしても `EFAULT` である（`tsp` を先に見る）。
//! 8. `sigmask` を渡すとき、`sigsetsize` が 8 なら待ち、4 なら `EINVAL`。`sigmask` が NULL なら `sigsetsize` は見ない。
//!    読めない `sigmask` は `EFAULT`。
//! 9. `poll` の待つ長さ: -1（読める fd）、0、30 ミリ秒（時間切れ）、5 秒（その間に別のスレッドが書く）、-5（負は待ち続ける。
//!    別のスレッドが書く）。5 秒と -5 は、タスクバーの最初の往復と同じ `POLLIN | POLLERR` で待つ。
//!
//! # 時間を数で見る
//!
//! 30 ミリ秒の時間切れ（3 と 9）は、`std::time::Instant`（`CLOCK_MONOTONIC`）で測った経過が 30 ミリ秒以上で 1 秒未満であることを
//! 見る。上限を 1 秒にした根拠: ZeikOS のティックは 10 ミリ秒で、切り上げと起こしで 2 ティックほど延びる。full check では QEMU が
//! 並んで走るので、順番を待つ分も延びる。Linux では 30 ミリ秒をわずかに越える程度である。5 秒の待ちで別のスレッドが書く形は、
//! 経過が 5 秒未満であること（時間切れの前に起きたこと）を見る。
//!
//! 行はどれも標準出力へ出す。終了ステータスは引数の数（4）。Linux でも同じ行を出す
//! （`linux-programs/reference/wl-ppoll-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

const NAME: &str = "wl-ppoll-rust";
const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/ppoll.sock";
const SYS_POLL: i64 = 7;
const SYS_PPOLL: i64 = 271;
const POLLIN: i16 = 1;
/// wayland-client が `POLLIN` に添えて渡す（Linux は `events` に関係なく返す）。
const POLLERR: i16 = 8;
const EINVAL: i32 = 22;
const EFAULT: i32 = 14;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const PAGE: usize = 4096;
/// 5 の形で、書き手のスレッドが書く前に譲る回数。
const YIELDS: usize = 50;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

extern "C" {
    fn syscall(number: i64, ...) -> i64;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn sched_yield() -> i32;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i32 {
    // SAFETY: musl のスレッドごとの errno。
    unsafe { *__errno_location() }
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// `ppoll` を直に打つ。返り値と errno。
fn ppoll(fd: i32, tsp: *mut Timespec, sigmask: *const u64, sigsetsize: usize) -> (i64, i32, i16) {
    ppoll_events(fd, POLLIN, tsp, sigmask, sigsetsize)
}

/// `events` を選んで `ppoll` を直に打つ。
fn ppoll_events(fd: i32, events: i16, tsp: *mut Timespec, sigmask: *const u64, sigsetsize: usize) -> (i64, i32, i16) {
    let mut slot = PollFd { fd, events, revents: 0 };
    // SAFETY: `slot` は 1 つの `struct pollfd`。`tsp` と `sigmask` は呼ぶ側が決める（外したページも渡す）。
    let got = unsafe { syscall(SYS_PPOLL, &mut slot as *mut PollFd, 1u64, tsp, sigmask, sigsetsize) };
    (got, if got < 0 { errno() } else { 0 }, slot.revents)
}

/// `poll` を直に打つ。返り値と errno。
fn poll(fd: i32, timeout: i32) -> (i64, i32, i16) {
    poll_events(fd, POLLIN, timeout)
}

/// `events` を選んで `poll` を直に打つ。
fn poll_events(fd: i32, events: i16, timeout: i32) -> (i64, i32, i16) {
    let mut slot = PollFd { fd, events, revents: 0 };
    // SAFETY: `slot` は 1 つの `struct pollfd`。
    let got = unsafe { syscall(SYS_POLL, &mut slot as *mut PollFd, 1u64, timeout as i64) };
    (got, if got < 0 { errno() } else { 0 }, slot.revents)
}

/// 1 ページを作って外し、そのアドレスを返す（読めない置き場）。
fn unmapped_page() -> usize {
    // SAFETY: 無名の 1 ページを作る。
    let page = unsafe { mmap(std::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
    if page as isize == -1 {
        fail("mmap failed", 89);
    }
    // SAFETY: いま作ったページを外す。
    if unsafe { munmap(page, PAGE) } != 0 {
        fail("munmap failed", 89);
    }
    page as usize
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let _ = std::fs::create_dir_all(DIR);
    let _ = std::fs::remove_file(PATH);
    let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 90));
    let mut client = UnixStream::connect(PATH).unwrap_or_else(|error| fail(&format!("connect failed: {error}"), 90));
    let (mut server, _) = listener.accept().unwrap_or_else(|error| fail(&format!("accept failed: {error}"), 90));
    let fd = server.as_raw_fd();
    let mut byte = [0u8; 1];
    let null_ts = std::ptr::null_mut::<Timespec>();
    let null_mask = std::ptr::null::<u64>();

    // 1. 読める fd を、NULL で待つ（タスクバーと同じ `POLLIN | POLLERR`）。
    client.write_all(b"a").unwrap_or_else(|error| fail(&format!("write failed: {error}"), 91));
    let (got, _, revents) = ppoll_events(fd, POLLIN | POLLERR, null_ts, null_mask, 0);
    println!(
        "{NAME}: ppoll with POLLIN|POLLERR and no timeout on a readable socket returned 1 with POLLIN = {}",
        got == 1 && revents == POLLIN
    );
    let _ = server.read(&mut byte);

    // 2. 読めない fd を、0 で待つ。
    let mut zero = Timespec { sec: 0, nsec: 0 };
    let (got, _, _) = ppoll(fd, &mut zero, null_mask, 0);
    println!("{NAME}: ppoll with a zero timeout on an empty socket returned 0 at once = {}", got == 0);

    // 3. 読めない fd を、30 ミリ秒で待つ。
    let mut short = Timespec { sec: 0, nsec: 30_000_000 };
    let started = Instant::now();
    let (got, _, _) = ppoll(fd, &mut short, null_mask, 0);
    let took = started.elapsed();
    println!(
        "{NAME}: ppoll with 30 ms on an empty socket returned 0 and wrote back 0 s 0 ns = {}",
        got == 0 && short.sec == 0 && short.nsec == 0
    );
    println!(
        "{NAME}: that ppoll took at least 30 ms and less than 1 s = {}",
        took >= Duration::from_millis(30) && took < Duration::from_secs(1)
    );

    // 4. 読める fd を、5 秒で待つ。
    client.write_all(b"b").unwrap_or_else(|error| fail(&format!("write failed: {error}"), 91));
    let mut long = Timespec { sec: 5, nsec: 0 };
    let (got, _, revents) = ppoll(fd, &mut long, null_mask, 0);
    let left_ok = (long.sec < 5 || (long.sec == 5 && long.nsec == 0)) && long.sec >= 0 && (0..1_000_000_000).contains(&long.nsec);
    println!(
        "{NAME}: ppoll with 5 s on a readable socket returned 1 and left at most 5 s = {}",
        got == 1 && revents == POLLIN && left_ok
    );
    let _ = server.read(&mut byte);

    // 5. 別のスレッドが少し後に書く。
    let mut writer = client.try_clone().unwrap_or_else(|error| fail(&format!("clone failed: {error}"), 92));
    let later = std::thread::spawn(move || {
        for _ in 0..YIELDS {
            // SAFETY: 引数を取らない。
            unsafe { sched_yield() };
        }
        writer.write_all(b"c").is_ok()
    });
    let (got, _, revents) = ppoll(fd, null_ts, null_mask, 0);
    let wrote = later.join().unwrap_or(false);
    println!(
        "{NAME}: ppoll with no timeout returned 1 after another thread wrote = {}",
        got == 1 && revents == POLLIN && wrote
    );
    let _ = server.read(&mut byte);

    // 6. 範囲の外の長さ。
    let mut too_many_nanos = Timespec { sec: 0, nsec: 1_000_000_000 };
    let (got_nanos, errno_nanos, _) = ppoll(fd, &mut too_many_nanos, null_mask, 0);
    let mut negative = Timespec { sec: -1, nsec: 0 };
    let (got_negative, errno_negative, _) = ppoll(fd, &mut negative, null_mask, 0);
    println!(
        "{NAME}: a timeout with 1e9 nanoseconds or a negative second failed with EINVAL = {}",
        got_nanos == -1 && errno_nanos == EINVAL && got_negative == -1 && errno_negative == EINVAL
    );

    // 7. 読めない `tsp`。
    let gone = unmapped_page();
    let (got_gone, errno_gone, _) = ppoll(fd, gone as *mut Timespec, null_mask, 0);
    let mask = 0u64;
    let (got_order, errno_order, _) = ppoll(fd, gone as *mut Timespec, &mask, 4);
    println!(
        "{NAME}: an unreadable timeout failed with EFAULT, before a bad sigsetsize was looked at = {}",
        got_gone == -1 && errno_gone == EFAULT && got_order == -1 && errno_order == EFAULT
    );

    // 8. シグナルのマスク。
    client.write_all(b"d").unwrap_or_else(|error| fail(&format!("write failed: {error}"), 91));
    let (got_mask, _, revents_mask) = ppoll(fd, null_ts, &mask, 8);
    let (got_size, errno_size, _) = ppoll(fd, null_ts, &mask, 4);
    let (got_null, _, _) = ppoll(fd, null_ts, null_mask, 4);
    let (got_bad_mask, errno_bad_mask, _) = ppoll(fd, null_ts, gone as *const u64, 8);
    println!(
        "{NAME}: a sigmask with sigsetsize 8 was taken, 4 failed with EINVAL, and a null sigmask ignored the size = {}",
        got_mask == 1 && revents_mask == POLLIN && got_size == -1 && errno_size == EINVAL && got_null == 1
    );
    println!(
        "{NAME}: an unreadable sigmask failed with EFAULT = {}",
        got_bad_mask == -1 && errno_bad_mask == EFAULT
    );
    let _ = server.read(&mut byte);

    // 9. `poll` の待つ長さ。
    client.write_all(b"e").unwrap_or_else(|error| fail(&format!("write failed: {error}"), 91));
    let (got, _, revents) = poll(fd, -1);
    println!("{NAME}: poll with -1 on a readable socket returned 1 with POLLIN = {}", got == 1 && revents == POLLIN);
    let _ = server.read(&mut byte);
    let (got, _, _) = poll(fd, 0);
    println!("{NAME}: poll with 0 on an empty socket returned 0 at once = {}", got == 0);
    let started = Instant::now();
    let (got, _, _) = poll(fd, 30);
    let took = started.elapsed();
    println!(
        "{NAME}: poll with 30 ms on an empty socket returned 0 after at least 30 ms and less than 1 s = {}",
        got == 0 && took >= Duration::from_millis(30) && took < Duration::from_secs(1)
    );
    for (timeout, what) in [(5_000, "5000 ms"), (-5, "-5 (a negative timeout waits on)")] {
        let mut writer = client.try_clone().unwrap_or_else(|error| fail(&format!("clone failed: {error}"), 92));
        let later = std::thread::spawn(move || {
            for _ in 0..YIELDS {
                // SAFETY: 引数を取らない。
                unsafe { sched_yield() };
            }
            writer.write_all(b"f").is_ok()
        });
        let started = Instant::now();
        // タスクバーの最初の往復と同じ `POLLIN | POLLERR`。
        let (got, _, revents) = poll_events(fd, POLLIN | POLLERR, timeout);
        let took = started.elapsed();
        let wrote = later.join().unwrap_or(false);
        println!(
            "{NAME}: poll with POLLIN|POLLERR and {what} returned 1 before 5 s after another thread wrote = {}",
            got == 1 && revents == POLLIN && wrote && took < Duration::from_secs(5)
        );
        let _ = server.read(&mut byte);
    }

    drop(server);
    drop(client);
    drop(listener);
    let _ = std::fs::remove_file(PATH);
    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
