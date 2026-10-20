//! `wl-realtime-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**壁の時計
//! （`CLOCK_REALTIME`）を読み、`std::time::SystemTime::now` が動くことと、時計の進み方を見る**（2026-10-20。`ADR-0090`）。
//!
//! # 何をするか（引数が `/etc/motd a b` のとき）
//!
//! 1. `clock_gettime(CLOCK_REALTIME)` と `CLOCK_MONOTONIC` を読む。**`CLOCK_REALTIME` が失敗したら、名指しの行を出して
//!    90 で終わる**（ZeikOS で、起動時に RTC が読めなかったとき）。
//! 2. 読んだ `CLOCK_REALTIME` の値を、**標準エラーへ** 1 行出す（`wl-realtime-rust: realtime <秒>.<ナノ秒>`）。値は起動の
//!    たびに変わるので、Linux の参照（標準出力だけを控える）とは突き合わせない。xtask が、起動の前後のホストの時刻と比べる。
//! 3. `SystemTime::now` が動くこと（252 の試しで、Seinas のコンポジタはここで panic した）と、`CLOCK_REALTIME` との差が
//!    1 秒未満であること。
//! 4. 1 秒眠り、もう 1 度読む。**`CLOCK_REALTIME` が 1 秒以上進んだこと**と、**`CLOCK_REALTIME − CLOCK_MONOTONIC` が
//!    20 ms より大きく動かなかったこと**（同じ起点から同じ速さで進む）。
//! 5. `clock_settime(CLOCK_REALTIME)` が `EPERM` で断られること。
//! 6. `CLOCK_REALTIME` の絶対の締切（`clock_nanosleep` の `TIMER_ABSTIME` と `timerfd_create`）を 1 回ずつ頼む。**答えは
//!    Linux（受ける）と ZeikOS（`EINVAL` で、カーネルが名前のある行を出す）で違う**ので、標準エラーへ出す。
//!
//! 終了ステータスは引数の数（4）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `realtime-test` のビルドだけである。**

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const TIMER_ABSTIME: i32 = 1;
const EPERM: i32 = 1;

extern "C" {
    fn clock_gettime(clock: i32, out: *mut Timespec) -> i32;
    fn clock_settime(clock: i32, value: *const Timespec) -> i32;
    fn clock_nanosleep(clock: i32, flags: i32, request: *const Timespec, remain: *mut Timespec) -> i32;
    fn timerfd_create(clock: i32, flags: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i32 {
    // SAFETY: musl の `__errno_location` は、呼んだスレッドの errno の置き場を指す。読むだけである。
    unsafe { *__errno_location() }
}

/// 時計を読む。**失敗したら errno を返す。**
fn read(clock: i32) -> Result<Timespec, i32> {
    let mut value = Timespec { sec: 0, nsec: 0 };
    // SAFETY: `value` は書ける 16 バイトの `struct timespec` である。
    if unsafe { clock_gettime(clock, &mut value) } == 0 {
        Ok(value)
    } else {
        Err(errno())
    }
}

fn nanos(value: Timespec) -> i128 {
    i128::from(value.sec) * 1_000_000_000 + i128::from(value.nsec)
}

fn main() {
    let argument_count = std::env::args().count();
    println!("wl-realtime-rust: main starts with {argument_count} argument(s)");

    let first_real = match read(CLOCK_REALTIME) {
        Ok(value) => value,
        Err(error) => {
            println!("wl-realtime-rust: CLOCK_REALTIME failed with errno {error}; exiting with 90");
            std::process::exit(90);
        }
    };
    let first_mono = read(CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC is always there");
    eprintln!("wl-realtime-rust: realtime {}.{:09}", first_real.sec, first_real.nsec);

    // `SystemTime::now` は `clock_gettime(CLOCK_REALTIME)` を打つ。失敗すれば `std` が panic する。
    let system = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the wall clock is after 1970");
    let gap = (i128::try_from(system.as_nanos()).unwrap() - nanos(first_real)).abs();
    println!(
        "wl-realtime-rust: SystemTime::now agrees with CLOCK_REALTIME within 1 s = {}",
        gap < 1_000_000_000
    );

    std::thread::sleep(Duration::from_secs(1));
    let second_real = read(CLOCK_REALTIME).expect("CLOCK_REALTIME was there a second ago");
    let second_mono = read(CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC is always there");
    println!(
        "wl-realtime-rust: CLOCK_REALTIME advanced by at least 1 s over a 1 s sleep = {}",
        nanos(second_real) - nanos(first_real) >= 1_000_000_000
    );
    let drift = (nanos(second_real) - nanos(second_mono)) - (nanos(first_real) - nanos(first_mono));
    println!(
        "wl-realtime-rust: CLOCK_REALTIME minus CLOCK_MONOTONIC moved by less than 20 ms = {}",
        drift.abs() < 20_000_000
    );

    // SAFETY: 渡す `timespec` は読める 16 バイトである。
    let set = unsafe { clock_settime(CLOCK_REALTIME, &second_real) };
    println!(
        "wl-realtime-rust: clock_settime(CLOCK_REALTIME) is refused with EPERM = {}",
        set == -1 && errno() == EPERM
    );

    // 絶対の締切: もう過ぎた時刻（1 秒前）なので、受ける Linux でも眠らずに戻る。
    let past = Timespec {
        sec: second_real.sec - 1,
        nsec: second_real.nsec,
    };
    // SAFETY: `past` は読める 16 バイトで、残りは書かない（NULL）。
    let sleep = unsafe { clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &past, std::ptr::null_mut()) };
    eprintln!("wl-realtime-rust: clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME) returned {sleep}");
    // SAFETY: 引数は整数だけである。
    let timer = unsafe { timerfd_create(CLOCK_REALTIME, 0) };
    if timer >= 0 {
        eprintln!("wl-realtime-rust: timerfd_create(CLOCK_REALTIME) returned a descriptor");
        // SAFETY: 今作った fd を閉じる。
        unsafe { close(timer) };
    } else {
        eprintln!("wl-realtime-rust: timerfd_create(CLOCK_REALTIME) failed with errno {}", errno());
    }

    std::process::exit(argument_count as i32);
}
