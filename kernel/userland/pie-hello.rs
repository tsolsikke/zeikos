//! `pie-hello`: 位置独立の像（`ET_DYN`）として載せられ、補助ベクタ（`auxv`）を自分で確かめるユーザープログラム
//! （2026-10-06）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独でリンクし、
//! できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # ほかのプログラムとの違い
//!
//! **番地 0 からリンクした `ET_DYN` である**（リンカスクリプトは `pie.ld`）。ローダーは、像の全体を決まった量だけ
//! ずらして載せる。**再配置は 1 つも要らない**——命令は、どれも今の番地からの相対で書いてある（`lea … [rip + …]`）。
//! musl の静的な位置独立のプログラムは、起動の最初に自分で再配置するが、その前に走る数命令は、これと同じ形である。
//!
//! # 何を確かめるか
//!
//! **入口の時点のスタックから `auxv` まで歩き、積まれた値を、自分で求めた値と突き合わせる。** 食い違えば、
//! その番号を終了状態にして終わる（カーネルの側の `PIE_HELLO_STATUS` が、番号を言葉にする）。全部合えば、
//! 1 行を書いて 0 で終わる。
//!
//! | 終了状態 | 意味 |
//! |---|---|
//! | 1 | `AT_ENTRY` が、実際に走っている `_start` の番地と違う |
//! | 2 | `AT_PHDR` が無いか、指す先の最初の項が `PT_PHDR` でない |
//! | 3 | `AT_PHENT` が 56 でない |
//! | 4 | `AT_PHNUM` が、ELF ヘッダの `e_phnum` と違う |
//! | 5 | `AT_PAGESZ` が 4096 でない |
//! | 6 | `AT_RANDOM` が無いか、指す 16 バイトが全部 0 である |
//! | 7 | `AT_EXECFN` が無いか、指す文字列が `pie-` で始まらない |
//! | 9 | `_start` が、ずらす量（`0x400000`）より下で走っている（ずらされていない） |
//! | 10 | `auxv` の終端（`AT_NULL`）が、32 対の中に無い |
//!
//! # 位置を固定している所
//!
//! 終了の呼び出しは `_start + 0x180` に置き、その直後（`_start + 0x187`）に `ud2` を置く。カーネルの側は、
//! 終了が効かなかったときの落ち先として、この位置を出す（`PIE_HELLO_UD2_OFFSET`）。

#![no_std]
#![no_main]

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。entry と区画の先頭を一致させない。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // 入口のスタック: argc / argv[] / NULL / envp[] / NULL / auxv の対 / AT_NULL。
    "  mov r12, rsp",
    "  mov rax, [r12]",
    // envp の先頭 = rsp + 8（argc）+ argc * 8 + 8（argv の終端）。
    "  lea rbx, [r12 + rax * 8 + 16]",
    // envp の終端を越える。
    "2:",
    "  mov rax, [rbx]",
    "  add rbx, 8",
    "  test rax, rax",
    "  jnz 2b",
    // auxv を歩いて、要る値を控える。
    "  xor r8d, r8d",   // AT_ENTRY
    "  xor r9d, r9d",   // AT_PHDR
    "  xor r10d, r10d", // AT_PAGESZ
    "  xor r13d, r13d", // AT_RANDOM
    "  xor r14d, r14d", // AT_EXECFN
    "  xor r15d, r15d", // AT_PHNUM
    "  xor ebp, ebp",   // AT_PHENT
    "  mov ecx, 32",
    "3:",
    "  mov rax, [rbx]",
    "  mov rdx, [rbx + 8]",
    "  add rbx, 16",
    "  test rax, rax",
    "  jz 4f",
    "  cmp rax, 9",
    "  cmove r8, rdx",
    "  cmp rax, 3",
    "  cmove r9, rdx",
    "  cmp rax, 6",
    "  cmove r10, rdx",
    "  cmp rax, 25",
    "  cmove r13, rdx",
    "  cmp rax, 31",
    "  cmove r14, rdx",
    "  cmp rax, 5",
    "  cmove r15, rdx",
    "  cmp rax, 4",
    "  cmove rbp, rdx",
    "  dec ecx",
    "  jnz 3b",
    "  mov edi, 10",
    "  jmp 9f",
    "4:",
    // **実際に走っている番地**（今の番地からの相対で求める。再配置は要らない）。
    "  lea rax, [rip + _start]",
    // 9: ずらされていること。
    "  mov edi, 9",
    "  mov esi, 0x400000",
    "  cmp rax, rsi",
    "  jb 9f",
    // 1: AT_ENTRY。
    "  mov edi, 1",
    "  cmp r8, rax",
    "  jne 9f",
    // 2: AT_PHDR が指す先が、プログラムヘッダの表であること（最初の項が PT_PHDR = 6）。
    "  mov edi, 2",
    "  test r9, r9",
    "  jz 9f",
    "  cmp dword ptr [r9], 6",
    "  jne 9f",
    // 3: AT_PHENT。
    "  mov edi, 3",
    "  cmp rbp, 56",
    "  jne 9f",
    // 4: AT_PHNUM が、ELF ヘッダの e_phnum と同じこと（表は、ヘッダの直後の 0x40 に在る。e_phnum は 0x38）。
    "  mov edi, 4",
    "  movzx eax, word ptr [r9 - 8]",
    "  cmp r15, rax",
    "  jne 9f",
    // 5: AT_PAGESZ。
    "  mov edi, 5",
    "  cmp r10, 4096",
    "  jne 9f",
    // 6: AT_RANDOM が指す 16 バイトが、全部 0 ではないこと。
    "  mov edi, 6",
    "  test r13, r13",
    "  jz 9f",
    "  mov rax, [r13]",
    "  or rax, [r13 + 8]",
    "  jz 9f",
    // 7: AT_EXECFN が指す文字列が `pie-` で始まること。
    "  mov edi, 7",
    "  test r14, r14",
    "  jz 9f",
    "  cmp dword ptr [r14], 0x2d656970",
    "  jne 9f",
    // 全部合った。write(fd=1, buf=MESSAGE, len)。
    "  mov eax, 1",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {message_len}",
    "  int 0x80",
    "  xor edi, edi",
    // **終了の呼び出しの位置を固定する**（詰め物は nop。ここまで落ちてきても、そのまま終了へ進む）。
    ".org 0x180, 0x90",
    "9:",
    "  mov eax, 60",
    "  int 0x80",
    // **`exit` が戻ってきたときの受け皿**（`_start + 0x187`）。
    "  ud2",

    ".section .rodata",
    "MESSAGE:",
    "  .ascii \"hello from a position-independent program\\n\"",
    message_len = const 42,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
