//! `wl-relative-path-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**相対パスと空のパスと
//! `getcwd`**（2026-11-02。`ADR-0043` の Addendum。作業ディレクトリはいつも `/`）。
//!
//! # 何をするか
//!
//! `/tmp/seinas/relpath.txt` を絶対パスで作り、相対パスで引く。システムコールは libc を通さず、`syscall` で打つ（musl の `stat`
//! は `newfstatat` を打つが、ここで見たいのは `stat` そのものである）。
//!
//! 1. `stat` を、相対・`./` で始まる形・`../` で始まる形・無いファイル・空のパスで打つ。
//! 2. `open`（読むだけ）を相対パスで打ち、中身を読む。
//! 3. `readlink` を、記号リンクでないファイルと、無いファイルで打つ。
//! 4. `mkdir` と `rmdir` を相対パスで打つ。
//! 5. `unlink` を、在るファイルと無いファイルの相対パスで打つ。
//! 6. `getcwd` を、置き場が十分なとき・ちょうど 2 バイト・1 バイト・0 バイト・NULL で打つ。
//!
//! 相対パスの `stat` が断られたら、名指しの行を出して 91 で終わる（破壊テスト `stat-refuses-relative-test`）。
//!
//! 行はどれも標準出力へ出す。終了ステータスは引数の数（4）。Linux でも、作業ディレクトリを `/` にして同じ行を出す
//! （`linux-programs/reference/wl-relative-path-rust.txt`）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。**起動時に起こすのは `linux-socket-test` と `relative-path-test` のビルドだけである。**

use std::fs;

const NAME: &str = "wl-relative-path-rust";
const DIR: &str = "/tmp/seinas";
const FILE: &str = "/tmp/seinas/relpath.txt";
const CONTENT: &[u8] = b"relative paths start at the root\n";

const SYS_READ: i64 = 0;
const SYS_OPEN: i64 = 2;
const SYS_CLOSE: i64 = 3;
const SYS_STAT: i64 = 4;
const SYS_GETCWD: i64 = 79;
const SYS_MKDIR: i64 = 83;
const SYS_RMDIR: i64 = 84;
const SYS_UNLINK: i64 = 87;
const SYS_READLINK: i64 = 89;

extern "C" {
    fn syscall(number: i64, ...) -> i64;
    fn __errno_location() -> *mut i32;
}

fn fail(what: &str, code: i32) -> ! {
    println!("{NAME}: {what}; exiting with {code}");
    std::process::exit(code)
}

/// 生の呼び出しの答えを、成功なら値、失敗なら負の errno にする（musl の `syscall` は失敗のとき -1 を返して errno を置く）。
fn answer(ret: i64) -> i64 {
    if ret < 0 {
        // SAFETY: musl のスレッドごとの errno。
        -(unsafe { *__errno_location() } as i64)
    } else {
        ret
    }
}

/// errno の名前（行に出すもの）。
fn name_of(ret: i64) -> String {
    match ret {
        -2 => "ENOENT".into(),
        -14 => "EFAULT".into(),
        -22 => "EINVAL".into(),
        -34 => "ERANGE".into(),
        r if r >= 0 => format!("{r}"),
        r => format!("errno {}", -r),
    }
}

