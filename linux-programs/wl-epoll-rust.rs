//! `wl-epoll-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**epoll・eventfd・
//! timerfd を、Seinas のイベントループ（calloop と polling）と同じ形で通す**（2026-10-19。`ADR-0088`）。
//!
//! # 何をするか（引数が `/etc/motd a b` のとき）
//!
//! 1. eventfd: 待たない読みは `EAGAIN`。3 と 4 を書くと 7 が読める。`EFD_SEMAPHORE` は 1 ずつ読める。
//! 2. epoll のレベルトリガー: スレッドが 300 ms 後に書く eventfd を待つと（期限 2 秒）、書かれて起きる。**期限の近くまで
//!    起きなかったら、名指しの行を出して 87 で終わる。** 読まずにもう一度待つと、また返る。**返らなければ 86 で終わる。**
//! 3. ONESHOT: 一度返した項目は、MOD で付け直すまで返らない。**付け直す前に返ったら、名指しの行を出して 91 で終わる。**
//! 4. timerfd: 止まった timerfd の待たない読みは `EAGAIN`。相対の 300 ms で据え、epoll で待つと満了で起きて、読むと 1 回。
//!    止めた後の `timerfd_gettime` は 0。`clock_gettime(CLOCK_MONOTONIC)` の今から 200 ms 先の絶対時刻
//!    （`TFD_TIMER_ABSTIME`）で据えても、満了で起きる。**起きた時刻が求めた時刻より前なら、名指しの行を出して 92 で終わる。**
//!
//!    レベルトリガーと ONESHOT を timerfd より先に置くのは、返した項目を止めてしまう破壊（`epoll-level-reported-once`）で、
//!    timerfd の待ちが永久に返らなくなる前に、名指しで終わるためである。
//! 5. listener: スレッドが 300 ms 後に繋ぐと、epoll が listener を返す。
//! 6. 入れ子: 内側の epoll を `F_DUPFD_CLOEXEC` で複製して外側へ入れる（wayland-backend の形）。スレッドが 300 ms 後に内側の
//!    eventfd へ書くと、外側が起きる。**外側が待ちの期限まで起きなかったら、名指しの行を出して 90 で終わる。**
//! 7. 閉じた fd は外れる: eventfd を登録して閉じ、同じ番号で別の eventfd を作って書いても、epoll はそれを返さない。
//! 8. 断る形: 自分を足すのは `EINVAL`、輪は `ELOOP`、6 個目を作る鎖は `ELOOP`、同じ組は `EEXIST`、普通のファイルは
//!    `EPERM`。timeout 0 と、正の timeout（50 ms）は 0 件で返る。
//!
//! 終了ステータスは引数の数（4）。Linux でも同じ行を出す（`linux-programs/reference/wl-epoll-rust.txt`）。`EPOLLET` は
//! ZeikOS だけが断る（Linux は受ける）ので、ここでは打たない。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

const DIR: &str = "/tmp/seinas";
const PATH: &str = "/tmp/seinas/ep.sock";
const NAME: &str = "wl-epoll-rust";

const EPOLLIN: u32 = 0x001;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_MOD: i32 = 3;
const EPOLL_CLOEXEC: i32 = 0o2000000;
const EFD_SEMAPHORE: i32 = 1;
const EFD_NONBLOCK: i32 = 0o4000;
const TFD_NONBLOCK: i32 = 0o4000;
const TFD_TIMER_ABSTIME: i32 = 1;
const CLOCK_MONOTONIC: i32 = 1;
const F_DUPFD_CLOEXEC: i32 = 1030;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Itimerspec {
    interval: Timespec,
    value: Timespec,
}

extern "C" {
    fn epoll_create1(flags: i32) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut u8) -> i32;
    fn epoll_pwait(epfd: i32, events: *mut u8, maxevents: i32, timeout: i32, sigmask: *const u8) -> i32;
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn timerfd_create(clockid: i32, flags: i32) -> i32;
    fn timerfd_settime(fd: i32, flags: i32, new: *const Itimerspec, old: *mut Itimerspec) -> i32;
    fn timerfd_gettime(fd: i32, curr: *mut Itimerspec) -> i32;
    fn clock_gettime(clockid: i32, ts: *mut Timespec) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// `epoll_ctl` を打ち、0 か errno を返す。
fn ctl(epfd: i32, op: i32, fd: i32, events: u32, data: u64) -> i32 {
    let mut event = [0u8; 12];
    event[..4].copy_from_slice(&events.to_le_bytes());
    event[4..].copy_from_slice(&data.to_le_bytes());
    // SAFETY: `event` はこの関数の中で生きている 12 バイトである。
    if unsafe { epoll_ctl(epfd, op, fd, event.as_mut_ptr()) } < 0 {
        errno()
    } else {
        0
    }
}

