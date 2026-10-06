//! x86_64 の Linux のシステムコールの番号（`asm/unistd_64.h`。`ADR-0071` の決定 1 の 2 で `crate::syscall` から移した。
//! 2026-09-30）。
//!
//! **Linux に同じ入口がある呼び出しは、Linux の番号をそのまま使う**（`ADR-0020` の Addendum）。**ここには番号と、
//! どのヘッダの値かの 1 行だけを置く**——受ける形・断り方・採らない入口などの振る舞いの説明は、`crate::syscall` の
//! 処理の側にある。Linux に無い独自の番号は [`crate::abi::private`] にある。
//!
//! **CPU によって違う**——aarch64 は番号が違い、`open`・`stat`・`poll`・`mkdir`・`rmdir`・`unlink` の入口が無い
//! （`openat`・`newfstatat`・`ppoll`・`mkdirat`・`unlinkat` の形だけがある。2026-09-30 に `aarch64-linux-gnu-gcc` と
//! クロスのヘッダで測った）。

/// `write(fd, buf, len)` の番号（S9-b-1。`asm/unistd_64.h` の `__NR_write`）。
pub const SYS_WRITE: u64 = 1;

/// `exit(status)` の番号（S9-b-3-1。`asm/unistd_64.h` の `__NR_exit`）。
pub const SYS_EXIT: u64 = 60;

/// `read(fd, buf, count)` の番号（S10-b。`asm/unistd_64.h` の `__NR_read`）。
pub const SYS_READ: u64 = 0;

/// `getdents64(fd, dirp, count)` の番号（S10-b。`asm/unistd_64.h` の `__NR_getdents64`）。
pub const SYS_GETDENTS64: u64 = 217;

/// `stat(path, statbuf)` の番号（S10-b。`asm/unistd_64.h` の `__NR_stat`）。
pub const SYS_STAT: u64 = 4;

/// `clock_gettime(clockid, timespec)` の番号（W2-d+。`asm/unistd_64.h` の `__NR_clock_gettime`）。
pub const SYS_CLOCK_GETTIME: u64 = 228;

/// `nanosleep(req, rem)` の番号（W2-d+。`asm/unistd_64.h` の `__NR_nanosleep`）。
pub const SYS_NANOSLEEP: u64 = 35;

/// `open(path, flags, mode)` の番号（S10-b。`asm/unistd_64.h` の `__NR_open`）。
pub const SYS_OPEN: u64 = 2;

/// `close(fd)` の番号（S10-b。`asm/unistd_64.h` の `__NR_close`）。
pub const SYS_CLOSE: u64 = 3;

/// `ioctl(fd, request, arg)` の番号（e-1。`asm/unistd_64.h` の `__NR_ioctl`）。
pub const SYS_IOCTL: u64 = 16;

/// `lseek` の番号（DIR-1b。`asm/unistd_64.h` の `__NR_lseek`）。
pub const SYS_LSEEK: u64 = 8;

/// `mkdir` の番号（DIR-1c。`asm/unistd_64.h` の `__NR_mkdir`）。
pub const SYS_MKDIR: u64 = 83;

/// `rmdir` の番号（DIR-1c。`asm/unistd_64.h` の `__NR_rmdir`）。
pub const SYS_RMDIR: u64 = 84;

/// `brk` の番号（H-a。ADR-0044。`asm/unistd_64.h` の `__NR_brk`）。
pub const SYS_BRK: u64 = 12;

/// `arch_prctl(code, address)` の番号（2026-10-05。`asm/unistd_64.h` の `__NR_arch_prctl`）。
pub const SYS_ARCH_PRCTL: u64 = 158;

/// `arch_prctl` の `code`（`asm/prctl.h`）。**GS の基底を入れる。**
pub const ARCH_SET_GS: u64 = 0x1001;
/// `arch_prctl` の `code`。**FS の基底を入れる**（libc が、スレッドローカルの領域を据えるのに使う）。
pub const ARCH_SET_FS: u64 = 0x1002;
/// `arch_prctl` の `code`。**FS の基底を、渡した番地へ書く。**
pub const ARCH_GET_FS: u64 = 0x1003;
/// `arch_prctl` の `code`。**GS の基底を、渡した番地へ書く。**
pub const ARCH_GET_GS: u64 = 0x1004;

