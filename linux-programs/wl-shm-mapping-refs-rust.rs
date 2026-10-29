//! `wl-shm-mapping-refs-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**共有メモリ
//! （memfd）のマッピングが、fd とは別に実体を持ち続けること、共有のマッピングの `munmap`、マップしていない memfd を縮める
//! `ftruncate`**（2026-10-30。`ADR-0065` の Addendum）。
//!
//! # 何をするか
//!
//! 1. memfd（4 ページ）を `mmap` して模様を書き、fd を閉じる。マッピングから模様が読めること、続けて作った別の memfd に
//!    書いても、前のマッピングの中身が変わらないことを見る。
//! 2. そのマッピングの `munmap` が 0 を返す。
//! 3. 3 ページのマッピングの真ん中の 1 ページだけを `munmap` する。前と後ろのページは読める。残りを外す。
//! 4. 1 つの memfd で「`mmap`→`munmap`」を 70 回くり返す（ZeikOS の 1 プロセスのマッピングの数 64 より多い）。
//! 5. 「`memfd_create`→16 MiB の `ftruncate`→`mmap`→最初と最後のバイトに書く→`close`→`munmap`」を、間を置かずに 20 回
//!    くり返す（ZeikOS の共有メモリの欄 16 と、全体の上限 64 MiB より多い）。失敗したら名指しの行を出して 95 で終わる。
//! 6. マップして外した後の memfd は、`ftruncate` で縮められる。縮めた後の `fstat` の大きさと、残ったページの中身を見る。
//!    `F_SEAL_SHRINK` を足すと、縮める `ftruncate` は `EPERM`。
//!
//! マップしたままの memfd を縮める形は出さない（Linux は通し、ZeikOS は `-EINVAL` で断る。`ADR-0065` の 2026-10-15 の
//! Addendum）。
//!
//! 行はどれも標準出力へ出す。終了ステータスは引数の数（4）。Linux でも同じ行を出す
//! （`linux-programs/reference/wl-shm-mapping-refs-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` のビルドだけである。**

const NAME: &str = "wl-shm-mapping-refs-rust";
const PAGE: usize = 4096;
const MFD_CLOEXEC: u32 = 1;
const MFD_ALLOW_SEALING: u32 = 2;
const F_ADD_SEALS: i32 = 1033;
const F_SEAL_SHRINK: i32 = 0x0002;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 0x01;
const EPERM: i32 = 1;
/// 4 でくり返す回数（ZeikOS の 1 プロセスのマッピングの数 64 より多い）。
const MAP_ROUNDS: usize = 70;
/// 5 でくり返す回数（ZeikOS の共有メモリの欄 16 より多い）。
const POOL_ROUNDS: usize = 20;
/// 5 の 1 回の大きさ（16 MiB。4 回で ZeikOS の全体の上限 64 MiB に届く）。
const POOL_BYTES: usize = 16 * 1024 * 1024;

/// `struct stat` の `st_size` の位置（x86_64）。
const STAT_SIZE_OFFSET: usize = 48;

extern "C" {
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn ftruncate(fd: i32, length: i64) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn fstat(fd: i32, buf: *mut u8) -> i32;
    fn close(fd: i32) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
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

fn new_memfd(flags: u32) -> i32 {
    // SAFETY: NUL で終わる名前。
    unsafe { memfd_create(b"mapping-refs\0".as_ptr(), MFD_CLOEXEC | flags) }
}

fn sized_memfd(len: usize, flags: u32) -> i32 {
    let fd = new_memfd(flags);
    if fd < 0 {
        fail(&format!("memfd_create failed with errno {}", errno()), 89);
    }
    // SAFETY: このプログラムが作った fd。
    if unsafe { ftruncate(fd, len as i64) } != 0 {
        fail(&format!("ftruncate failed with errno {}", errno()), 89);
    }
    fd
}

/// 共有で読み書きにマップする。失敗なら null。
fn map(fd: i32, len: usize) -> *mut u8 {
    // SAFETY: memfd の先頭から `len` バイトを共有で読み書きにマップする。
    let at = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
    if at as isize == -1 {
        std::ptr::null_mut()
    } else {
        at
    }
}

fn unmap(at: *mut u8, len: usize) -> i32 {
    // SAFETY: このプログラムがマップした範囲（かその一部）を外す。外した後は触らない。
    unsafe { munmap(at, len) }
}

fn close_fd(fd: i32) {
    // SAFETY: このプログラムが開いた fd を閉じるだけである。
    unsafe { close(fd) };
}

/// マッピングの `offset` バイト目を読む。呼ぶ側は、`offset` がマップしている範囲の内側であることを守る。
fn read_at(at: *mut u8, offset: usize) -> u8 {
    // SAFETY: 呼ぶ側が、`at` から `offset` までが生きているマッピングの内側であることを守る。
    unsafe { at.add(offset).read_volatile() }
}

/// マッピングの `offset` バイト目に書く。呼ぶ側は、`offset` がマップしている範囲の内側であることを守る。
fn write_at(at: *mut u8, offset: usize, value: u8) {
    // SAFETY: 呼ぶ側が、`at` から `offset` までが生きている、書けるマッピングの内側であることを守る。
    unsafe { at.add(offset).write_volatile(value) }
}

fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8 ^ 0x5a
}

