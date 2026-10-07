//! 隔離の容量（256 フレーム）より多いフレームを持つプロセス（2026-10-07。空間を壊すときに漏れないことの確かめ）。
//!
//! 16 MiB の無名の `mmap` を 2 つ取り（無名の `mmap` の 1 回の上限）、各ページに 1 バイト書いて、0 で終わる。
//! フレームは 8,192 本と表の分になる。判定はカーネルが破棄の後に出す行で行う——`user-destroy:` の行（道と掛かった時間）と、
//! `spawn: /bin/frame-hog ended` の行（取った数と集めた数が釣り合い、漏れが 0）。起動時に `frame-hog-test` の構成が起こす。
#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, syscall6, write_all, PROT_READ_WRITE, STDOUT, SYS_MMAP};

/// `MAP_PRIVATE | MAP_ANONYMOUS`（Linux の値）。
const MAP_PRIVATE_ANONYMOUS: u64 = 0x22;
/// 1 回に取る大きさ（無名の `mmap` の 1 回の上限）。
const CHUNK: u64 = 16 * 1024 * 1024;
const PAGE: u64 = 4096;

/// # Safety
///
/// カーネルが初期スタックを指す `stack` を渡して呼ぶ（`userlib` の約束）。ここでは使わない。
#[no_mangle]
pub unsafe extern "sysv64" fn zeikos_main(_stack: *const u64) -> ! {
    for _ in 0..2 {
        // SAFETY: カーネルが場所を決めて範囲を検証する。
        let addr = unsafe { syscall6(SYS_MMAP, 0, CHUNK, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS, u64::MAX, 0) };
        if addr < 0 {
            write_all(STDOUT, b"frame-hog: mmap failed\n");
            exit(1);
        }
        let base = addr as u64 as *mut u8;
        let mut offset = 0u64;
        while offset < CHUNK {
            // SAFETY: 今 `mmap` で写した範囲の中である。
            unsafe { base.add(offset as usize).write_volatile(1) };
            offset += PAGE;
        }
    }
    write_all(STDOUT, b"frame-hog: holding 32 MiB in 2 anonymous mappings\n");
    exit(0)
}