/// `epoll_pwait` で待ち、返った `data` を並べる。
fn wait(epfd: i32, timeout: i32) -> Vec<u64> {
    let mut events = [0u8; 12 * 8];
    // SAFETY: `events` はこの関数の中で生きていて、8 件分の大きさがある。
    let count = unsafe { epoll_pwait(epfd, events.as_mut_ptr(), 8, timeout, std::ptr::null()) };
    if count < 0 {
        fail(&format!("epoll_pwait failed with errno {}", errno()), 89);
    }
    (0..count as usize)
        .map(|index| {
            let mut data = [0u8; 8];
            data.copy_from_slice(&events[index * 12 + 4..index * 12 + 12]);
            u64::from_le_bytes(data)
        })
        .collect()
}

fn read_u64(fd: i32) -> Result<u64, i32> {
    let mut value = [0u8; 8];
    // SAFETY: `value` は 8 バイトである。
    if unsafe { read(fd, value.as_mut_ptr(), 8) } == 8 {
        Ok(u64::from_le_bytes(value))
    } else {
        Err(errno())
    }
}

fn write_u64(fd: i32, value: u64) -> bool {
    // SAFETY: 値は 8 バイトである。
    unsafe { write(fd, value.to_le_bytes().as_ptr(), 8) == 8 }
}

fn monotonic() -> Timespec {
    let mut now = Timespec::default();
    // SAFETY: `now` はこの関数の中で生きている。
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut now) };
    now
}

fn nanos(ts: Timespec) -> i64 {
    ts.sec * 1_000_000_000 + ts.nsec
}

