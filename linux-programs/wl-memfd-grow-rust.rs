//! `wl-memfd-grow-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**memfd を伸ばす
//! 2 回目の `ftruncate`**（2026-10-26。`ADR-0065` の Addendum）。
//!
//! # 何をするか
//!
//! 1. `memfd_create(MFD_CLOEXEC | MFD_ALLOW_SEALING)` → `ftruncate(256000)` → `mmap`（MAP_SHARED、読み書き）→ 全体に模様を書く。
//!    Seinas のタスクバーの pool の初めの大きさ（1280 × 50 × 4）である。
//! 2. 同じ大きさへの `ftruncate` は 0。
//! 3. `ftruncate(512000)`（2 枚目のバッファで pool が伸びる形）は 0。伸ばした後の大きさ全体を新しく `mmap` し、前の 256,000 バイトに
//!    模様が残っていること、伸ばした分が 0 であることを見る。前のマッピングからも、模様がそのまま読めること（既にある
//!    マッピングは触らない）を見る。
//! 4. `F_SEAL_SHRINK` だけを足した memfd は伸ばせる。`F_SEAL_GROW` を足すと、伸ばす `ftruncate` は `EPERM`。
//! 5. 16 MiB への `ftruncate` は 0（ZeikOS の 1 つの上限ちょうど）。
//!
//! 古いマッピングの `munmap` は打たない（ZeikOS は、マッピングの数を持つまで共有のマッピングの `munmap` を断る。
//! `ADR-0065` の Addendum）。1 つの上限を越える形（`EFBIG`）と全体の上限（`ENOMEM`）は Linux では通るので、ここでは見ない
//! （`kernel/src/shm.rs` のホストのテストが見る）。
//!
//! 行はどれも標準出力へ出す。終了ステータスは引数の数（4）。Linux でも同じ行を出す
//! （`linux-programs/reference/wl-memfd-grow-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

const NAME: &str = "wl-memfd-grow-rust";
const MFD_CLOEXEC: u32 = 1;
const MFD_ALLOW_SEALING: u32 = 2;
const F_ADD_SEALS: i32 = 1033;
const F_SEAL_SHRINK: i32 = 0x0002;
const F_SEAL_GROW: i32 = 0x0004;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 0x01;
const EPERM: i32 = 1;
const FIRST: usize = 256_000;
const SECOND: usize = 512_000;
const LARGEST: i64 = 16 * 1024 * 1024;

extern "C" {
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn ftruncate(fd: i32, length: i64) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
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

fn new_memfd() -> i32 {
    // SAFETY: NUL で終わる名前。
    let fd = unsafe { memfd_create(b"grow\0".as_ptr(), MFD_CLOEXEC | MFD_ALLOW_SEALING) };
    if fd < 0 {
        fail("memfd_create failed", 89);
    }
    fd
}

fn map(fd: i32, len: usize) -> *mut u8 {
    // SAFETY: memfd の全体を共有で読み書きにマップする。
    let at = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
    if at as isize == -1 {
        fail("mmap failed", 90);
    }
    at
}

/// マッピングの `offset` バイト目を読む。呼ぶ側は、`offset` がマッピングの内側であることを守る。
fn read_at(at: *mut u8, offset: usize) -> u8 {
    // SAFETY: 呼ぶ側が、`at` から `offset` までが生きているマッピングの内側であることを守る（`new` は SECOND バイト、`old` は
    // FIRST バイトで、どちらもその内側だけを読む）。
    unsafe { at.add(offset).read() }
}

fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8 + 1
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");

    // 1. 初めの大きさ。
    let fd = new_memfd();
    // SAFETY: memfd の fd。
    let first = unsafe { ftruncate(fd, FIRST as i64) };
    let old = map(fd, FIRST);
    for offset in 0..FIRST {
        // SAFETY: `old` は FIRST バイトの書けるマッピング。
        unsafe { old.add(offset).write(pattern(offset)) };
    }
    println!("{NAME}: ftruncate to 256000 bytes returned {first}");

    // 2. 同じ大きさ。
    // SAFETY: memfd の fd。
    let same = unsafe { ftruncate(fd, FIRST as i64) };
    println!("{NAME}: ftruncate to the same size returned {same}");

    // 3. 伸ばす。
    // SAFETY: memfd の fd。
    let grown = unsafe { ftruncate(fd, SECOND as i64) };
    let new = map(fd, SECOND);
    let kept = (0..FIRST).all(|offset| read_at(new, offset) == pattern(offset));
    let zero = (FIRST..SECOND).all(|offset| read_at(new, offset) == 0);
    let old_kept = (0..FIRST).all(|offset| read_at(old, offset) == pattern(offset));
    println!("{NAME}: ftruncate to 512000 bytes returned {grown}");
    println!(
        "{NAME}: the new mapping kept the first 256000 bytes = {kept}, read zeros past them = {zero}, and the old mapping still reads the same bytes = {old_kept}"
    );
    // 伸ばした分に書けて、前のマッピングとは別の所であることも見る。
    // SAFETY: SECOND - 1 は `new` の内側。
    unsafe { new.add(SECOND - 1).write(0x5a) };
    // SAFETY: 同上。
    println!("{NAME}: the last byte of the grown part can be written = {}", unsafe { new.add(SECOND - 1).read() } == 0x5a);

    // 4. 封。
    let sealed = new_memfd();
    // SAFETY: memfd の fd。
    let base = unsafe { ftruncate(sealed, 4096) };
    // SAFETY: 封を足す。
    let shrink_seal = unsafe { fcntl(sealed, F_ADD_SEALS, F_SEAL_SHRINK) };
    // SAFETY: memfd の fd。
    let grow_with_shrink_seal = unsafe { ftruncate(sealed, 8192) };
    // SAFETY: 封を足す。
    let grow_seal = unsafe { fcntl(sealed, F_ADD_SEALS, F_SEAL_GROW) };
    // SAFETY: memfd の fd。
    let refused = unsafe { ftruncate(sealed, 12288) };
    let refused_errno = if refused < 0 { errno() } else { 0 };
    println!(
        "{NAME}: with F_SEAL_SHRINK only, growing returned {grow_with_shrink_seal} (ftruncate {base}, seal {shrink_seal})"
    );
    println!(
        "{NAME}: with F_SEAL_GROW, growing failed with EPERM = {} (seal {grow_seal})",
        refused == -1 && refused_errno == EPERM
    );

    // 5. 1 つの上限ちょうど。
    let largest = new_memfd();
    // SAFETY: memfd の fd。
    let largest_answer = unsafe { ftruncate(largest, LARGEST) };
    println!("{NAME}: ftruncate to 16 MiB returned {largest_answer}");

    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
