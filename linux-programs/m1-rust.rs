//! `m1-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。
//!
//! # 何のためのものか
//!
//! ZeikOS が Linux のプログラムをそのまま動かせること（M1）を確かめる本体である。**Linux 上でも ZeikOS 上でも、同じ
//! 出力と同じ終了の状態になる**ことを、控えた参照（`linux-programs/reference/`）と突き合わせる。出力に、環境で変わる値
//! （環境変数の中身、番地、時刻、1 つ目の引数の道）は入れない。
//!
//! # 何をするか
//!
//! 1. 引数の数と環境変数の数と、2 つ目以降の引数を出す（1 つ目は読むファイルの道で、Linux 側と ZeikOS 側で違うので
//!    出さない）。ここまでで `std` の起動の列を通る——`arch_prctl`・`set_tid_address`・`poll`・`rt_sigaction`・
//!    `sigaltstack`・`mmap`・`mprotect`・`rt_sigprocmask`（2026-10-06 の Linux 上の `strace`）。
//! 2. 引数の 1 つ目を道としてファイルを読み、長さと中身を出す（`open`・`fcntl`・`fstat`・`lseek`・`read`・`close`）。
//! 3. 1 MiB の `Vec` を確保して両端へ書く（`malloc` の大きな確保。`brk`・`mmap`・`munmap`）。
//! 4. `HashMap` に数個入れて取り出す（`RandomState` が `getrandom` を打つ）。
//! 5. 終了の状態は引数の数（`exit_group`）。
//!
//! # crate ではない
//!
//! `kernel/userland/hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回、
//! `--target x86_64-unknown-linux-musl` で呼び、ディスク像の `/bin/linux` に置く。

use std::collections::HashMap;
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let env_count = std::env::vars_os().count();
    let mut out = std::io::stdout().lock();

    writeln!(out, "m1-rust: {} argument(s), {} environment variable(s)", args.len(), env_count).unwrap();
    for (index, arg) in args.iter().enumerate().skip(2) {
        writeln!(out, "m1-rust: argv[{index}] = {arg}").unwrap();
    }

    if let Some(path) = args.get(1) {
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut text = String::new();
                match file.read_to_string(&mut text) {
                    Ok(length) => {
                        writeln!(out, "m1-rust: read {length} byte(s) from the file named by argv[1]").unwrap();
                        for line in text.lines() {
                            writeln!(out, "m1-rust: | {line}").unwrap();
                        }
                    }
                    Err(error) => writeln!(out, "m1-rust: read failed: {}", error.kind()).unwrap(),
                }
            }
            Err(error) => writeln!(out, "m1-rust: open failed: {}", error.kind()).unwrap(),
        }
    }

    let mut big = vec![0u8; 1 << 20];
    big[0] = 0x5a;
    let last = big.len() - 1;
    big[last] = 0xa5;
    let sum: u32 = big.iter().map(|&b| b as u32).sum();
    writeln!(out, "m1-rust: 1 MiB vector, ends {:#x} {:#x}, sum {sum}", big[0], big[last]).unwrap();
    drop(big);

    let mut table = HashMap::new();
    for (key, value) in [("one", 1u32), ("two", 2), ("three", 3), ("four", 4)] {
        table.insert(key, value);
    }
    let total: u32 = table.values().sum();
    writeln!(out, "m1-rust: hash map with {} entries, total {total}, two = {}", table.len(), table["two"]).unwrap();

    out.flush().unwrap();
    std::process::exit(args.len() as i32);
}