/// NUL で終わるパスを作る。
fn c_path(path: &str) -> Vec<u8> {
    let mut bytes = path.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn stat(path: &str) -> i64 {
    let path = c_path(path);
    let mut buf = [0u8; 144];
    // SAFETY: NUL で終わるパスと、`struct stat` より大きい置き場。
    answer(unsafe { syscall(SYS_STAT, path.as_ptr(), buf.as_mut_ptr()) })
}

fn main() {
    let argument_count = std::env::args().count();
    println!("{NAME}: main starts with {argument_count} argument(s)");
    let _ = fs::create_dir_all(DIR);
    fs::write(FILE, CONTENT).unwrap_or_else(|error| fail(&format!("writing {FILE} failed: {error}"), 90));

    // 1. stat
    let relative = stat("tmp/seinas/relpath.txt");
    if relative != 0 {
        fail(&format!("stat of a relative path failed with errno {}", -relative), 91);
    }
    println!(
        "{NAME}: stat of tmp/seinas/relpath.txt returned {}, ./tmp/seinas/relpath.txt {}, ../tmp/seinas/relpath.txt {}",
        name_of(relative),
        name_of(stat("./tmp/seinas/relpath.txt")),
        name_of(stat("../tmp/seinas/relpath.txt"))
    );
    println!(
        "{NAME}: stat of tmp/seinas/no-such-file returned {}, and of the empty path {}",
        name_of(stat("tmp/seinas/no-such-file")),
        name_of(stat(""))
    );

    // 2. open
    let path = c_path("tmp/seinas/relpath.txt");
    // SAFETY: NUL で終わるパス。読むだけで開く。
    let fd = answer(unsafe { syscall(SYS_OPEN, path.as_ptr(), 0i64) });
    let mut buf = [0u8; 64];
    let read = if fd >= 0 {
        // SAFETY: このプログラムが開いた fd と、置き場の長さ。
        let read = answer(unsafe { syscall(SYS_READ, fd, buf.as_mut_ptr(), buf.len()) });
        // SAFETY: このプログラムが開いた fd を閉じるだけである。
        unsafe { syscall(SYS_CLOSE, fd) };
        read
    } else {
        fd
    };
    println!(
        "{NAME}: open of tmp/seinas/relpath.txt for reading gave an fd = {}, and read the content = {}",
        fd >= 0,
        read == CONTENT.len() as i64 && &buf[..CONTENT.len()] == CONTENT
    );

    // 3. readlink
    let readlink = |path: &str| {
        let path = c_path(path);
        let mut buf = [0u8; 64];
        // SAFETY: NUL で終わるパスと、置き場の長さ。
        answer(unsafe { syscall(SYS_READLINK, path.as_ptr(), buf.as_mut_ptr(), buf.len()) })
    };
    println!(
        "{NAME}: readlink of tmp/seinas/relpath.txt (not a link) returned {}, and of tmp/seinas/nope {}",
        name_of(readlink("tmp/seinas/relpath.txt")),
        name_of(readlink("tmp/seinas/nope"))
    );

    // 4. mkdir と rmdir
    let dir = c_path("tmp/seinas/relpath-dir");
    // SAFETY: NUL で終わるパス。
    let made = answer(unsafe { syscall(SYS_MKDIR, dir.as_ptr(), 0o755i64) });
    let seen = stat("/tmp/seinas/relpath-dir");
    // SAFETY: 同上。
    let removed = answer(unsafe { syscall(SYS_RMDIR, dir.as_ptr()) });
    println!(
        "{NAME}: mkdir of tmp/seinas/relpath-dir returned {}, it was there as /tmp/seinas/relpath-dir = {}, and rmdir returned {}",
        name_of(made),
        seen == 0,
        name_of(removed)
    );

    // 5. unlink
    let unlink = |path: &str| {
        let path = c_path(path);
        // SAFETY: NUL で終わるパス。
        answer(unsafe { syscall(SYS_UNLINK, path.as_ptr()) })
    };
    let gone = unlink("tmp/seinas/relpath.txt");
    println!(
        "{NAME}: unlink of tmp/seinas/relpath.txt returned {}, the file is gone = {}, and unlink of tmp/seinas/nope returned {}",
        name_of(gone),
        stat(FILE) == -2,
        name_of(unlink("tmp/seinas/nope"))
    );

    // 6. getcwd
    let getcwd = |buf: *mut u8, size: usize| {
        // SAFETY: 置き場は呼び手が渡す（NULL も試す。カーネルが確かめる）。
        answer(unsafe { syscall(SYS_GETCWD, buf, size) })
    };
    let mut big = [0xaau8; 64];
    let got = getcwd(big.as_mut_ptr(), big.len());
    println!(
        "{NAME}: getcwd with 64 bytes returned {}, and wrote {:?}",
        name_of(got),
        String::from_utf8_lossy(&big[..got.max(0) as usize])
    );
    let mut exact = [0xaau8; 4];
    let got = getcwd(exact.as_mut_ptr(), 2);
    println!(
        "{NAME}: getcwd with 2 bytes returned {}, wrote / and NUL = {}, and left the rest = {}",
        name_of(got),
        exact[..2] == *b"/\0",
        exact[2..] == [0xaa, 0xaa]
    );
    let mut small = [0xaau8; 4];
    let one = getcwd(small.as_mut_ptr(), 1);
    let zero = getcwd(small.as_mut_ptr(), 0);
    println!(
        "{NAME}: getcwd with 1 byte returned {}, with 0 bytes {}, and wrote nothing = {}",
        name_of(one),
        name_of(zero),
        small == [0xaa; 4]
    );
    println!(
        "{NAME}: getcwd into NULL with 64 bytes returned {}",
        name_of(getcwd(std::ptr::null_mut(), 64))
    );

    println!("{NAME}: done");
    std::process::exit(argument_count as i32);
}