/// `unlink` の番号（DIR-1b。`asm/unistd_64.h` の `__NR_unlink`）。
pub const SYS_UNLINK: u64 = 87;

/// `socket` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_socket`）。
pub const SYS_SOCKET: u64 = 41;

/// `connect` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_connect`）。
pub const SYS_CONNECT: u64 = 42;

/// `accept` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_accept`）。
pub const SYS_ACCEPT: u64 = 43;

/// `bind` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_bind`）。
pub const SYS_BIND: u64 = 49;

/// `listen` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_listen`）。
pub const SYS_LISTEN: u64 = 50;

/// `poll` の番号（`ADR-0066` の Y-b。`asm/unistd_64.h` の `__NR_poll`）。
pub const SYS_POLL: u64 = 7;

/// `mmap` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_mmap`）。
pub const SYS_MMAP: u64 = 9;

/// `ftruncate` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_ftruncate`）。
pub const SYS_FTRUNCATE: u64 = 77;

/// `sendmsg` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_sendmsg`）。
pub const SYS_SENDMSG: u64 = 46;

/// `recvmsg` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_recvmsg`）。
pub const SYS_RECVMSG: u64 = 47;

/// `memfd_create` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_memfd_create`）。
pub const SYS_MEMFD_CREATE: u64 = 319;

/// `fstat(fd, statbuf)` の番号（2026-10-06。`__NR_fstat`）。
pub const SYS_FSTAT: u64 = 5;
/// `rt_sigaction(sig, act, oldact, sigsetsize)` の番号（2026-10-06。`__NR_rt_sigaction`）。
pub const SYS_RT_SIGACTION: u64 = 13;
/// `rt_sigprocmask(how, set, oldset, sigsetsize)` の番号（2026-10-06。`__NR_rt_sigprocmask`）。
pub const SYS_RT_SIGPROCMASK: u64 = 14;
/// `sendto(fd, buf, len, flags, addr, addrlen)` の番号（2026-10-06。`__NR_sendto`）。
pub const SYS_SENDTO: u64 = 44;
/// `uname(buf)` の番号（2026-10-06。`__NR_uname`）。
pub const SYS_UNAME: u64 = 63;
/// `fcntl(fd, cmd, arg)` の番号（2026-10-06。`__NR_fcntl`）。
pub const SYS_FCNTL: u64 = 72;
/// `readlink(path, buf, bufsiz)` の番号（2026-10-06。`__NR_readlink`）。
pub const SYS_READLINK: u64 = 89;
/// `sigaltstack(ss, old_ss)` の番号（2026-10-06。`__NR_sigaltstack`）。
pub const SYS_SIGALTSTACK: u64 = 131;
/// `futex(uaddr, op, val, timeout, uaddr2, val3)` の番号（2026-10-06。`__NR_futex`）。
pub const SYS_FUTEX: u64 = 202;
/// `set_tid_address(tidptr)` の番号（2026-10-06。`__NR_set_tid_address`）。
pub const SYS_SET_TID_ADDRESS: u64 = 218;
/// `exit_group(status)` の番号（2026-10-06。`__NR_exit_group`）。
pub const SYS_EXIT_GROUP: u64 = 231;
/// `prlimit64(pid, resource, new, old)` の番号（2026-10-06。`__NR_prlimit64`）。
pub const SYS_PRLIMIT64: u64 = 302;
/// `getrandom(buf, len, flags)` の番号（2026-10-06。`__NR_getrandom`）。
pub const SYS_GETRANDOM: u64 = 318;
/// `munmap(addr, len)` の番号（2026-10-06。`__NR_munmap`）。
pub const SYS_MUNMAP: u64 = 11;