fn arm(timer: i32, flags: i32, value: Timespec) -> i32 {
    let setting = Itimerspec {
        interval: Timespec::default(),
        value,
    };
    // SAFETY: `setting` はこの関数の中で生きている。前の値は受けない。
    unsafe { timerfd_settime(timer, flags, &setting, std::ptr::null_mut()) }
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    // SAFETY: 以下の FFI はどれも、この関数の中で生きている値だけを渡す。
    unsafe {
        // 1. eventfd。
        let efd = eventfd(0, EFD_NONBLOCK);
        println!("{NAME}: an empty eventfd that does not wait reads EAGAIN = {}", read_u64(efd) == Err(11));
        write_u64(efd, 3);
        write_u64(efd, 4);
        println!("{NAME}: writing 3 and 4 and reading the eventfd gives {:?}", read_u64(efd));
        let semaphore = eventfd(2, EFD_NONBLOCK | EFD_SEMAPHORE);
        let first = read_u64(semaphore);
        let second = read_u64(semaphore);
        let third = read_u64(semaphore);
        println!("{NAME}: a semaphore eventfd started at 2 reads {first:?} {second:?} {third:?}");
        close(semaphore);

        // 2. レベルトリガーの eventfd。
        let ep = epoll_create1(0);
        let efd = eventfd(0, EFD_NONBLOCK);
        ctl(ep, EPOLL_CTL_ADD, efd, EPOLLIN, 7);
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                write_u64(efd, 1);
            });
            let started = monotonic();
            let woke = wait(ep, 2000);
            let waited_ms = (nanos(monotonic()) - nanos(started)) / 1_000_000;
            if woke.is_empty() || waited_ms >= 1500 {
                fail("epoll woke only at its time limit, not when another thread wrote the eventfd", 87);
            }
            println!("{NAME}: epoll woke when another thread wrote the eventfd = {woke:?}");
        });
        let again = wait(ep, 0);
        if again.is_empty() {
            fail("a level-triggered entry did not come back while it was still ready", 86);
        }
        println!("{NAME}: without reading, the level-triggered entry comes back = {again:?}");
        read_u64(efd).ok();
        println!("{NAME}: after reading, nothing is ready = {:?}", wait(ep, 0));

        // 3. ONESHOT。
        let one = eventfd(1, EFD_NONBLOCK);
        ctl(ep, EPOLL_CTL_ADD, one, EPOLLIN | EPOLLONESHOT, 8);
        println!("{NAME}: a oneshot entry comes back once = {:?}", wait(ep, 0));
        let again = wait(ep, 0);
        if !again.is_empty() {
            fail("a oneshot entry came back before it was modified", 91);
        }
        println!("{NAME}: and not again before it is modified = {again:?}");
        ctl(ep, EPOLL_CTL_MOD, one, EPOLLIN | EPOLLONESHOT, 9);
        println!("{NAME}: after EPOLL_CTL_MOD it comes back = {:?}", wait(ep, 0));
        close(one);
        close(efd);
        close(ep);

        // 4. timerfd。
        let ep = epoll_create1(EPOLL_CLOEXEC);
        let timer = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
        println!("{NAME}: a stopped timerfd that does not wait reads EAGAIN = {}", read_u64(timer) == Err(11));
        ctl(ep, EPOLL_CTL_ADD, timer, EPOLLIN, 1);
        arm(timer, 0, Timespec { sec: 0, nsec: 300_000_000 });
        let woke = wait(ep, -1);
        println!("{NAME}: epoll woke for the relative timer = {woke:?}; reading it gives {:?}", read_u64(timer));
        arm(timer, 0, Timespec::default());
        let mut current = Itimerspec::default();
        timerfd_gettime(timer, &mut current);
        println!(
            "{NAME}: after stopping it, timerfd_gettime gives 0 = {}",
            current.value.sec == 0 && current.value.nsec == 0
        );
        let start = monotonic();
        let target = Timespec {
            sec: (nanos(start) + 200_000_000) / 1_000_000_000,
            nsec: (nanos(start) + 200_000_000) % 1_000_000_000,
        };
        arm(timer, TFD_TIMER_ABSTIME, target);
        let woke = wait(ep, -1);
        let woke_at = monotonic();
        if nanos(woke_at) < nanos(target) {
            fail("the absolute timer woke epoll before the time it was set for", 92);
        }
        println!("{NAME}: epoll woke for the absolute timer = {woke:?}, not before the set time; reading gives {:?}", read_u64(timer));
        close(timer);
        close(ep);

        // 5. listener。
        let _ = fs::create_dir_all(DIR);
        let _ = fs::remove_file(PATH);
        let listener = UnixListener::bind(PATH).unwrap_or_else(|error| fail(&format!("bind failed: {error}"), 88));
        listener.set_nonblocking(true).ok();
        let ep = epoll_create1(0);
        ctl(ep, EPOLL_CTL_ADD, listener.as_raw_fd(), EPOLLIN, 5);
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                let mut client = UnixStream::connect(PATH).ok();
                if let Some(client) = client.as_mut() {
                    let _ = client.write_all(b"x");
                }
            });
            let woke = wait(ep, -1);
            let accepted = listener.accept().is_ok();
            println!("{NAME}: epoll woke for the listener when a client connected later = {woke:?}, accepted = {accepted}");
        });
        let would_block = matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock);
        println!("{NAME}: and the listener has no more connections = {would_block}");
        close(ep);
        drop(listener);
        let _ = fs::remove_file(PATH);

        // 6. 入れ子（複製した内側の epoll を外側へ入れる）。
        let inner = epoll_create1(EPOLL_CLOEXEC);
        let efd = eventfd(0, EFD_NONBLOCK);
        ctl(inner, EPOLL_CTL_ADD, efd, EPOLLIN, 11);
        let copy = fcntl(inner, F_DUPFD_CLOEXEC, 0);
        let outer = epoll_create1(EPOLL_CLOEXEC);
        ctl(outer, EPOLL_CTL_ADD, copy, EPOLLIN, 12);
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                write_u64(efd, 1);
            });
            let woke = wait(outer, 3000);
            if woke.is_empty() {
                fail("the outer epoll did not wake when the inner one became ready", 90);
            }
            println!("{NAME}: the outer epoll woke for the nested copy of the inner one = {woke:?}");
        });
        println!("{NAME}: and the inner one returns the eventfd = {:?}", wait(inner, 0));
        read_u64(efd).ok();

        // 7. 閉じた fd は外れる。
        let gone = eventfd(0, EFD_NONBLOCK);
        ctl(outer, EPOLL_CTL_ADD, gone, EPOLLIN, 13);
        close(gone);
        let reused = eventfd(0, EFD_NONBLOCK);
        write_u64(reused, 1);
        println!(
            "{NAME}: a closed fd leaves the epoll; a new eventfd with the same number ({}) is not returned = {:?}",
            reused == gone,
            wait(outer, 0)
        );
        close(reused);

        // 8. 断る形と timeout。
        println!("{NAME}: adding an epoll to itself gives errno {}", ctl(outer, EPOLL_CTL_ADD, outer, EPOLLIN, 0));
        println!("{NAME}: adding the outer epoll into the inner one gives errno {}", ctl(inner, EPOLL_CTL_ADD, outer, EPOLLIN, 0));
        let mut chain = Vec::new();
        for _ in 0..6 {
            chain.push(epoll_create1(0));
        }
        let results: Vec<i32> = (0..5).map(|i| ctl(chain[i], EPOLL_CTL_ADD, chain[i + 1], EPOLLIN, 0)).collect();
        println!("{NAME}: building a chain of six epolls gives errnos {results:?}");
        let twice = eventfd(0, EFD_NONBLOCK);
        let first = ctl(outer, EPOLL_CTL_ADD, twice, EPOLLIN, 0);
        let second = ctl(outer, EPOLL_CTL_ADD, twice, EPOLLIN, 0);
        println!("{NAME}: adding the same fd twice gives errnos {first} and {second}");
        let path = std::env::args().nth(1).unwrap_or_default();
        let file = File::open(&path).unwrap_or_else(|_| fail("opening the file given as the first argument failed", 88));
        println!("{NAME}: adding a regular file gives errno {}", ctl(outer, EPOLL_CTL_ADD, file.as_raw_fd(), EPOLLIN, 0));
        println!("{NAME}: a wait with timeout 0 returns {:?}", wait(outer, 0));
        println!("{NAME}: a wait with timeout 50 ms returns {:?}", wait(outer, 50));
        for fd in chain {
            close(fd);
        }
        close(twice);
        close(outer);
        close(copy);
        close(inner);
        close(efd);
    }
    std::process::exit(argument_count as i32);
}
