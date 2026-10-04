//! `syscall-insn`: `syscall` 命令でシステムコールを呼び、`int 0x80` と同じ結果になることを確かめるユーザープログラム
//! （2026-10-04）。
//!
//! # crate ではない
//!
//! `syscall-test.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独で
//! リンクし、できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何を確かめるか
//!
//! Linux 向けのプログラムは、システムコールを `syscall` 命令で呼ぶ。ZeikOS は `int 0x80` の入口を残したまま、
//! `syscall` 命令の入口を足した。**どちらの入口から呼んでも、同じ番号表を引き、同じ結果が返ること**を、同じ呼び出しを
//! 両方の入口で打って比べる。
//!
//! 1. **6 つの引数が届く。** 確かめ用の番号（probe）を `syscall` 命令で呼ぶ。届いた 6 つの値は、カーネルの側が
//!    突き合わせる。
//! 2. **両方の入口で同じ結果。** `write`・`brk(0)`・時計・無い番号（`-ENOSYS`）・誤ったポインタ（`-EFAULT`）。
//! 3. **レジスタ。** 戻った後、RCX は戻り先、R11 は呼ぶ前の RFLAGS で、ほかの汎用レジスタは呼ぶ前のままである
//!    （Linux の決まりと同じ）。
//! 4. **旗を立てたまま呼んでも、カーネルが止まらずに戻る。** 方向の旗（DF）・NT・AC を、それぞれ立ててから呼ぶ。
//!    NT は、落とさずにカーネルへ入ると、戻りの `iretq` がカーネルの中で例外を起こす旗である。
//! 5. **`mov ss` の直後の `syscall` も通る。**
//!
//! 最後は `syscall` 命令で `exit(0)` を呼ぶ。
//!
//! # 終了状態
//!
//! - `0` すべて通った
//! - `1` probe の戻り値が違った
//! - `2` `write` の結果が、入口で違った（または、渡したバイト数でなかった）
//! - `3` `brk(0)` の結果が、入口で違った
//! - `4` 時計の結果が、入口で違った（または、0 でなかった）
//! - `5` 無い番号の結果が、入口で違った（または、`-ENOSYS` でなかった）
//! - `6` 誤ったポインタの結果が、入口で違った（または、`-EFAULT` でなかった）
//! - `7` 戻った後の RCX が、戻り先でなかった
//! - `8` 戻った後の R11 が、呼ぶ前の RFLAGS でなかった
//! - `9` 戻った後、保たれるはずのレジスタが変わっていた
//! - `10` 方向の旗を立てて呼んだ結果が違った（または、戻った後に旗が落ちていた）
//! - `11` NT を立てて呼んだ結果が違った
//! - `12` AC を立てて呼んだ結果が違った
//! - `13` `mov ss` の直後に呼んだ結果が違った
//!
//! **失敗の報告は `int 0x80` の `exit` で行う**——確かめている側の入口に頼らない。

#![no_std]
#![no_main]

/// 確かめ用の番号（probe）と、返るはずの値、渡す 6 つの引数。**`syscall-test.rs` と同じ値である**（カーネルの
/// `PROBE_ARGS` と対になっている）。
const PROBE_NUMBER: u32 = 0x1000;
const PROBE_RETURN: u32 = 0x00C0_FFEE;
const PROBE_ARG0: u32 = 0x1111_1111;
const PROBE_ARG1: u32 = 0x2222_2222;
const PROBE_ARG2: u32 = 0x3333_3333;
const PROBE_ARG3: u32 = 0x4444_4444;
const PROBE_ARG4: u32 = 0x5555_5555;
const PROBE_ARG5: u32 = 0x6666_6666;

/// Linux の番号（x86_64）。
const SYS_WRITE: u32 = 1;
const SYS_BRK: u32 = 12;
const SYS_EXIT: u32 = 60;
const SYS_CLOCK_GETTIME: u32 = 228;
/// 実装していない番号。
const SYS_UNKNOWN: u32 = 0x999;
const CLOCK_MONOTONIC: u32 = 1;
const MINUS_ENOSYS: i32 = -38;
const MINUS_EFAULT: i32 = -14;