fn size_of(fd: i32) -> i64 {
    let mut buf = [0u8; 144];
    // SAFETY: `buf` は `struct stat` より大きい。
    if unsafe { fstat(fd, buf.as_mut_ptr()) } != 0 {
        return -1;
    }
    i64::from_le_bytes(buf[STAT_SIZE_OFFSET..STAT_SIZE_OFFSET + 8].try_into().unwrap())
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");

    // 1. fd を閉じてもマッピングが残る
    let len = 4 * PAGE;
    let fd = sized_memfd(len, 0);
    let first = map(fd, len);
    if first.is_null() {
        fail(&format!("mmap failed with errno {}", errno()), 90);
    }
    for offset in 0..len {
        write_at(first, offset, pattern(offset));
    }
    close_fd(fd);
    let kept = (0..len).all(|offset| read_at(first, offset) == pattern(offset));
    let other = sized_memfd(len, 0);
    let other_map = map(other, len);
    if other_map.is_null() {
        fail(&format!("mmap failed with errno {}", errno()), 90);
    }
    for offset in 0..len {
        write_at(other_map, offset, 0xee);
    }
    let unchanged = (0..len).all(|offset| read_at(first, offset) == pattern(offset));
    println!(
        "{NAME}: after closing the fd, the mapping still reads the pattern = {kept}, and a new memfd did not change it = {unchanged}"
    );
    unmap(other_map, len);
    close_fd(other);

    // 2. 共有のマッピングの munmap
    println!("{NAME}: munmap of the shared mapping returned {}", unmap(first, len));

    // 3. 途中だけを外す
    let fd = sized_memfd(3 * PAGE, 0);
    let three = map(fd, 3 * PAGE);
    if three.is_null() {
        fail(&format!("mmap failed with errno {}", errno()), 90);
    }
    for page in 0..3 {
        write_at(three, page * PAGE, 0x10 + page as u8);
    }
    // SAFETY: 真ん中のページは、このマッピングの内側である。
    let middle = unmap(unsafe { three.add(PAGE) }, PAGE);
    let ends = read_at(three, 0) == 0x10 && read_at(three, 2 * PAGE) == 0x12;
    // SAFETY: 後ろのページは、このマッピングの内側である。
    let rest = unmap(three, PAGE) == 0 && unmap(unsafe { three.add(2 * PAGE) }, PAGE) == 0;
    close_fd(fd);
    println!(
        "{NAME}: munmap of the middle page returned {middle}, the first and last pages still read = {ends}, and the rest unmapped = {rest}"
    );

    // 4. mmap と munmap を、1 プロセスのマッピングの数より多くくり返す
    let fd = sized_memfd(PAGE, 0);
    let mut done = 0;
    for round in 0..MAP_ROUNDS {
        let at = map(fd, PAGE);
        if at.is_null() {
            println!("{NAME}: mmap failed at round {round} with errno {}", errno());
            break;
        }
        write_at(at, 0, round as u8);
        if unmap(at, PAGE) != 0 {
            println!("{NAME}: munmap failed at round {round} with errno {}", errno());
            break;
        }
        done += 1;
    }
    close_fd(fd);
    println!("{NAME}: {MAP_ROUNDS} rounds of mmap and munmap on one memfd: all succeeded = {}", done == MAP_ROUNDS);

    // 5. 作る→大きくする→マップ→閉じる→外す、を間を置かずにくり返す
    for round in 0..POOL_ROUNDS {
        let fd = new_memfd(0);
        if fd < 0 {
            fail(&format!("memfd_create failed at round {round} with errno {}", errno()), 95);
        }
        // SAFETY: このプログラムが作った fd。
        if unsafe { ftruncate(fd, POOL_BYTES as i64) } != 0 {
            fail(&format!("ftruncate to 16 MiB failed at round {round} with errno {}", errno()), 95);
        }
        let at = map(fd, POOL_BYTES);
        if at.is_null() {
            fail(&format!("mmap of 16 MiB failed at round {round} with errno {}", errno()), 95);
        }
        write_at(at, 0, 1);
        write_at(at, POOL_BYTES - 1, 2);
        close_fd(fd);
        if unmap(at, POOL_BYTES) != 0 {
            fail(&format!("munmap of 16 MiB failed at round {round} with errno {}", errno()), 95);
        }
    }
    println!(
        "{NAME}: {POOL_ROUNDS} rounds of memfd_create, a 16 MiB ftruncate, mmap, close and munmap: all succeeded = true"
    );

    // 6. マップして外した後の memfd を縮める
    let fd = sized_memfd(4 * PAGE, MFD_ALLOW_SEALING);
    let at = map(fd, 4 * PAGE);
    if at.is_null() {
        fail(&format!("mmap failed with errno {}", errno()), 90);
    }
    write_at(at, 0, 0x77);
    unmap(at, 4 * PAGE);
    // SAFETY: このプログラムが作った fd。
    let shrunk = unsafe { ftruncate(fd, PAGE as i64) };
    let size = size_of(fd);
    let again = map(fd, PAGE);
    let first_byte = if again.is_null() { -1 } else { i32::from(read_at(again, 0)) };
    if !again.is_null() {
        unmap(again, PAGE);
    }
    println!(
        "{NAME}: after munmap, ftruncate to one page returned {shrunk}, fstat says {size} byte(s), and the first byte reads {first_byte:#x}"
    );
    // SAFETY: このプログラムが作った fd。
    let sealed = unsafe { fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK) };
    // SAFETY: 同上。
    let refused = unsafe { ftruncate(fd, 0) } == -1 && errno() == EPERM;
    println!("{NAME}: with F_SEAL_SHRINK, shrinking failed with EPERM = {refused} (seal {sealed})");
    close_fd(fd);

    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
