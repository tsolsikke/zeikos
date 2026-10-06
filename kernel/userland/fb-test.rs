//! `fb-test`: `/dev/fb0` を Linux の fbdev の形で開き、四隅に色を置いて、`FBIOZPRESENT` を打たずに画面へ出るのを待つ
//! ユーザープログラム（2026-10-07。M2 の最初の刻み）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独でリンクし、
//! ディスク像の `/bin` に置く。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! 1. `open("/dev/fb0", O_RDWR)`。**`SYS_OPEN_SCREEN`（ZeikOS 独自）ではなく、Linux の道で開く**——Seinas の fbdev の裏側が
//!    そうするため。
//! 2. `ioctl(FBIOGET_VSCREENINFO)`・`ioctl(FBIOGET_FSCREENINFO)` で形を訊き、`mmap(MAP_SHARED)` で面を写す。
//! 3. 四隅に 80 画素の四角を置く——左上 赤、右上 緑、左下 青、右下 黄（Seinas の fbdev の確かめの絵と同じ配置。向きと
//!    色の並びが分かる）。**`FBIOZPRESENT` は打たない**——Linux の fbdev は書けば映るので、カーネルが間隔ごとに写すはずである。
//! 4. `fb-test: painted` と言って、1.5 秒眠る（`nanosleep`。その間に `xtask` が `screendump` で読む）。
//! 5. `fb-test: slept` と言って、`munmap` し、`close` する（図形モードから抜け、文字の画面が戻る）。0 で終わる。
//!
//! # 終わり方（`argv[1]`）
//!
//! - 無し: 上のとおり `close` して 0 で終わる。
//! - `noclose`: `munmap` も `close` も打たずに `exit(0)`。**プロセスの終わりに表ごと閉じられ、最後の転送と文字の画面が戻る**
//!   ことを見る（`fb-test: exiting without close`）。
//! - `fold`: 眠った後に番地 0 へ書いて、ページフォルトで畳まれる（`fb-test: folding on purpose`）。同じく表ごと閉じられる
//!   ことを見る。
//!
//! 終了状態: `1` 開けなかった／`2` 形が訊けなかった・32 ビットでない・小さすぎる／`3` `mmap` が失敗／`4` `nanosleep` が
//! 失敗／`5` `close` が失敗。

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{argument, close, exit, mmap_shared, screen_info, syscall3, write_all, STDOUT, SYS_OPEN};

/// `nanosleep(req, rem)` の番号（Linux と同じ）。
const SYS_NANOSLEEP: u64 = 35;
/// `munmap(addr, len)` の番号（Linux と同じ）。
const SYS_MUNMAP: u64 = 11;
/// `O_RDWR`。
const O_RDWR: u64 = 2;
/// 四隅の四角の 1 辺（画素）。`xtask` の判定と対になっている。
const CORNER: u32 = 80;
/// 色（`0x00RRGGBB`。画面の並びが BGR でも RGB でも、赤・緑・青の位置は `screen_info` で読んで並べる）。
const RED: (u8, u8, u8) = (0xff, 0x00, 0x00);
const GREEN: (u8, u8, u8) = (0x00, 0xff, 0x00);
const BLUE: (u8, u8, u8) = (0x00, 0x00, 0xff);
const YELLOW: (u8, u8, u8) = (0xff, 0xff, 0x00);

fn say(text: &[u8]) {
    write_all(STDOUT, text);
    write_all(STDOUT, b"\n");
}

fn fail(text: &[u8], status: u64) -> ! {
    say(text);
    exit(status)
}

/// `(r, g, b)` を、画面の並び（赤と青のビット位置）に合わせた 32 ビットの画素にする。
fn pixel(color: (u8, u8, u8), red_offset: u32, blue_offset: u32) -> u32 {
    let (r, g, b) = color;
    (u32::from(r) << red_offset) | (u32::from(g) << 8) | (u32::from(b) << blue_offset)
}

