//! `m3a-memfd-shrink-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**マップ中の
//! memfd を縮める `ftruncate` を ZeikOS が断ること（`-EINVAL`）を見る**（`ADR-0065` の 2026-10-15 の Addendum）。
//!
//! # 何をするか
//!
//! 1. `memfd_create` → `ftruncate` で 2 ページ → `mmap`（MAP_SHARED、読み書き）→ 2 ページ目の先頭に 0x5a を書く。
//! 2. マップしたまま、1 ページへ縮める `ftruncate` を打ち、戻り値を出す。ZeikOS は 2 回目の `ftruncate` を向きにかかわらず
//!    `-EINVAL` で断る（マップ中の memfd は縮めない）。Linux は 0 を返して縮め、縮めた先に触ると SIGBUS になる。
//!    **この差のため、このイメージは Linux の参照と突き合わせない**（`memfd-shrink-test` のビルドだけが起こす）。
//! 3. 2 ページ目を読み戻して出す（断られていれば 0x5a のまま）。終了ステータスは引数の数。**縮める `ftruncate` が 0 を返したら
//!    （ZeikOS では起きてはならない形。破壊テスト `shm-ftruncate-shrinks-mapped-test` が作る）、97 で終わる**——カーネルの
//!    起動時の確かめは引数の数を期待しているので、名指しして止まる。Linux で走らせれば 97 で終わる（上の理由で、Linux の
//!    参照とは突き合わせない）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

const SYS_MMAP: u64 = 9;
const SYS_FTRUNCATE: u64 = 77;
const SYS_MEMFD_CREATE: u64 = 319;
const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 1;

/// `syscall` 命令を直に打つ（6 引数まで）。
fn raw_syscall(number: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let ret: i64;
    // SAFETY: Linux の x86_64 の呼び出し規約どおりにレジスタを渡し、壊れる RCX と R11 を申告する。
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            in("r8") e,
            in("r9") f,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-memfd-shrink-rust: main starts with {argument_count} argument(s)");
    let name = b"shrink\0";
    let fd = raw_syscall(SYS_MEMFD_CREATE, name.as_ptr() as u64, 0, 0, 0, 0, 0);
    let grown = raw_syscall(SYS_FTRUNCATE, fd as u64, 2 * PAGE, 0, 0, 0, 0);
    let mapped = raw_syscall(
        SYS_MMAP,
        0,
        2 * PAGE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd as u64,
        0,
    );
    println!(
        "m3a-memfd-shrink-rust: memfd_create returned a fd = {}, ftruncate to 2 pages returned {grown}, mmap of 2 pages succeeded = {}",
        fd >= 0,
        mapped > 0
    );
    if fd < 0 || grown != 0 || mapped <= 0 {
        std::process::exit(98);
    }
    let second_page = (mapped as u64 + PAGE) as *mut u8;
    // SAFETY: いま MAP_SHARED で 2 ページをマップした範囲の中である。
    unsafe { second_page.write_volatile(0x5a) };
    let shrunk = raw_syscall(SYS_FTRUNCATE, fd as u64, PAGE, 0, 0, 0, 0);
    println!("m3a-memfd-shrink-rust: ftruncate to 1 page while mapped returned {shrunk}");
    // SAFETY: 断られていればマップはそのままである。縮められていれば、ここは Linux では SIGBUS になる場所である。
    let read_back = unsafe { second_page.read_volatile() };
    println!(
        "m3a-memfd-shrink-rust: the second page still reads back 0x5a = {}",
        read_back == 0x5a
    );
    if shrunk == 0 {
        // ZeikOS が縮めてしまった（破壊テストの形）。引数の数ではない状態で終わり、起動時の確かめに名指しさせる。
        std::process::exit(97);
    }
    std::process::exit(argument_count as i32);
}