/// `write` で送るバイト数。**`MESSAGE` の長さと、`kernel/src/main.rs` の `SYSCALL_INSN_MESSAGE` と対になっている。**
const MESSAGE_LEN: u32 = 24;

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。entry と最初の `PT_LOAD` の先頭を一致させない。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",

    // --- 1. probe を `syscall` 命令で呼ぶ。6 つの引数を、決まりどおりのレジスタへ置く ---
    "  mov edi, {arg0}",
    "  mov esi, {arg1}",
    "  mov edx, {arg2}",
    "  mov r10d, {arg3}",
    "  mov r8d, {arg4}",
    "  mov r9d, {arg5}",
    "  mov eax, {probe}",
    "  syscall",
    "  cmp rax, {probe_ret}",
    "  mov edi, 1",
    "  jne 9f",

    // --- 2. write。両方の入口で、渡したバイト数が返る ---
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  syscall",
    "  mov edi, 2",
    "  cmp rax, r12",
    "  jne 9f",
    "  cmp rax, {msg_len}",
    "  jne 9f",

    // --- 3. brk(0)。両方の入口で、同じ上端が返る ---
    "  mov eax, {sys_brk}",
    "  xor edi, edi",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_brk}",
    "  xor edi, edi",
    "  syscall",
    "  mov edi, 3",
    "  cmp rax, r12",
    "  jne 9f",

    // --- 4. 時計。両方の入口で 0 が返る（読んだ値そのものは、呼ぶたびに進むので比べない） ---
    "  sub rsp, 16",
    "  mov eax, {sys_clock}",
    "  mov edi, {clock_monotonic}",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_clock}",
    "  mov edi, {clock_monotonic}",
    "  mov rsi, rsp",
    "  syscall",
    "  add rsp, 16",
    "  mov edi, 4",
    "  cmp rax, r12",
    "  jne 9f",
    "  test rax, rax",
    "  jnz 9f",

    // --- 5. 無い番号。両方の入口で -ENOSYS が返る ---
    "  mov eax, {sys_unknown}",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_unknown}",
    "  syscall",
    "  mov edi, 5",
    "  cmp rax, r12",
    "  jne 9f",
    "  cmp rax, {minus_enosys}",
    "  jne 9f",

    // --- 6. 誤ったポインタ（カーネルの番地）。両方の入口で -EFAULT が返る ---
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  mov rsi, 0xffffffff80100000",
    "  mov edx, 4",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  mov rsi, 0xffffffff80100000",
    "  mov edx, 4",
    "  syscall",
    "  mov edi, 6",
    "  cmp rax, r12",
    "  jne 9f",
    "  cmp rax, {minus_efault}",
    "  jne 9f",

    // --- 7〜9. レジスタ。目印を置いてから無い番号を呼び、戻った後に見る ---
    // 呼ぶ前の RFLAGS を、スタックに控える（この後の `mov` は旗を変えない）。
    "  mov rbx, 0x0b0b0001",
    "  mov rdx, 0x0d0d0002",
    "  mov rsi, 0x05050003",
    "  mov rdi, 0x0d1d0004",
    "  mov rbp, 0x0b9b0005",
    "  mov r8,  0x08080006",
    "  mov r9,  0x09090007",
    "  mov r10, 0x10100008",
    "  mov r12, 0x12120009",
    "  mov r13, 0x1313000a",
    "  mov r14, 0x1414000b",
    "  mov r15, 0x1515000c",
    "  pushfq",
    "  mov eax, {sys_unknown}",
    "  syscall",
    "3:",
    // 7: RCX は戻り先（`syscall` 命令の次の番地）。
    "  lea rax, [rip + 3b]",
    "  cmp rcx, rax",
    "  mov eax, 7",
    "  jne 8f",
    // 8: R11 は呼ぶ前の RFLAGS。
    "  pop rax",
    "  cmp r11, rax",
    "  mov eax, 8",
    "  jne 8f",
    // 9: ほかのレジスタは、呼ぶ前のまま。
    "  mov eax, 9",
    "  cmp rbx, 0x0b0b0001",
    "  jne 8f",
    "  cmp rdx, 0x0d0d0002",
    "  jne 8f",
    "  cmp rsi, 0x05050003",
    "  jne 8f",
    "  cmp rdi, 0x0d1d0004",
    "  jne 8f",
    "  cmp rbp, 0x0b9b0005",
    "  jne 8f",
    "  cmp r8,  0x08080006",
    "  jne 8f",
    "  cmp r9,  0x09090007",
    "  jne 8f",
    "  cmp r10, 0x10100008",
    "  jne 8f",
    "  cmp r12, 0x12120009",
    "  jne 8f",
    "  cmp r13, 0x1313000a",
    "  jne 8f",
    "  cmp r14, 0x1414000b",
    "  jne 8f",
    "  cmp r15, 0x1515000c",
    "  jne 8f",

    // --- 10. 方向の旗（DF）を立てたまま呼ぶ。戻った後も、ユーザーの旗は立ったままである ---
    "  std",
    "  mov eax, {sys_unknown}",
    "  syscall",
    "  mov edi, 10",
    "  cmp rax, {minus_enosys}",
    "  jne 7f",
    "  pushfq",
    "  pop rax",
    "  test eax, 0x400",
    "  jz 7f",
    "  cld",

    // --- 11. NT を立てたまま呼ぶ ---
    "  pushfq",
    "  or qword ptr [rsp], 0x4000",
    "  popfq",
    "  mov eax, {sys_unknown}",
    "  syscall",
    "  mov edi, 11",
    "  cmp rax, {minus_enosys}",
    "  jne 9f",
    "  pushfq",
    "  and qword ptr [rsp], -0x4001",
    "  popfq",

    // --- 12. AC を立てたまま呼ぶ ---
    "  pushfq",
    "  or qword ptr [rsp], 0x40000",
    "  popfq",
    "  mov eax, {sys_unknown}",
    "  syscall",
    "  mov edi, 12",
    "  cmp rax, {minus_enosys}",
    "  jne 9f",
    "  pushfq",
    "  and qword ptr [rsp], -0x40001",
    "  popfq",

    // --- 13. `mov ss` の直後の `syscall` ---
    "  mov dx, ss",
    "  mov eax, {sys_unknown}",
    "  mov ss, dx",
    "  syscall",
    "  mov edi, 13",
    "  cmp rax, {minus_enosys}",
    "  jne 9f",

    // --- 終わり。`syscall` 命令で exit(0) ---
    "  mov eax, {sys_exit}",
    "  xor edi, edi",
    "  syscall",
    "  ud2",

    // 失敗の出口。**`int 0x80` で終わる。**
    // 7: 方向の旗を立てたまま来る（先に降ろす）。
    "7:",
    "  cld",
    "  jmp 9f",
    // 8: 終了状態が EAX に在る。
    "8:",
    "  mov edi, eax",
    // 9: 終了状態が EDI に在る。
    "9:",
    "  mov eax, {sys_exit}",
    "  int 0x80",
    "  ud2",

    ".section .rodata",
    "MESSAGE:",
    "  .ascii \"syscall-insn wrote this\\n\"",

    arg0 = const PROBE_ARG0,
    arg1 = const PROBE_ARG1,
    arg2 = const PROBE_ARG2,
    arg3 = const PROBE_ARG3,
    arg4 = const PROBE_ARG4,
    arg5 = const PROBE_ARG5,
    probe = const PROBE_NUMBER,
    probe_ret = const PROBE_RETURN,
    sys_write = const SYS_WRITE,
    sys_brk = const SYS_BRK,
    sys_exit = const SYS_EXIT,
    sys_clock = const SYS_CLOCK_GETTIME,
    sys_unknown = const SYS_UNKNOWN,
    clock_monotonic = const CLOCK_MONOTONIC,
    minus_enosys = const MINUS_ENOSYS,
    minus_efault = const MINUS_EFAULT,
    msg_len = const MESSAGE_LEN,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