/// `argv[1]` が `word` か（NUL 終端の比較）。
unsafe fn argument_is(stack: *const u64, word: &[u8]) -> bool {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指し、`argv` の文字列は NUL 終端である。
    let Some(mut at) = (unsafe { argument(stack, 1) }) else {
        return false;
    };
    for &expected in word {
        // SAFETY: NUL 終端の文字列の中で、終端の前までを読む。
        let have = unsafe { *at };
        if have != expected {
            return false;
        }
        // SAFETY: 同上。
        at = unsafe { at.add(1) };
    }
    // SAFETY: 同上。終端の NUL を読む。
    unsafe { *at == 0 }
}

#[no_mangle]
pub unsafe extern "sysv64" fn zeikos_main(stack: *const u64) -> ! {
    // SAFETY: この関数の契約により `stack` は初期スタックの先頭である。
    let no_close = unsafe { argument_is(stack, b"noclose") };
    // SAFETY: 同上。
    let fold = unsafe { argument_is(stack, b"fold") };
    let path = b"/dev/fb0\0";
    // SAFETY: `path` は NUL 終端の静的な文字列で、`open` はそれを読むだけである。
    let fd = unsafe { syscall3(SYS_OPEN, path.as_ptr() as u64, O_RDWR, 0) };
    if fd < 0 {
        fail(b"fb-test: open(/dev/fb0, O_RDWR) failed", 1);
    }
    let fd = fd as u64;
    say(b"fb-test: opened /dev/fb0");
    let info = match screen_info(fd) {
        Ok(info) => info,
        Err(_) => fail(b"fb-test: the fbdev ioctls failed", 2),
    };
    if info.bits_per_pixel != 32 || info.width < 2 * CORNER || info.height < 2 * CORNER {
        fail(b"fb-test: the screen is not 32 bits per pixel, or is too small", 2);
    }
    let mapped = mmap_shared(fd, u64::from(info.smem_len));
    if mapped < 0 {
        fail(b"fb-test: mmap failed", 3);
    }
    let base = mapped as usize as *mut u8;
    let corners = [
        (0, 0, RED),
        (info.width - CORNER, 0, GREEN),
        (0, info.height - CORNER, BLUE),
        (info.width - CORNER, info.height - CORNER, YELLOW),
    ];
    for (x0, y0, color) in corners {
        let value = pixel(color, info.red_offset, info.blue_offset);
        for y in y0..y0 + CORNER {
            for x in x0..x0 + CORNER {
                let at = (y * info.line_length + x * 4) as usize;
                // SAFETY: `at` は `mmap` で写した面（`smem_len` バイト）の中である（四隅は画面の中に収まる）。
                unsafe { core::ptr::write_volatile(base.add(at).cast::<u32>(), value) };
            }
        }
    }
    say(b"fb-test: painted");

    // 1.5 秒眠る（`struct timespec { tv_sec: 1, tv_nsec: 500_000_000 }`）。
    let request: [u64; 2] = [1, 500_000_000];
    // SAFETY: `request` は `struct timespec` の 16 バイトで、この関数の間は生きている。`rem` は渡さない（0）。
    let slept = unsafe { syscall3(SYS_NANOSLEEP, request.as_ptr() as u64, 0, 0) };
    if slept < 0 {
        fail(b"fb-test: nanosleep failed", 4);
    }
    say(b"fb-test: slept");
    if fold {
        say(b"fb-test: folding on purpose");
        // SAFETY ではない——**わざと番地 0 へ書いて、ページフォルトで畳まれる**（`close` を打たずに終わる形の 1 つ）。
        // SAFETY: この書きは失敗するためのもので、成功したら `exit(6)` で違いが分かる。
        unsafe { core::ptr::write_volatile(core::ptr::null_mut::<u64>(), 1) };
        fail(b"fb-test: the write to address 0 did not fault", 6);
    }
    if no_close {
        say(b"fb-test: exiting without close");
        exit(0)
    }

    // SAFETY: `mmap` が返した範囲をそのまま外す。以後、`base` には触らない。
    let _ = unsafe { syscall3(SYS_MUNMAP, mapped as u64, u64::from(info.smem_len), 0) };
    if close(fd) < 0 {
        fail(b"fb-test: close failed", 5);
    }
    say(b"fb-test: closed /dev/fb0");
    exit(0)
}
