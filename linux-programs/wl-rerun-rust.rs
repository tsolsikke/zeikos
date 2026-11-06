//! `wl-rerun-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**引数のプログラムを、決まった回数だけ
//! 続けて起こす**（2026-11-05。Seinas の `--seinas-zeyes-exit-test`）。
//!
//! 同時に走れる Ring 3 は 2 本で、シェルはコンポジタが終わるまで待つ。そこで、パイプの左にこのプログラムを置き、ZeikOS の私用の `spawn`
//! で zeyes-min を入れ子に起こす。zeyes-min が Ctrl+C で終わると、少し間をおいて次の 1 回を起こす。コンポジタが次の接続を受けられることを
//! 確かめるためである。**ZeikOS の上でだけ動く**（Linux には私用の番号が無い）ので、Linux の参照は持たない。
//!
//! 使い方: `wl-rerun-rust <回数> <間のミリ秒> <プログラム> [引数...]`
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::ffi::CString;
use std::time::Duration;

const NAME: &str = "wl-rerun-rust";
/// ZeikOS の私用の `spawn`（`kernel/src/abi/private.rs` の `SYS_SPAWN`）。`spawn(path, argv, envp)`。子が終わるまで戻らない。
const SYS_SPAWN: i64 = 0x1004;

extern "C" {
    fn syscall(number: i64, ...) -> i64;
}

/// NULL で終わるポインタの並び。
fn pointers(strings: &[CString]) -> Vec<*const i8> {
    strings
        .iter()
        .map(|string| string.as_ptr().cast())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [times, pause, command @ ..] = args.as_slice() else {
        eprintln!("{NAME}: usage: wl-rerun-rust TIMES PAUSE_MS PROGRAM [ARGS...]");
        std::process::exit(2);
    };
    let times: u32 = times.parse().unwrap_or(0);
    let pause: u64 = pause.parse().unwrap_or(0);
    let argv: Vec<CString> = command.iter().filter_map(|arg| CString::new(arg.as_str()).ok()).collect();
    let envp: Vec<CString> =
        std::env::vars().filter_map(|(name, value)| CString::new(format!("{name}={value}")).ok()).collect();
    let (argv_pointers, envp_pointers) = (pointers(&argv), pointers(&envp));
    for run in 1..=times {
        if run > 1 {
            std::thread::sleep(Duration::from_millis(pause));
        }
        // SAFETY: 渡すポインタは、どれもこの関数の中で生きている。
        let status = unsafe { syscall(SYS_SPAWN, argv[0].as_ptr(), argv_pointers.as_ptr(), envp_pointers.as_ptr()) };
        eprintln!("{NAME}: run {run} of {times} ended with status {status}");
    }
}
