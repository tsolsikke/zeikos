//! `syscall-test`: ABI の契約をユーザー側から確かめるプログラム（S9-b-3-2a）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が
//! `rustc` を 1 回呼んで単独でリンクし、できた ELF を kernel が `include_bytes!`
//! で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 確かめているのは dispatch ではない
//!
//! カーネルには既に往復の検証がある（`verify_syscall_roundtrip`）。**あちらは
//! ユーザールーチンの機械語をカーネルが Rust で組み立てている**——オペコードを
//! 直接並べ、即値を `to_le_bytes` で埋める。**つまり ABI の符号化はカーネルの
//! 著者の手にある。**
//!
//! こちらは asm で `mov rdi, ...` と書き、**アセンブラが符号化し、リンカが配置し、
//! ローダーが写像し、`iretq` が飛ばす。** 確かめているのは dispatch ではなく、
//! **`ADR-0020` が決めた ABI の契約そのものが、その経路を通っても成り立つこと**
//! である。
//!
//! # 二重に見る
//!
//! - **カーネル側**——`dispatch` が記録した番号と 6 引数を、既知値と突き合わせる。
//!   **第 4 引数が R10 から来ていることが `ADR-0020` の要である。**
//! - **ユーザー側**——戻り値を自分で検算し、結果を `exit` の終了状態で返す。
//!   **終了状態の一致は S9-b-3-1 で判定行になっており、反証も用意してある**ので、
//!   新しい観測路を作らずに済む。
//!
//! # 終了状態の意味
//!
//! **値に意味がある。** どの検算が落ちたかは、この値でしか分からない。
//! **カーネル側の `SYSCALL_TEST_STATUS` と対になっている。**
//!
//! - `0` すべて通った
//! - `1` probe の戻り値が `PROBE_RETURN` でなかった
//! - `2` `write` が渡したバイト数を返さなかった
//! - `3` 実装していない番号が `-ENOSYS` を返さなかった
//! - `4` `open("/etc/motd", O_RDONLY)` が 3 番を返さなかった
//! - `5` `close(3)` が 0 を返さなかった
//! - `6` 閉じた直後の `open` が 3 番を返さなかった（スロットが空いていない）
//! - `7` `open("/nope")` が `-ENOENT` を返さなかった
//! - `8` `open("/etc/motd", O_WRONLY)` が `-EROFS` を返さなかった
//! - `9` `close` の 2 度目が `-EBADF` を返さなかった
//! - `10` `open(NULL)` が `-EFAULT` を返さなかった
//! - `11` `/etc/motd` の全長 `read` が 18 を返さなかった
//! - `12` 読めたバイト列が既知の中身と食い違った
//! - `13` 末尾での `read` が 0 を返さなかった
//! - `14` 5 バイトの短い `read` が 5 を返さなかった、または中身が食い違った
//! - `15` 続きの `read` が残りの 13 を返さなかった、または中身が食い違った
//! - `16` ディレクトリの `read` が `-EISDIR` を返さなかった
//! - `17` 閉じた fd の `read` が `-EBADF` を返さなかった
//! - `18` `stat("/etc/motd")` が 0 を返さなかった
//! - `19` `st_size` が 18 でなかった
//! - `20` `st_mode` が通常ファイルを表していなかった
//! - `21` `st_blocks` が 8 でなかった（**512 バイト単位**）
//! - `22` `/etc` の `st_mode` がディレクトリを表していなかった
//! - `23` `stat("/nope")` が `-ENOENT` を返さなかった
//! - `24` `getdents64` がバッファを埋めなかった
//! - `25` ルートの一覧が 6 エントリでなかった
//! - `26` `d_reclen` が 8 の倍数でなかった
//! - `27` `d_type` が通常ファイルとディレクトリを分けなかった
//! - `28` 末尾での `getdents64` が 0 を返さなかった
//! - `29` 1 レコードも収まらないバッファで `-EINVAL` を返さなかった
//! - `30` `argc` が 2 でなかった
//! - `31` `argv[0]` が "syscall-test" でなかった
//! - `32` `argv[1]` が "alpha" でなかった
//! - `33` `argv[2]`（終端）が NULL でなかった
//! - `34` `envp[0]` が NULL だった（f-1。**以前は `"TERM=zaytos"` と突き合わせていたが、出どころがファイルになって前提が消えたりする**）
//! - `61` `envp` の終端が NULL でなかった
//! - `62` `brk(0)` が正の上端を返さなかった
//! - `63` `brk` で 2 ページ伸ばせなかった
//! - `64` 伸ばした 1 ページ目の末尾が読み書きできなかった
//! - `65` 伸ばした 2 ページ目の末尾が読み書きできなかった
//! - `66` 上限を越える要求が `-ENOMEM` で断られなかった
//! - `67` `brk` で元へ縮められなかった
//! - `68` `spawn(path, argv, NULL)` が `-EFAULT` を返さなかった（f-2）
//! - `35` `auxv` の終端（`AT_NULL`）が無かった
//! - `36` `spawn("/bin/hello")` が 0 を返さなかった
//! - `79` `spawn("/bin/pie-hello")` が 0 を返さなかった（位置独立の像を、ファイルシステムを通る道で載せる。2026-10-06）
//! - `37` `spawn("/nope")` が `-ENOENT` を返さなかった
//! - `38` `spawn("/etc")` が `-EISDIR` を返さなかった
//! - `39` `spawn(NULL)` が `-EFAULT` を返さなかった
//! - `40` `spawn("/bin/spawn-test")` が 0 を返さなかった（孫が断られなかった）
//! - `41` `spawn(path, NULL)` が `-EFAULT` を返さなかった
//! - `42` 要素数が上限を越える `argv` が `-E2BIG` を返さなかった
//! - `43` 全体が長すぎる `argv` が `-E2BIG` を返さなかった
//! - `44` `write(0, ...)` が渡したバイト数を返さなかった（0 も同じ端末である）
//! - `45` `write(3, ...)` が `-EBADF` を返さなかった
//! - `46` `write(2, ...)` が渡したバイト数を返さなかった
//! - `47` 64 バイトを越える `write` が渡したバイト数を返さなかった
//! - `51` `read(0)` が `-EAGAIN` を返さなかった（打鍵が無い）
//! - `52` `read(1)` が `-EAGAIN` を返さなかった（1 も同じ端末である）
//! - `53` `read(3)` が `-EBADF` を返さなかった（開いていない）
//! - `48` `spawn("/bin/ls", ["ls"])` が 0 を返さなかった
//! - `49` `spawn("/bin/cat", ["cat", "/etc/motd"])` が 0 を返さなかった
//! - `50` `spawn("/bin/cat", ["cat", "/nope"])` が 1 を返さなかった（開けない。
//!   **(b3) までは「引数が無い」の 2 を見ていた**——`cat` が標準入力を読むようになり、
//!   引数なしは打鍵を待つ形になったので、**「子の 0 以外の状態が親へ届く」を別の入口で見る**）
//! - `54` `open("/data/writable", O_WRONLY|O_TRUNC)` が fd 3 を返さなかった
//! - `55` 書きで開いた fd への `read` が `-EBADF` を返さなかった
//! - `56` ファイルへの `write` が渡したバイト数を返さなかった
//! - `57` 書いた後の `close` が 0 を返さなかった
//! - `58` 読み戻しが書いた中身と一致しなかった（長さ・バイト列・EOF）
//! - `59` 読みで開いた fd への `write` が `-EBADF` を返さなかった
//! - `60` カナリア（/etc/motd）が変わっていた（別のファイルへ書いた）
//! - `61` ファイルの fd への `ioctl` が `-ENOTTY` を返さなかった（端末ではない）
//! - `62` 端末への `ioctl(TIOCGWINSZ)` が 0 を返さなかった
//! - `63` 画面の無い文脈なのに行が 0 でなかった（**起動シーケンスには前景が無い**）
//! - `64` 知らない要求が `-ENOTTY` を返さなかった
//! - `65` `socket(AF_UNIX, SOCK_STREAM, 0)` が最小の空き番号（3）を返さなかった、または閉じられなかった（`ADR-0064`）
//! - `66` 待ち受けの無い名前への `connect` が `-ECONNREFUSED` を返さなかった（`ADR-0064`）
//! - `67` `memfd_create`＋`ftruncate`＋`mmap` した共有メモリへ書いた値が読み戻せなかった（`ADR-0065`）
//! - `68` 共有メモリでない fd（stdin）の `mmap` が `-EBADF` を返さなかった（`ADR-0065`）
//! - `71` 実行できる保護（`PROT_EXEC`）を求めた `mmap` が `-EPERM` を返さなかった（2026-10-03）
//! - `72` `arch_prctl(ARCH_SET_FS)` が 0 を返さなかった、または `fs:0` から、基底の先に置いた値が読めなかった（2026-10-05）
//! - `73` `arch_prctl(ARCH_GET_FS)` が、入れた基底を返さなかった
//! - `74` GS で同じこと（`ARCH_SET_GS`・`gs:0`・`ARCH_GET_GS`）ができなかった
//! - `75` 正準でない番地を基底にする求めが `-EPERM` を返さなかった、または基底が変わってしまった
//! - `76` カーネルの番地を基底にする求めが `-EPERM` を返さなかった
//! - `77` 知らない `code` が `-EINVAL` を返さなかった
//! - `78` 書けない番地を渡した `ARCH_GET_FS` が `-EFAULT` を返さなかった
//! - `80` `set_tid_address` が 1（唯一のスレッドの番号）を返さなかった（2026-10-06）
//! - `81` `rt_sigaction(SIGPIPE, SIG_IGN)` が 0 を返さなかったか、読み戻した登録が `SIG_IGN` でなかった
//! - `82` `rt_sigaction(SIGKILL, …)` が `-EINVAL` を返さなかった
//! - `83` `rt_sigprocmask(SIG_BLOCK)` の後の問い合わせが、塞いだ集合を返さなかった
//! - `84` `sigaltstack` で据えた後の問い合わせが、同じ `ss_sp`・`ss_size` を返さなかった
//! - `85` `prlimit64(RLIMIT_STACK)` が 0 と `rlim_cur = 8 MiB` を返さなかった
//! - `86` `getrandom(16)` が 16 を返さなかったか、16 バイトが全部 0 だった
//! - `87` `futex(FUTEX_WAKE)` が 0 を返さなかった
//! - `88` 値の違う `futex(FUTEX_WAIT)` が `-EAGAIN` を返さなかった
//! - `89` `uname` が 0 を返さなかったか、`sysname` が `Linux` でなかった
//! - `90` `readlink("/proc/self/exe")` が、このプログラムの名前を返さなかった
//! - `91` 開いたファイルの `fstat` が、`stat` と同じ大きさを返さなかった
//! - `92` `fcntl(F_GETFD)` が 0 を返さなかったか、`F_DUPFD_CLOEXEC` が下限以上の番号を返さなかった
//! - `93` `spawn("/bin/futex-wait")` が 137 を返さなかった（起こす者の居ない `FUTEX_WAIT` は、プロセスを終わらせる）
//! - `94` `SA_RESTORER` の無いハンドラの `rt_sigaction(SIGUSR1, …)` が `-EINVAL` を返さなかった
//! - `95` `mmap(NULL, 2 ページ, RW, MAP_PRIVATE|MAP_ANONYMOUS)` が、基点以上の番地を返さなかった（2026-10-06）
//! - `96` 無名の写像が 0 で埋まっていなかったか、書いた値が読み戻せなかった
//! - `97` 写像の全体の `munmap` が 0 を返さなかったか、次の `mmap` が同じ番地を使い直さなかった
//! - `98` ページの境界に無い番地の `munmap` が `-EINVAL` を返さなかったか、長さ 0 の `mmap` が `-EINVAL` を返さなかった
//! - `99` 写像の無い範囲の `munmap` が 0 を返さなかった
//! - `100` 3 ページの写像の真ん中の `munmap` が 0 を返さなかったか、両側のページが残っていなかった（2026-10-06）
//! - `101` 残った 2 つの断片の `munmap` が 0 を返さなかった
//! - `102` 無名の写像の 2 ページ目への `MAP_FIXED` が、その番地と 0 のページを返さなかった
//! - `103` 写像の上への `MAP_FIXED_NOREPLACE` が `-EEXIST` を返さなかったか、空いた所でその番地を返さなかった
//! - `104` スタックとその見張りのページへの `MAP_FIXED` が `-EINVAL` を返さなかった
//! - `105` `brk` の見張りの流れ（`brk`、ヒープの先頭への `PROT_NONE` の `MAP_FIXED`、`brk` を戻す、`munmap`）が通らなかった
//! - `69` 方向フラグを立てたまま打った `clock_gettime` が 0 を返さなかった（2026-09-24。
//!   **判定の本体はカーネルの入口の監視である**——こちらは前提を作り、戻り値だけを見る）
//! - `70` 読み込み先が読み取り専用のページ（このプログラムの `.rodata`）の `read` が `-EFAULT` を返さなかった
//!
//! # `argv` は `_start` の時点の `rsp` から読む
//!
//! **カーネルが Linux と同じ形で積む**（`argc` / `argv[]` / NULL / `envp[]` /
//! NULL / `auxv`）。**入口で `rsp` を控えておかないと、後から辿れない。**
//!
//! # 中身の突き合わせは `hello` の `write` と同じ形である
//!
//! **既知のバイト列と一致することを示す。** カーネル側は種のファイルを
//! `include_bytes!` で持っており、**こちらはその写しを持つ。**
//! **食い違えば 12 番か 15 番の検算が落ちる**ので、静かには残らない。
//!
//! # `open` はこのプログラムの `.rodata` のパスを渡す
//!
//! **カーネルが受け取るのはユーザー空間のポインタである。** `UserSlice` と
//! ウィンドウ（`S9-b-3-2b` で 1 つにまとめたもの）がそのまま効くことを、**この経路が
//! 実際に通ることで確かめている。**
//!
//! # 定数はカーネルの写しである
//!
//! このファイルは crate に属さないので、`kernel/src/syscall.rs` の定数を
//! `use` できない。**同じ値を 2 か所で持つが、食い違えば判定行が落ちる**ので
//! 静かには残らない（`hello.rs` の `.org` と `HELLO_UD2_OFFSET` の関係と同じ）。

#![no_std]
#![no_main]

/// 検証用 probe の番号（`ZEIKOS_PRIVATE_BASE`）。
const PROBE_NUMBER: u32 = 0x1000;
/// probe が返す既知の値。
const PROBE_RETURN: u32 = 0x00C0_FFEE;
/// probe へ渡す 6 引数。**レジスタごとに区別できる値である。**
const PROBE_ARG0: u32 = 0x1111_1111;
const PROBE_ARG1: u32 = 0x2222_2222;
const PROBE_ARG2: u32 = 0x3333_3333;
const PROBE_ARG3: u32 = 0x4444_4444;
const PROBE_ARG4: u32 = 0x5555_5555;
const PROBE_ARG5: u32 = 0x6666_6666;
/// RCX へ入れる番兵。**RCX は引数ではない**（`ADR-0020`。第 4 引数は R10）。
const SENTINEL_RCX: u32 = 0xCCCC_CCCC;
/// `write` の番号（Linux と同じ 1）。
const SYS_WRITE: u32 = 1;
/// `exit_group` の番号（Linux と同じ 231。2026-10-06 から、終わりはこれで呼ぶ——Linux の libc が終わりに呼ぶ入口）。
const SYS_EXIT_GROUP: u32 = 231;
/// **永久に実装しない番号。** `-ENOSYS` が返ることを確かめるための的である。
const NEVER_IMPLEMENTED: u32 = 0x10FF;
/// `-ENOSYS`。失敗は `-errno` で返る（`ADR-0020`）。
const MINUS_ENOSYS: i32 = -38;
/// `socket` の番号（Linux x86-64。`ADR-0064`）。
const SYS_SOCKET: u32 = 41;
/// `connect` の番号。
const SYS_CONNECT: u32 = 42;
/// `clock_gettime` の番号（Linux x86-64）。
const SYS_CLOCK_GETTIME: u32 = 228;
/// `CLOCK_MONOTONIC`（Linux x86-64）。**カーネルが答えるのはこの時計だけである。**
const CLOCK_MONOTONIC: u32 = 1;
/// `-ECONNREFUSED`。
const MINUS_ECONNREFUSED: i32 = -111;
/// `SOCKADDR_NOBODY` の長さ（`sa_family_t` 2 + `"nobody"` 6 + NUL 1）。
const SOCKADDR_NOBODY_LEN: u32 = 9;
/// `memfd_create` の番号（`ADR-0065`）。
const SYS_MEMFD_CREATE: u32 = 319;
/// `ftruncate` の番号。
const SYS_FTRUNCATE: u32 = 77;
/// `mmap` の番号。
const SYS_MMAP: u32 = 9;
/// `mmap` の大きさ（1 ページ）。
const SHM_TEST_LEN: u32 = 4096;
/// `mmap` に書き込む目印（イメージとログの語に当たらない）。
const SHM_TEST_PATTERN: u32 = 0x5C0F_1234;
/// `PROT_READ | PROT_WRITE`。
const PROT_RW: u32 = 3;
/// `PROT_READ | PROT_EXEC`（2026-10-03。実行できる保護を求める形）。
const PROT_RX: u32 = 5;
/// `-EPERM`（Linux の値は 1）。
const MINUS_EPERM: i32 = -1;
/// `MAP_SHARED`。
const MAP_SHARED: u32 = 1;
/// `-EBADF`。
const MINUS_EBADF_SHM: i32 = -9;
/// 送るバイト列の長さ。
const MESSAGE_LEN: u32 = 24;
/// `open` の番号（Linux と同じ 2）。
const SYS_OPEN: u32 = 2;
/// `close` の番号（Linux と同じ 3）。
const SYS_CLOSE: u32 = 3;
/// 読み取りで開く（`O_RDONLY`）。
const O_RDONLY: u32 = 0;
/// 書き込みで開く（`O_WRONLY`）。**読み取り専用なので拒まれるはずである。**
const O_WRONLY: u32 = 1;

/// 書き込みで開き、同時に長さ 0 へ切る（zi-c。ADR-0037 の受理形）。
const O_WRONLY_TRUNC: u32 = 0o1001;

/// 54 番からの書き込みの検算が書く中身。**ビルドしたイメージの `/data/writable` の
/// 中身と違う列であること**——同じ中身を書くと「状態が変わらない」の種類で、
/// 破壊テストを立てても検算が通ってしまう。
const NEW_BODY_LEN: u32 = 23;
/// `-ENOENT`（そのパスは無い）。
const MINUS_ENOENT: i32 = -2;
/// `-EBADF`（そのファイルディスクリプタは開いていない）。
const MINUS_EBADF: i32 = -9;
/// `-ENOTTY`（端末に対する要求ではない。e-1）。
///
/// **端末でない fd への `ioctl` と、知らない要求の両方で返る**
/// （`kernel/src/syscall.rs` の `sys_ioctl`）。
const MINUS_ENOTTY: i32 = -25;
/// `ioctl` の番号（e-1）。**カーネルの `SYS_IOCTL` と同じ値である。**
const SYS_IOCTL: u32 = 16;
/// 端末の大きさを訊く要求（`TIOCGWINSZ`。e-1）。
const TIOCGWINSZ: u32 = 0x5413;
/// 知らない要求（e-1）。**`TIOCGWINSZ` でなければ何でもよい。**
const UNKNOWN_IOCTL: u32 = 0x5555;
/// `-EROFS`（読み取り専用のファイルシステム）。
const MINUS_EROFS: i32 = -30;
/// `-EFAULT`（不正なアドレス）。
const MINUS_EFAULT: i32 = -14;
/// `read` の番号（Linux と同じ 0）。
const SYS_READ: u32 = 0;
/// `-EISDIR`（ディレクトリに対して許されない操作）。
const MINUS_EISDIR: i32 = -21;
/// `/etc/motd` の長さ。**種のファイルと同じでなければ検算が落ちる。**
const MOTD_LEN: u32 = 18;
/// 短い `read` で読む長さ。
const MOTD_HEAD: u32 = 5;
/// その続きに残る長さ。
const MOTD_TAIL: u32 = 13;
/// 末尾を越えて要求する長さ。**`i_size` で切られるはずである。**
const OVER_READ: u32 = 100;
/// `stat` の番号（Linux と同じ 4）。
const SYS_STAT: u32 = 4;
/// `struct stat` の `st_mode` の位置（実測）。
const STAT_MODE_OFFSET: u32 = 24;
/// `struct stat` の `st_size` の位置（実測）。
const STAT_SIZE_OFFSET: u32 = 48;
/// `struct stat` の `st_blocks` の位置（実測）。
const STAT_BLOCKS_OFFSET: u32 = 64;
/// `st_mode` のうちファイル種別を表すビット。
const MODE_FORMAT_MASK: u32 = 0xF000;
/// 種別: 通常ファイル。
const MODE_REGULAR: u32 = 0x8000;
/// 種別: ディレクトリ。
const MODE_DIRECTORY: u32 = 0x4000;
/// `/etc/motd` が占める 512 バイト単位のブロック数。**4096 の 1 ブロック分である。**
const MOTD_BLOCKS: u32 = 8;
/// `getdents64` の番号（Linux と同じ 217）。
const SYS_GETDENTS64: u32 = 217;
/// ルートディレクトリのエントリ数（`. .. lost+found bin data etc tmp`）。
///
/// **DIR-1c で 6 から 7 になった**——**`/tmp` をイメージに足したためである。**
/// **f-1 で 8 になった**——**`/root` を足したためである**（`ADR-0052`）。
/// **B-d で 9 になった**——**`/lib` を足したためである**（`ADR-0042` の
/// Addendum。**既定のフォントの置き場である**）。
///
/// **この数はイメージの中身に寄りかかっている。** **置き場所を足したら、
/// ここも数え直すこと**（実測。**2 度とも、足した日にこの検算が起動を
/// 止めた**——**止まるので気づける。**）。
///
/// **数え方**——**ルート直下の項を数えた。** **`.` と `..` を含む**
/// （いまは `. .. lost+found bin data etc lib root tmp` の 9 つである）。
const ROOT_ENTRIES: u32 = 9;
/// `linux_dirent64` の `d_reclen` の位置。
const DIRENT_RECLEN_OFFSET: u32 = 16;
/// `linux_dirent64` の `d_type` の位置。
const DIRENT_TYPE_OFFSET: u32 = 18;
/// `d_type`: ディレクトリ。
const DT_DIR: u32 = 4;
/// `d_type`: 通常ファイル。
const DT_REG: u32 = 8;
/// **1 レコードも収まらない大きさ。** 固定部だけで 19 バイト要る。
const TINY_BUFFER: u32 = 16;
/// `-EINVAL`。
const MINUS_EINVAL: i32 = -22;
/// `-EEXIST`（2026-10-06。`MAP_FIXED_NOREPLACE` が、写像の上で返す）。
const MINUS_EEXIST: i32 = -17;
/// 期待する `argc`。**カーネルの `USER_PROGRAMS` の `argv` と対になっている。**
const EXPECTED_ARGC: u32 = 2;
/// `argv[0]` の長さ（NUL を含む）。
const ARGV0_LEN: u32 = 13;
/// `argv[1]` の長さ（NUL を含む）。
const ARGV1_LEN: u32 = 6;


/// `envp` を歩く上限（f-1b）。
///
/// **カーネルの `MAX_ENVP` より 1 つ大きい。** **終端そのものを踏む
/// 余地が要る**——**上限ぴったりだと、いっぱいに積んだときに
/// 終端へ届く前に打ち切ってしまう。**
///
/// **数え方**——**`kernel/src/userland.rs` の `MAX_ENVP`（8）に 1 を足した。**
const ENVP_WALK_MAX: usize = 9;
/// `spawn` の番号（`ZEIKOS_PRIVATE_BASE + 4`）。
const SYS_SPAWN: u32 = 0x1004;

/// `brk` の番号（Linux と同じ。H-a。ADR-0044）。
const SYS_BRK: u32 = 12;

/// `brk` で伸ばす量（2 ページ）。
///
/// **1 ページでは足りない**——**2 ページ目の末尾まで書けることを見たい。**
/// **「1 ページだけ写って 2 ページ目が無い」形が、1 ページでは検出されない。**
const BRK_GROWTH: u32 = 8192;

/// 伸ばした領域の末尾から測ったオフセット（最後の 4 バイト）。
const BRK_LAST_WORD: u32 = BRK_GROWTH - 4;

/// 1 ページ目の末尾（最後の 4 バイト）。
const BRK_FIRST_PAGE_LAST_WORD: u32 = 4092;

/// 伸ばした領域へ書く模様。**他の検算の値と紛れない値にする。**
const BRK_PATTERN: u32 = 0x5A5A_1234;

/// **上限を必ず越える要求。** **上限そのものを写さない**
/// ——**カーネルの定数を検算へ書き写すと、片方だけが古くなる。**
const BRK_TOO_FAR: u32 = 0x7FFF_FFFF;

/// `-ENOMEM`。
const MINUS_ENOMEM: i32 = -12;
/// `-E2BIG`（引数が多すぎる、または長すぎる）。
const MINUS_E2BIG: i32 = -7;
/// `-EAGAIN`（今は無い）。**端末に打鍵が溜まっていないときの答えである。**
const MINUS_EAGAIN: i32 = -11;
/// 64 バイトを越える 1 本の長さ。**記録用の緩衝より長いことが主張である。**
const LONG_MESSAGE_LEN: u32 = 68;

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // --- 30..35. 初期スタックが Linux の形で積まれていること ---
    // **入口の rsp をそのまま使う。** ここより前で何も push していない。
    // **後の検算は rbx を数えに使うので、控えても壊れる**——実測で踏んだ。
    // argc / argv[0] / argv[1] / NULL / envp[] / NULL / AT_NULL。
    "  cmp qword ptr [rsp], {argc}",
    "  mov edi, 30",
    "  jne 9f",
    // argv[0] は \"syscall-test\"。
    "  cld",
    "  mov rsi, [rsp + 8]",
    "  lea rdi, [rip + ARGV0_TEXT]",
    "  mov ecx, {argv0_len}",
    "  repe cmpsb",
    "  mov edi, 31",
    "  jne 9f",
    // argv[1] は \"alpha\"。
    "  cld",
    "  mov rsi, [rsp + 16]",
    "  lea rdi, [rip + ARGV1_TEXT]",
    "  mov ecx, {argv1_len}",
    "  repe cmpsb",
    "  mov edi, 32",
    "  jne 9f",
    // argv の終端。
    "  cmp qword ptr [rsp + 24], 0",
    "  mov edi, 33",
    "  jne 9f",
    // **envp[0] は NULL でない（f-1）。**
    //
    // **以前は \"TERM=zaytos\" と突き合わせていた**（EV。ADR-0041）。
    // **f-1 で環境の出どころがファイルになり、その前提が消えた**——
    // **利用者が `/etc/environment` の `TERM` を書き換えると、
    // ここが落ちて起動が止まる。** **実際に踏んだ**（実測。2026-08-31。
    // `--persist-env-test` の 2 度目が止まった）。
    //
    // **`/data/writable` の演習と同じ種類である**——**能力を足すと、
    // 既存の判定の前提が消える**（`docs/troubleshooting.md`）。
    //
    // **したがって、ここはカーネルが保証するものだけを見る**——
    // **`envp[0]` が在ること。** **値の突き合わせはホスト側へ移した**
    // （`--shell-test` の `echo a$TERM b` が `azeikos b` を出すこと。
    // **あちらは出どころがファイルでも定数でも、値そのものを見る**）。
    "  cmp qword ptr [rsp + 32], 0",
    "  mov edi, 34",
    "  je 9f",
    // envp の終端を歩いて探す（f-1b）。
    //
    // **以前は位置を要素数から決めていた**（`ENVP_TERMINATOR_OFFSET`）。
    // **f-1b でその前提が消えた**——**利用者が `/etc/environment` へ
    // 1 行足すと要素数が変わり、固定の位置が終端を指さなくなって
    // 起動が止まる。** **実際に踏んだ**（実測。2026-08-31。
    // `KEYMAP=us` を足した 2 度目が止まった）。
    //
    // **同じ種類の 3 度目である**——**`/data/writable` の演習、
    // `envp[0]` の突き合わせ、そしてここ**（`docs/troubleshooting.md`）。
    // **3 度とも「種の中身に寄りかかった判定」だった。**
    //
    // **歩けば要素数に寄りかからない。** **`rcx` を使う**——
    // **ここより後の検算は `rbx` と `r12` を使っており、`rcx` は
    // 直前の `repe cmpsb` が使い終えている。**
    //
    // **上限を置く**（`{envp_max}`）。**終端が無いイメージで無限に歩かない**
    // ——**歩き切ったら 61 番で落ちる。**
    "  lea rcx, [rsp + 32]",
    "  mov edx, {envp_max}",
    "10:",
    "  cmp qword ptr [rcx], 0",
    "  je 11f",
    "  add rcx, 8",
    "  dec edx",
    "  jnz 10b",
    "  mov edi, 61",
    "  jmp 9f",
    "11:",
    // auxv の終端（AT_NULL = 0）を、歩いて探す。**auxv は、envp の終端の次から、型と値の対で並ぶ**
    // （2026-10-06 に、カーネルが中身を積むようになった。それまでは終端だけで、envp の終端のすぐ次だった）。
    // **16 対の中に終端が無ければ、35 番で落ちる**（カーネルが積むのは、終端を入れて 13 対である）。
    "  lea rcx, [rcx + 8]",
    "  mov edx, 16",
    "12:",
    "  cmp qword ptr [rcx], 0",
    "  je 13f",
    "  add rcx, 16",
    "  dec edx",
    "  jnz 12b",
    "  mov edi, 35",
    "  jmp 9f",
    "13:",

    // --- 62..67. ヒープ（`brk`。H-a。ADR-0044） ---
    //
    // **`r12` は callee-saved で、`int 0x80` を跨いで残る**
    // （カーネルは GPR を復元する。`syscall-test` の他の検算が rbx で
    // 同じことをしている）。
    //
    // **問い合わせ（`brk(0)`）が正の上端を返すこと。**
    "  xor edi, edi",
    "  mov eax, {brk}",
    "  int 0x80",
    "  cmp rax, 0",
    "  mov edi, 62",
    "  jle 9f",
    "  mov r12, rax",
    // **2 ページ伸ばすと、要求した値がそのまま返ること。**
    "  lea rdi, [r12 + {brk_growth}]",
    "  mov eax, {brk}",
    "  int 0x80",
    "  lea rdx, [r12 + {brk_growth}]",
    "  cmp rax, rdx",
    "  mov edi, 63",
    "  jne 9f",
    // **伸ばした領域が読み書きできること。** **1 ページ目の末尾と
    // 2 ページ目の末尾の両方を見る**——**1 ページしかマップされていない形を検出する。**
    "  mov dword ptr [r12 + {brk_first_last}], {brk_pattern}",
    "  mov dword ptr [r12 + {brk_last}], {brk_pattern}",
    "  cmp dword ptr [r12 + {brk_first_last}], {brk_pattern}",
    "  mov edi, 64",
    "  jne 9f",
    "  cmp dword ptr [r12 + {brk_last}], {brk_pattern}",
    "  mov edi, 65",
    "  jne 9f",
    // **上限を越える要求が `-ENOMEM` で断られること。**
    "  mov edi, {brk_too_far}",
    "  mov eax, {brk}",
    "  int 0x80",
    "  cmp eax, {minus_enomem}",
    "  mov edi, 66",
    "  jne 9f",
    // **元へ縮められること。** **返ったかどうかはカーネルが数える**
    // （`user-heap:` の行。ホスト側が突き合わせる）。
    "  mov rdi, r12",
    "  mov eax, {brk}",
    "  int 0x80",
    "  cmp rax, r12",
    "  mov edi, 67",
    "  jne 9f",


    // --- 1. probe。6 引数を規約どおりのレジスタへ置く ---
    "  mov edi, {arg0}",
    "  mov esi, {arg1}",
    "  mov edx, {arg2}",
    "  mov r10d, {arg3}",
    "  mov r8d, {arg4}",
    "  mov r9d, {arg5}",
    // **RCX は引数ではない。** 番兵を置くので、カーネルが第 4 引数を RCX から
    // 読んでいれば、記録される値が食い違う。
    "  mov ecx, {sentinel}",
    "  mov eax, {probe}",
    "  int 0x80",
    "  cmp rax, {probe_ret}",
    "  mov edi, 1",
    "  jne 9f",

    // --- 2. write。渡したバイト数が返るはず ---
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  cmp rax, {msg_len}",
    "  mov edi, 2",
    "  jne 9f",

    // --- 3. 実装していない番号。-ENOSYS が返るはず ---
    "  mov eax, {never}",
    "  int 0x80",
    "  cmp rax, {minus_enosys}",
    "  mov edi, 3",
    "  jne 9f",

    // --- 4. open("/etc/motd", O_RDONLY)。最初の fd は 3 のはず ---
    // **0/1/2 は端末である**（S11-10。`kernel/src/vfs.rs` の `FileTable::new`）。
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, 3",
    "  mov edi, 4",
    "  jne 9f",

    // --- 5. close(3)。0 が返るはず ---
    "  mov eax, {sys_close}",
    "  mov edi, 3",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 5",
    "  jne 9f",

    // --- 6. もう一度 open。**閉じたスロットが空いているので、また 3 のはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, 3",
    "  mov edi, 6",
    "  jne 9f",

    // --- 7. 無いパス。-ENOENT が返るはず ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MISSING_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_enoent}",
    "  mov edi, 7",
    "  jne 9f",

    // --- 8. 書き込みで開く。**読み取り専用なので -EROFS のはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_wronly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_erofs}",
    "  mov edi, 8",
    "  jne 9f",

    // --- 9. 同じ fd を 2 度閉じる。**2 度目は -EBADF のはず** ---
    "  mov eax, {sys_close}",
    "  mov edi, 3",
    "  int 0x80",
    "  mov eax, {sys_close}",
    "  mov edi, 3",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 9",
    "  jne 9f",

    // --- 10. パスに NULL を渡す。**ウィンドウの下端より下なので -EFAULT のはず** ---
    // **`open` がユーザーポインタを検証していることの、否定側の観測である。**
    "  mov eax, {sys_open}",
    "  xor edi, edi",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 10",
    "  jne 9f",

    // --- 11/12. /etc/motd を全部読み、既知のバイト列と突き合わせる ---
    // **読み込み先はユーザースタックである**（このプログラムに書ける区画は無い）。
    "  sub rsp, 64",
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_len}",
    "  int 0x80",
    "  cmp rax, {motd_len}",
    "  mov edi, 11",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_BYTES]",
    "  mov ecx, {motd_len}",
    "  repe cmpsb",
    "  mov edi, 12",
    "  jne 9f",

    // --- 13. 末尾での read。**0 が返るはず（EOF）** ---
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_len}",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 13",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 70. 読み込み先が読み取り専用のページ。**-EFAULT が返るはず** ---
    // **読み込み先はこのプログラムの `.rodata` である**（書けない区画）。**カーネルが確かめずに書くと、
    // 読み取り専用のページへ書くことになる。** 開き直すのは、読める中身が残っている状態で確かめるためである。
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  lea rsi, [rip + MOTD_BYTES]",
    "  mov edx, {motd_len}",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 70",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 14/15. 短く読んでから続きを読む。**位置が進んでいること** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_head}",
    "  int 0x80",
    "  cmp rax, {motd_head}",
    "  mov edi, 14",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_BYTES]",
    "  mov ecx, {motd_head}",
    "  repe cmpsb",
    "  mov edi, 14",
    "  jne 9f",
    // **末尾を越えて要求する。** 残りの 13 だけが返るはず。
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {over_read}",
    "  int 0x80",
    "  cmp rax, {motd_tail}",
    "  mov edi, 15",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_REST]",
    "  mov ecx, {motd_tail}",
    "  repe cmpsb",
    "  mov edi, 15",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 16. ディレクトリを read。**-EISDIR が返るはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + ETC_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_head}",
    "  int 0x80",
    "  cmp rax, {minus_eisdir}",
    "  mov edi, 16",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 17. 閉じた fd を read。**-EBADF が返るはず** ---
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_head}",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 17",
    "  jne 9f",
    "  add rsp, 64",

    // --- 18..21. stat("/etc/motd")。**埋まる欄を突き合わせる** ---
    // `struct stat` は 144 バイトなので、スタックへ余裕を取る。
    "  sub rsp, 192",
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 18",
    "  jne 9f",
    "  mov rax, [rsp + {stat_size_off}]",
    "  cmp rax, {motd_len}",
    "  mov edi, 19",
    "  jne 9f",
    "  mov eax, [rsp + {stat_mode_off}]",
    "  and eax, {mode_mask}",
    "  cmp eax, {mode_regular}",
    "  mov edi, 20",
    "  jne 9f",
    "  mov rax, [rsp + {stat_blocks_off}]",
    "  cmp rax, {motd_blocks}",
    "  mov edi, 21",
    "  jne 9f",

    // --- 22. /etc は ディレクトリ ---
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + ETC_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov eax, [rsp + {stat_mode_off}]",
    "  and eax, {mode_mask}",
    "  cmp eax, {mode_directory}",
    "  mov edi, 22",
    "  jne 9f",

    // --- 23. 無いパス。-ENOENT が返るはず ---
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + MISSING_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  cmp rax, {minus_enoent}",
    "  mov edi, 23",
    "  jne 9f",
    "  add rsp, 192",

    // --- 24..27. ルートを getdents64 で読み、レコードを歩く ---
    // r12=fd、r13=バッファ先頭、r14=書かれたバイト数、r15=歩いた位置。
    "  sub rsp, 1024",
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + ROOT_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 1024",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 24",
    "  jle 9f",
    "  mov r14, rax",
    "  mov r13, rsp",
    "  xor r15, r15",          // 歩いた位置
    "  xor ebx, ebx",          // 数えたエントリ
    "  xor ebp, ebp",          // 見た d_type の論理和
    "20:",
    "  cmp r15, r14",
    "  jae 21f",
    // d_reclen が 8 の倍数か。
    "  movzx eax, word ptr [r13 + r15 + {reclen_off}]",
    "  test eax, 7",
    "  mov edi, 26",
    "  jne 9f",
    // 進まないレコードは無いはず（**歩きが止まる**）。
    "  test eax, eax",
    "  mov edi, 26",
    "  je 9f",
    // d_type を集める。
    "  movzx ecx, byte ptr [r13 + r15 + {type_off}]",
    "  or ebp, ecx",
    "  inc ebx",
    "  add r15, rax",
    "  jmp 20b",
    "21:",
    "  cmp ebx, {root_entries}",
    "  mov edi, 25",
    "  jne 9f",
    // **ルートは全部ディレクトリである**（`. .. lost+found bin data etc`）。
    "  cmp ebp, {dt_dir}",
    "  mov edi, 27",
    "  jne 9f",

    // --- 27 の本命。**/data は `. ..` と通常ファイル 2 本なので、
    // `d_type` が DT_DIR と DT_REG の両方になる。** ルートだけでは分かれない。
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + DATA_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 1024",
    "  int 0x80",
    "  mov r14, rax",
    "  mov r13, rsp",
    "  xor r15, r15",
    "  xor ebp, ebp",
    "22:",
    "  cmp r15, r14",
    "  jae 23f",
    "  movzx eax, word ptr [r13 + r15 + {reclen_off}]",
    "  test eax, eax",
    "  mov edi, 26",
    "  je 9f",
    "  movzx ecx, byte ptr [r13 + r15 + {type_off}]",
    "  or ebp, ecx",
    "  add r15, rax",
    "  jmp 22b",
    "23:",
    "  cmp ebp, {dt_both}",
    "  mov edi, 27",
    "  jne 9f",

    // --- 28. 末尾での getdents64。**0 が返るはず** ---
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 1024",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 28",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 29. 1 レコードも収まらないバッファ。**-EINVAL が返るはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + ROOT_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {tiny}",
    "  int 0x80",
    "  cmp rax, {minus_einval}",
    "  mov edi, 29",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    "  add rsp, 1024",

    // --- 36. spawn("/bin/hello")。**イメージをファイルシステムから読んで走り、0 で終わるはず** ---
    // **入れ子の遠征が本物になる場所である**（S11-2 の検証用 syscall はこれで外した）。
    // **子はシステムコールを発行する**ので、BKL を保持したまま降りていれば
    // 子の最初の `write` で再帰取得として止まる。
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
    "  lea rsi, [rip + ARGV_HELLO]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 36",
    "  jne 9f",

    // --- 79. spawn("/bin/pie-hello")。**位置独立の像が、ファイルシステムを通る道で載り、auxv を自分で確かめて
    //     0 で終わるはず**（2026-10-06）。 ---
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + PIE_HELLO_PATH]",
    "  lea rsi, [rip + ARGV_PIE_HELLO]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 79",
    "  jne 9f",

    // --- 37. spawn("/nope")。**-ENOENT が返るはず** ---
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + MISSING_PATH]",
    "  lea rsi, [rip + ARGV_HELLO]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_enoent}",
    "  mov edi, 37",
    "  jne 9f",

    // --- 38. spawn("/etc")。**-EISDIR が返るはず** ---
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + ETC_PATH]",
    "  lea rsi, [rip + ARGV_HELLO]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_eisdir}",
    "  mov edi, 38",
    "  jne 9f",

    // --- 39. spawn(NULL)。**-EFAULT が返るはず** ---
    // **パスをコピーする経路は `open` と同じ**（`copy_user_path`）。**同じウィンドウが効く。**
    "  mov eax, {sys_spawn}",
    "  xor edi, edi",
    "  lea rsi, [rip + ARGV_HELLO]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 39",
    "  jne 9f",

    // --- 40. spawn("/bin/spawn-test")。**孫が断られて 0 で終わるはず** ---
    // **深さの上限が効いていることを、子の側から見ている。**
    // `spawn-test` は自分の `spawn` が `-EAGAIN` で断られたときだけ 0 で終わる。
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + SPAWN_TEST_PATH]",
    "  lea rsi, [rip + ARGV_SPAWN_TEST]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 40",
    "  jne 9f",

    // --- 41. argv が NULL。**-EFAULT が返るはず** ---
    // **配列を要求している。** 「引数が無い」は空の配列（先頭が NULL）で表す。
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
    "  xor esi, esi",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 41",
    "  jne 9f",

    // --- 68. envp が NULL。**-EFAULT が返るはず**（f-2。`ADR-0053` の Decision 2）---
    // **`argv` と同じ規則である。** 「環境が無い」は空の配列（先頭が NULL）で表す。
    // **新しい規則を作らない**ので、判定も `argv` の隣に設ける。
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
    "  lea rsi, [rip + ARGV_HELLO]",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 68",
    "  jne 9f",

    // --- 42. 要素数が上限を越える argv。**-E2BIG が返るはず** ---
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
    "  lea rsi, [rip + ARGV_TOO_MANY]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_e2big}",
    "  mov edi, 42",
    "  jne 9f",

    // --- 43. 全体が長すぎる argv。**-E2BIG が返るはず** ---
    // **要素数は上限内である**（8 本）。**落ちるのは総バイト数のほうである。**
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
    "  lea rsi, [rip + ARGV_TOO_BIG]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_e2big}",
    "  mov edi, 43",
    "  jne 9f",

    // --- 48. spawn("/bin/ls")。**ルートを並べて 0 で終わるはず** ---
    // **出力はシリアルへ出る。** 中身の突き合わせは起動ログの参照が行う。
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + LS_PATH]",
    "  lea rsi, [rip + ARGV_LS]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 48",
    "  jne 9f",

    // --- 49. spawn("/bin/cat", ["cat", "/etc/motd"]) ---
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + CAT_PATH]",
    "  lea rsi, [rip + ARGV_CAT]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 49",
    "  jne 9f",

    // --- 50. spawn("/bin/cat", ["cat", "/nope"])。**開けないので 1 で終わるはず** ---
    // **見ているのは「子の 0 以外の終了状態が親へ届くこと」である。** **(b3) までは引数なしの
    // 2 を見ていたが、`cat` が標準入力を読むようになった**（`ADR-0063` の (b3)）**ので、
    // 引数なしは打鍵を待つ形になる。** **無いファイルなら待たずに 1 で終わる。**
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + CAT_PATH]",
    "  lea rsi, [rip + ARGV_CAT_MISSING]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, 1",
    "  mov edi, 50",
    "  jne 9f",

    // --- 51. read(0)。**打鍵が無いので -EAGAIN が返るはず** ---
    // **0 は端末である**（S11-10）。**待たない**——待つにはユーザープロセスの
    // スケジューラが要る。**`0` を返さない**のは、Linux では末尾の意味だからである。
    "  sub rsp, 64",
    "  mov eax, {sys_read}",
    "  xor edi, edi",
    "  mov rsi, rsp",
    "  mov edx, 16",
    "  int 0x80",
    "  cmp rax, {minus_eagain}",
    "  mov edi, 51",
    "  jne 9f",

    // --- 52. read(1)。**書く側なので -EAGAIN ではなく、端末として読める** ---
    // **1 も端末である。** Linux でも同じ端末を指すので、読めるほうが正しい。
    "  mov eax, {sys_read}",
    "  mov edi, 1",
    "  mov rsi, rsp",
    "  mov edx, 16",
    "  int 0x80",
    "  cmp rax, {minus_eagain}",
    "  mov edi, 52",
    "  jne 9f",

    // --- 53. read(3)。**開いていないので -EBADF のはず** ---
    "  mov eax, {sys_read}",
    "  mov edi, 3",
    "  mov rsi, rsp",
    "  mov edx, 16",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 53",
    "  jne 9f",
    "  add rsp, 64",

    // --- 44. write(0)。**書けるはず** ---
    // **0 も 1 も 2 も同じ端末である**（S11-10。`FileTable::new`）。
    // **番号ではなく中身で決まる**ので、どれへ書いても同じ先へ届く。
    // **Linux でも同じである**——`init` が端末を読み書き両方で開き、複製する。
    "  mov eax, {sys_write}",
    "  xor edi, edi",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  cmp rax, {msg_len}",
    "  mov edi, 44",
    "  jne 9f",

    // --- 45. write(3)。**-EBADF が返るはず** ---
    // **開いていない番号である。** `open` が 0 番から返すのとは別の話で、
    // **`write` は表を引かない**（ファイルへ書く道がまだ無い）。
    "  mov eax, {sys_write}",
    "  mov edi, 3",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 45",
    "  jne 9f",

    // --- 46. write(2)。**標準エラー出力も受ける** ---
    "  mov eax, {sys_write}",
    "  mov edi, 2",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  cmp rax, {msg_len}",
    "  mov edi, 46",
    "  jne 9f",

    // --- 47. 64 バイトを越える write ---
    // **記録用の緩衝は 64 バイトだが、それは `write` の上限ではない。**
    // **ページ単位に検証しては出す**ので、長さぶんの緩衝はカーネル側に要らない。
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + LONG_MESSAGE]",
    "  mov edx, {long_len}",
    "  int 0x80",
    "  cmp rax, {long_len}",
    "  mov edi, 47",
    "  jne 9f",
    // **最後にもう一度 MESSAGE を送る。** カーネル側の判定行が突き合わせるのは
    // **最後の `write`** なので、**長い行で上書きしたままにしない。**
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",

    // --- 54-60. ファイルへ書く（zi-c。ADR-0037） ---
    // 54: /data/writable を O_WRONLY|O_TRUNC で開く。fd は最小の空き（3）。
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + WRITABLE_PATH]",
    "  mov esi, {o_wronly_trunc}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, 3",
    "  mov edi, 54",
    "  jne 9f",
    // 55: 書きで開いた fd への read は -EBADF（向きの取り違えの片側）。
    "  mov eax, {sys_read}",
    "  mov edi, 3",
    "  mov rsi, rsp",
    "  mov edx, 8",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 55",
    "  jne 9f",
    // 56: 書く。返るのは渡した長さである。
    "  mov eax, {sys_write}",
    "  mov edi, 3",
    "  lea rsi, [rip + NEW_BODY]",
    "  mov edx, {new_body_len}",
    "  int 0x80",
    "  cmp rax, {new_body_len}",
    "  mov edi, 56",
    "  jne 9f",
    // 57: 閉じる。
    "  mov eax, {sys_close}",
    "  mov edi, 3",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 57",
    "  jne 9f",
    // 58: 開き直して読み戻す。**長さ・中身・EOF の 3 つで見る。**
    // 長さの一致だけだと「後ろに古い中身が残る」形（切り詰めの欠け）を
    // 素通しする——直後の read が 0（EOF）であることまでが 58 の主張である。
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + WRITABLE_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {new_body_len}",
    "  int 0x80",
    "  cmp rax, {new_body_len}",
    "  mov edi, 58",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + NEW_BODY]",
    "  mov ecx, {new_body_len}",
    "  repe cmpsb",
    "  mov edi, 58",
    "  jne 9f",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {new_body_len}",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 58",
    "  jne 9f",
    // 59: 読みで開いた fd への write は -EBADF（対称のもう片側）。
    "  mov eax, {sys_write}",
    "  mov rdi, r12",
    "  lea rsi, [rip + NEW_BODY]",
    "  mov edx, {new_body_len}",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 59",
    "  jne 9f",
    // 61: ファイルの fd への ioctl は -ENOTTY（e-1）。**端末ではない。**
    // **`sys_write` と同じく、番号ではなく表を引いて分けている。**
    "  mov eax, {sys_ioctl}",
    "  mov rdi, r12",
    "  mov esi, {tiocgwinsz}",
    "  mov rdx, rsp",
    "  int 0x80",
    "  cmp rax, {minus_enotty}",
    "  mov edi, 61",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    // 60: カナリア。**他のファイルへ書いていない**——/etc/motd の先頭 5 バイトが
    // 変わっていないこと（wrong-inode の種類の否定側）。
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 5",
    "  int 0x80",
    "  cmp rax, 5",
    "  mov edi, 60",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_BYTES]",
    "  mov ecx, 5",
    "  repe cmpsb",
    "  mov edi, 60",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    // 62: 端末への ioctl(TIOCGWINSZ) は 0 を返す（e-1）。
    "  mov eax, {sys_ioctl}",
    "  xor edi, edi",
    "  mov esi, {tiocgwinsz}",
    "  mov rdx, rsp",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 62",
    "  jne 9f",
    // 63: **起動シーケンスには前景のコンソールが無い**ので、行は 0 である
    //     （`sys_ioctl` の doc。「0 は分からない」であって「端末でない」ではない）。
    "  movzx eax, word ptr [rsp]",
    "  test eax, eax",
    "  mov edi, 63",
    "  jne 9f",
    // 64: 知らない要求は -ENOTTY（e-1）。**入口はここで閉じている。**
    "  mov eax, {sys_ioctl}",
    "  xor edi, edi",
    "  mov esi, {unknown_ioctl}",
    "  mov rdx, rsp",
    "  int 0x80",
    "  cmp rax, {minus_enotty}",
    "  mov edi, 64",
    "  jne 9f",

    // 65: socket(AF_UNIX, SOCK_STREAM, 0) は最小の空き番号（3。端末の 3 つの次）を返し、
    //     閉じられる（ADR-0064）。**ここまでに開いた fd は全部閉じてある。**
    "  mov eax, {sys_socket}",
    "  mov edi, {af_unix}",
    "  mov esi, {sock_stream}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, 3",
    "  mov edi, 65",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov edi, 3",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 65",
    "  jne 9f",
    // 66: 待ち受けの無い名前への connect は -ECONNREFUSED（ADR-0064）。**起動シーケンスに
    //     サーバーは居ない。**
    "  mov eax, {sys_socket}",
    "  mov edi, {af_unix}",
    "  mov esi, {sock_stream}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_connect}",
    "  mov rdi, r12",
    "  lea rsi, [rip + SOCKADDR_NOBODY]",
    "  mov edx, {sockaddr_nobody_len}",
    "  int 0x80",
    "  cmp rax, {minus_econnrefused}",
    "  mov edi, 66",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    // 67: memfd_create+ftruncate+mmap した共有メモリへ書いた値が読み戻せる（ADR-0065）。
    "  mov eax, {sys_memfd}",
    "  xor edi, edi",
    "  xor esi, esi",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",              // shm の fd
    "  mov eax, {sys_ftruncate}",
    "  mov rdi, r12",
    "  mov esi, {shm_len}",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 67",
    "  jne 9f",
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",              // addr=NULL（カーネルが決める）
    "  mov esi, {shm_len}",
    "  mov edx, {prot_rw}",
    "  mov r10d, {map_shared}",
    "  mov r8, r12",               // fd
    "  xor r9, r9",                // offset=0
    "  int 0x80",
    "  cmp rax, 0",                // マップしたアドレスは正（負なら errno）
    "  mov edi, 67",
    "  jl 9f",
    "  mov r13, rax",              // マップしたアドレス
    "  mov dword ptr [r13], {shm_pattern}",
    "  mov eax, dword ptr [r13]",
    "  cmp eax, {shm_pattern}",
    "  mov edi, 67",
    "  jne 9f",
    // 68: 共有メモリでない fd（stdin=0）の mmap は -EBADF（ADR-0065）。
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, {shm_len}",
    "  mov edx, {prot_rw}",
    "  mov r10d, {map_shared}",
    "  xor r8, r8",                // fd=0（stdin。共有メモリでない）
    "  xor r9, r9",
    "  int 0x80",
    "  cmp rax, {minus_ebadf_shm}",
    "  mov edi, 68",
    "  jne 9f",
    // 71: 実行できる保護を求めた mmap は -EPERM（2026-10-03）。**共有メモリは、書けるか読むだけかで写し、
    // 実行はさせない。** 同じ共有メモリの fd に、読みと実行（PROT_READ | PROT_EXEC）を求める。
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, {shm_len}",
    "  mov edx, {prot_rx}",
    "  mov r10d, {map_shared}",
    "  mov r8, r12",               // fd
    "  xor r9, r9",
    "  int 0x80",
    "  cmp rax, {minus_eperm}",
    "  mov edi, 71",
    "  jne 9f",
    "  mov eax, {sys_close}",      // shm の fd を閉じる
    "  mov rdi, r12",
    "  int 0x80",

    // --- 72〜78: `arch_prctl`（2026-10-05）。FS と GS の基底を入れる・訊く ---
    // スタックに 4 語取る。[rsp] が FS の基底の先、[rsp+8] が訊いた FS の基底の受け皿、[rsp+16] が GS の基底の先、
    // [rsp+24] が訊いた GS の基底の受け皿である。
    "  sub rsp, 32",
    "  mov rax, {tls_mark_fs}",
    "  mov qword ptr [rsp], rax",
    "  mov rax, {tls_mark_gs}",
    "  mov qword ptr [rsp + 16], rax",
    // 72: FS の基底を入れ、`fs:0` から読む。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_set_fs}",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov edi, 72",
    "  test rax, rax",
    "  jne 9f",
    "  mov rax, qword ptr fs:[0]",
    "  cmp rax, qword ptr [rsp]",
    "  jne 9f",
    // 73: FS の基底を訊く。入れた番地（rsp）が返る。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_get_fs}",
    "  lea rsi, [rsp + 8]",
    "  int 0x80",
    "  mov edi, 73",
    "  test rax, rax",
    "  jne 9f",
    "  cmp qword ptr [rsp + 8], rsp",
    "  jne 9f",
    // 74: GS で同じこと。**FS の基底は、そのまま残っている**ことも見る。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_set_gs}",
    "  lea rsi, [rsp + 16]",
    "  int 0x80",
    "  mov edi, 74",
    "  test rax, rax",
    "  jne 9f",
    "  mov rax, qword ptr gs:[0]",
    "  cmp rax, qword ptr [rsp + 16]",
    "  jne 9f",
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_get_gs}",
    "  lea rsi, [rsp + 24]",
    "  int 0x80",
    "  mov edi, 74",
    "  test rax, rax",
    "  jne 9f",
    "  lea rax, [rsp + 16]",
    "  cmp qword ptr [rsp + 24], rax",
    "  jne 9f",
    "  mov rax, qword ptr fs:[0]",
    "  cmp rax, qword ptr [rsp]",
    "  jne 9f",
    // 75: 正準でない番地は -EPERM。**基底は変わらない。**
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_set_fs}",
    "  mov rsi, 0x0000800000000000",
    "  int 0x80",
    "  mov edi, 75",
    "  cmp rax, {minus_eperm}",
    "  jne 9f",
    "  mov rax, qword ptr fs:[0]",
    "  cmp rax, qword ptr [rsp]",
    "  jne 9f",
    // 76: カーネルの番地は -EPERM。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_set_fs}",
    "  mov rsi, 0xffffffff80100000",
    "  int 0x80",
    "  mov edi, 76",
    "  cmp rax, {minus_eperm}",
    "  jne 9f",
    // 77: 知らない code は -EINVAL。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, 0x1fff",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov edi, 77",
    "  cmp rax, {minus_einval}",
    "  jne 9f",
    // 78: 書けない番地（カーネルの番地）へ訊いた結果を書かせると -EFAULT。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_get_fs}",
    "  mov rsi, 0xffffffff80100000",
    "  int 0x80",
    "  mov edi, 78",
    "  cmp rax, {minus_efault}",
    "  jne 9f",
    // 基底を 0 へ戻して、スタックを戻す。
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_set_fs}",
    "  xor esi, esi",
    "  int 0x80",
    "  mov eax, {sys_arch_prctl}",
    "  mov edi, {arch_set_gs}",
    "  xor esi, esi",
    "  int 0x80",
    "  add rsp, 32",

    // 69: 方向フラグを立てたまま `int 0x80` を打つ（2026-09-24）。**入口が DF を降ろすことの
    // 前提を作る**——カーネルはこの入場を数え、数えられなければ止まる
    // （`kernel/src/main.rs` の `check_direction_flag_premise`）。**書き戻しのある呼び出しを
    // 選んだ**——**降ろさなければ、カーネルのコピーの向きをこちらが決めることになる。**
    "  sub rsp, 16",
    "  std",
    "  mov eax, {sys_clock_gettime}",
    "  mov edi, {clock_monotonic}",
    "  mov rsi, rsp",
    "  int 0x80",
    "  cld",
    "  add rsp, 16",
    "  test rax, rax",
    "  mov edi, 69",
    "  jne 9f",

    // --- 80〜94: 起動に要る小物（2026-10-06。`ADR-0081`）。スタックに 128 バイト取る。 ---
    //
    // **受け皿の後ろの節に置く**（`.userland.after`。`user.ld`）。受け皿の位置は `0x401000` に固定してあり、`.text` が
    // そこを越えるとリンカが止める。ここまでの検算で `.text` はほぼ一杯なので、ここからは受け皿の後ろに続ける。
    // 節をまたぐのは明示の `jmp` だけで、落ちて入ることは無い。
    "  jmp 20f",
    ".section .userland.after,\"ax\"",
    "20:",
    // [rsp] 作業用の語 / [rsp+8..40] 登録の読み戻し（32 バイト）/ [rsp+40..64] 代替スタックの読み戻し（24 バイト）
    // / [rsp+64..80] 上限の読み戻し（16 バイト）/ [rsp+80..96] 乱数（16 バイト）/ [rsp+96..128] 登録（32 バイト）
    "  sub rsp, 128",
    // 80: set_tid_address。
    "  mov eax, {sys_set_tid_address}",
    "  mov rdi, rsp",
    "  int 0x80",
    "  mov edi, 80",
    "  cmp rax, 1",
    "  jne 8f",
    // 81: rt_sigaction(SIGPIPE, SIG_IGN) を登録し、読み戻す。
    "  mov qword ptr [rsp + 96], 1",   // sa_handler = SIG_IGN
    "  mov qword ptr [rsp + 104], 0",  // sa_flags
    "  mov qword ptr [rsp + 112], 0",  // sa_restorer
    "  mov qword ptr [rsp + 120], 0",  // sa_mask
    "  mov eax, {sys_rt_sigaction}",
    "  mov edi, 13",
    "  lea rsi, [rsp + 96]",
    "  xor edx, edx",
    "  mov r10d, 8",
    "  int 0x80",
    "  mov edi, 81",
    "  test rax, rax",
    "  jne 8f",
    "  mov eax, {sys_rt_sigaction}",
    "  mov edi, 13",
    "  xor esi, esi",
    "  lea rdx, [rsp + 8]",
    "  mov r10d, 8",
    "  int 0x80",
    "  mov edi, 81",
    "  test rax, rax",
    "  jne 8f",
    "  cmp qword ptr [rsp + 8], 1",
    "  jne 8f",
    // 82: SIGKILL は替えられない。
    "  mov eax, {sys_rt_sigaction}",
    "  mov edi, 9",
    "  lea rsi, [rsp + 96]",
    "  xor edx, edx",
    "  mov r10d, 8",
    "  int 0x80",
    "  mov edi, 82",
    "  cmp rax, {minus_einval}",
    "  jne 8f",
    // 94: SA_RESTORER の無いハンドラは断られる。
    "  mov qword ptr [rsp + 96], 0x401000", // sa_handler = 番地（SIG_DFL でも SIG_IGN でもない）
    "  mov eax, {sys_rt_sigaction}",
    "  mov edi, 10",
    "  lea rsi, [rsp + 96]",
    "  xor edx, edx",
    "  mov r10d, 8",
    "  int 0x80",
    "  mov edi, 94",
    "  cmp rax, {minus_einval}",
    "  jne 8f",
    // 83: rt_sigprocmask(SIG_BLOCK, {SIGUSR1}) then query.
    "  mov qword ptr [rsp], 0x200",   // 1 << (10 - 1)
    "  mov eax, {sys_rt_sigprocmask}",
    "  xor edi, edi",               // SIG_BLOCK
    "  mov rsi, rsp",
    "  xor edx, edx",
    "  mov r10d, 8",
    "  int 0x80",
    "  mov edi, 83",
    "  test rax, rax",
    "  jne 8f",
    "  mov qword ptr [rsp + 8], 0",
    "  mov eax, {sys_rt_sigprocmask}",
    "  xor edi, edi",
    "  xor esi, esi",
    "  lea rdx, [rsp + 8]",
    "  mov r10d, 8",
    "  int 0x80",
    "  mov edi, 83",
    "  test rax, rax",
    "  jne 8f",
    "  cmp qword ptr [rsp + 8], 0x200",
    "  jne 8f",
    // 84: sigaltstack を据えて、問い合わせる。
    "  mov qword ptr [rsp + 40], 0x700000", // ss_sp
    "  mov qword ptr [rsp + 48], 0",        // ss_flags（と詰め物）
    "  mov qword ptr [rsp + 56], 8192",     // ss_size
    "  mov eax, {sys_sigaltstack}",
    "  lea rdi, [rsp + 40]",
    "  xor esi, esi",
    "  int 0x80",
    "  mov edi, 84",
    "  test rax, rax",
    "  jne 8f",
    "  mov qword ptr [rsp + 40], 0",
    "  mov qword ptr [rsp + 56], 0",
    "  mov eax, {sys_sigaltstack}",
    "  xor edi, edi",
    "  lea rsi, [rsp + 40]",
    "  int 0x80",
    "  mov edi, 84",
    "  test rax, rax",
    "  jne 8f",
    "  cmp qword ptr [rsp + 40], 0x700000",
    "  jne 8f",
    "  cmp qword ptr [rsp + 56], 8192",
    "  jne 8f",
    // 85: prlimit64(0, RLIMIT_STACK, NULL, old)。
    "  mov eax, {sys_prlimit64}",
    "  xor edi, edi",
    "  mov esi, 3",
    "  xor edx, edx",
    "  lea r10, [rsp + 64]",
    "  int 0x80",
    "  mov edi, 85",
    "  test rax, rax",
    "  jne 8f",
    "  cmp qword ptr [rsp + 64], 0x800000",
    "  jne 8f",
    // 86: getrandom(buf, 16, 0)。
    "  mov qword ptr [rsp + 80], 0",
    "  mov qword ptr [rsp + 88], 0",
    "  mov eax, {sys_getrandom}",
    "  lea rdi, [rsp + 80]",
    "  mov esi, 16",
    "  xor edx, edx",
    "  int 0x80",
    "  mov edi, 86",
    "  cmp rax, 16",
    "  jne 8f",
    "  mov rax, qword ptr [rsp + 80]",
    "  or rax, qword ptr [rsp + 88]",
    "  jz 8f",
    // 87: futex(FUTEX_WAKE|PRIVATE) は 0。
    "  mov dword ptr [rsp], 1",
    "  mov eax, {sys_futex}",
    "  mov rdi, rsp",
    "  mov esi, 129",   // FUTEX_WAKE | FUTEX_PRIVATE_FLAG
    "  mov edx, 1",
    "  int 0x80",
    "  mov edi, 87",
    "  test rax, rax",
    "  jne 8f",
    // 88: 値の違う futex(FUTEX_WAIT|PRIVATE) は -EAGAIN。
    "  mov eax, {sys_futex}",
    "  mov rdi, rsp",
    "  mov esi, 128",   // FUTEX_WAIT | FUTEX_PRIVATE_FLAG
    "  mov edx, 2",     // [rsp] は 1
    "  xor r10d, r10d",
    "  int 0x80",
    "  mov edi, 88",
    "  cmp rax, {minus_eagain}",
    "  jne 8f",
    // 89: uname。390 バイトの受け皿はスタックの下に取る。
    "  sub rsp, 400",
    "  mov eax, {sys_uname}",
    "  mov rdi, rsp",
    "  int 0x80",
    "  mov edi, 89",
    "  test rax, rax",
    "  jne 7f",
    "  cmp dword ptr [rsp], 0x756e694c",  // "Linu"
    "  jne 7f",
    "  cmp byte ptr [rsp + 4], 0x78",     // "x"
    "  jne 7f",
    // 90: readlink("/proc/self/exe") → "syscall-test"（12 バイト）。
    "  mov eax, {sys_readlink}",
    "  lea rdi, [rip + PROC_SELF_EXE]",
    "  mov rsi, rsp",
    "  mov edx, 64",
    "  int 0x80",
    "  mov edi, 90",
    "  cmp rax, 12",
    "  jne 7f",
    "  cmp dword ptr [rsp], 0x63737973",  // "sysc"
    "  jne 7f",
    // 91: fstat(open("/etc/motd")) の大きさが stat と同じ。
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  int 0x80",
    "  mov edi, 91",
    "  test rax, rax",
    "  js 7f",
    "  mov r12, rax",
    "  mov eax, {sys_fstat}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov edi, 91",
    "  test rax, rax",
    "  jne 7f",
    "  mov r13, qword ptr [rsp + 48]",    // st_size
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov edi, 91",
    "  test rax, rax",
    "  jne 7f",
    "  cmp r13, qword ptr [rsp + 48]",
    "  jne 7f",
    // 92: fcntl(fd, F_GETFD) は 0、F_DUPFD_CLOEXEC は 10 以上の番号。
    "  mov eax, {sys_fcntl}",
    "  mov rdi, r12",
    "  mov esi, 1",       // F_GETFD
    "  int 0x80",
    "  mov edi, 92",
    "  test rax, rax",
    "  jne 7f",
    "  mov eax, {sys_fcntl}",
    "  mov rdi, r12",
    "  mov esi, 1030",    // F_DUPFD_CLOEXEC
    "  mov edx, 10",
    "  int 0x80",
    "  mov edi, 92",
    "  cmp rax, 10",
    "  jl 7f",
    "  mov r13, rax",
    "  mov eax, {sys_close}",
    "  mov rdi, r13",
    "  int 0x80",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    // 95: mmap(NULL, 2 ページ, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) は基点以上の番地を返す。
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, 8192",
    "  mov edx, 3",
    "  mov r10d, 0x22",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 95",
    "  mov r13, 0x10000000",
    "  cmp rax, r13",
    "  jb 7f",
    "  mov r12, rax",
    // 96: 0 で埋まっていて、書いた値が読み戻せる（2 ページ目の末尾の語）。
    "  mov edi, 96",
    "  cmp qword ptr [r12], 0",
    "  jne 7f",
    "  cmp qword ptr [r12 + 8184], 0",
    "  jne 7f",
    "  mov rax, 0x5a5a1234",
    "  mov qword ptr [r12 + 8184], rax",
    "  cmp qword ptr [r12 + 8184], rax",
    "  jne 7f",
    // 98: ページの境界に無い番地の munmap は -EINVAL。長さ 0 の mmap も -EINVAL。
    "  mov eax, {sys_munmap}",
    "  lea rdi, [r12 + 8]",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 98",
    "  cmp rax, {minus_einval}",
    "  jne 7f",
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  xor esi, esi",
    "  mov edx, 3",
    "  mov r10d, 0x22",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 98",
    "  cmp rax, {minus_einval}",
    "  jne 7f",
    // 97: 全体の munmap は 0 で、次の mmap は同じ番地を使い直す。
    "  mov eax, {sys_munmap}",
    "  mov rdi, r12",
    "  mov esi, 8192",
    "  int 0x80",
    "  mov edi, 97",
    "  test rax, rax",
    "  jne 7f",
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, 4096",
    "  mov edx, 3",
    "  mov r10d, 0x22",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 97",
    "  cmp rax, r12",
    "  jne 7f",
    // 使い直した 1 ページは 0 に戻っている（前の中身を渡さない）。
    "  mov edi, 96",
    "  cmp qword ptr [r12], 0",
    "  jne 7f",
    "  mov eax, {sys_munmap}",
    "  mov rdi, r12",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 97",
    "  test rax, rax",
    "  jne 7f",
    // 99: 写像の無い範囲の munmap は 0。
    "  mov eax, {sys_munmap}",
    "  mov rdi, r12",
    "  mov esi, 8192",
    "  int 0x80",
    "  mov edi, 99",
    "  test rax, rax",
    "  jne 7f",
    // 100: 3 ページの無名の写像の、真ん中の 1 ページを munmap する。両側は残る。
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, 12288",
    "  mov edx, 3",
    "  mov r10d, 0x22",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 100",
    "  cmp rax, r13",          // r13 = 0x10000000（95 で入れた）
    "  jb 7f",
    "  mov r12, rax",
    "  mov qword ptr [r12], 11",
    "  mov qword ptr [r12 + 8192], 22",
    "  mov eax, {sys_munmap}",
    "  lea rdi, [r12 + 4096]",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 100",
    "  test rax, rax",
    "  jne 7f",
    "  cmp qword ptr [r12], 11",
    "  jne 7f",
    "  cmp qword ptr [r12 + 8192], 22",
    "  jne 7f",
    // 101: 残った 2 つの断片を返す。
    "  mov eax, {sys_munmap}",
    "  mov rdi, r12",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 101",
    "  test rax, rax",
    "  jne 7f",
    "  mov eax, {sys_munmap}",
    "  lea rdi, [r12 + 8192]",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 101",
    "  test rax, rax",
    "  jne 7f",
    // 102: 2 ページの無名の写像の 2 ページ目へ、MAP_FIXED で置き換える。中身は 0 に戻る。
    "  mov eax, {sys_mmap}",
    "  xor edi, edi",
    "  mov esi, 8192",
    "  mov edx, 3",
    "  mov r10d, 0x22",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 102",
    "  cmp rax, r13",
    "  jb 7f",
    "  mov r12, rax",
    "  mov qword ptr [r12 + 4096], 33",
    "  mov eax, {sys_mmap}",
    "  lea rdi, [r12 + 4096]",
    "  mov esi, 4096",
    "  mov edx, 3",
    "  mov r10d, 0x32",         // MAP_PRIVATE|MAP_FIXED|MAP_ANONYMOUS
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 102",
    "  lea rcx, [r12 + 4096]",
    "  cmp rax, rcx",
    "  jne 7f",
    "  cmp qword ptr [r12 + 4096], 0",
    "  jne 7f",
    // 103: MAP_FIXED_NOREPLACE は、写像の上では -EEXIST、空いた所ではその番地。
    "  mov eax, {sys_mmap}",
    "  mov rdi, r12",
    "  mov esi, 4096",
    "  mov edx, 3",
    "  mov r10d, 0x100022",     // MAP_PRIVATE|MAP_FIXED_NOREPLACE|MAP_ANONYMOUS
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 103",
    "  cmp rax, {minus_eexist}",
    "  jne 7f",
    "  mov eax, {sys_mmap}",
    "  mov rdi, 0x10100000",
    "  mov esi, 4096",
    "  mov edx, 3",
    "  mov r10d, 0x100022",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 103",
    "  mov rcx, 0x10100000",
    "  cmp rax, rcx",
    "  jne 7f",
    "  mov eax, {sys_munmap}",
    "  mov rdi, 0x10100000",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 103",
    "  test rax, rax",
    "  jne 7f",
    "  mov eax, {sys_munmap}",
    "  mov rdi, r12",
    "  mov esi, 8192",
    "  int 0x80",
    "  mov edi, 103",
    "  test rax, rax",
    "  jne 7f",
    // 104: スタック（0x7ff000）と見張りのページ（0x7fb000）への MAP_FIXED は -EINVAL。
    "  mov eax, {sys_mmap}",
    "  mov edi, 0x7ff000",
    "  mov esi, 4096",
    "  mov edx, 3",
    "  mov r10d, 0x32",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 104",
    "  cmp rax, {minus_einval}",
    "  jne 7f",
    "  mov eax, {sys_mmap}",
    "  mov edi, 0x7fb000",
    "  mov esi, 4096",
    "  mov edx, 0",
    "  mov r10d, 0x32",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 104",
    "  cmp rax, {minus_einval}",
    "  jne 7f",
    // 105: brk の見張りの流れ（musl の malloc と同じ）。X = brk(0) をページへ切り上げ、2 ページ伸ばし、先頭へ PROT_NONE を
    //      MAP_FIXED で置き、2 ページ目へ書けること、brk を X へ戻せること、見張りを munmap できること、brk が元へ戻ること。
    "  mov eax, {brk}",
    "  xor edi, edi",
    "  int 0x80",
    "  mov edi, 105",
    "  test rax, rax",
    "  jle 7f",
    "  mov r14, rax",                 // 元の brk
    "  lea r12, [rax + 4095]",
    "  and r12, -4096",               // X
    "  mov eax, {brk}",
    "  lea rdi, [r12 + 8192]",
    "  int 0x80",
    "  mov edi, 105",
    "  lea rcx, [r12 + 8192]",
    "  cmp rax, rcx",
    "  jne 7f",
    "  mov eax, {sys_mmap}",
    "  mov rdi, r12",
    "  mov esi, 4096",
    "  xor edx, edx",                 // PROT_NONE
    "  mov r10d, 0x32",
    "  mov r8, -1",
    "  xor r9d, r9d",
    "  int 0x80",
    "  mov edi, 105",
    "  cmp rax, r12",
    "  jne 7f",
    "  mov qword ptr [r12 + 4096], 44",
    "  cmp qword ptr [r12 + 4096], 44",
    "  jne 7f",
    "  mov eax, {brk}",
    "  mov rdi, r12",
    "  int 0x80",
    "  mov edi, 105",
    "  cmp rax, r12",
    "  jne 7f",
    "  mov eax, {sys_munmap}",
    "  mov rdi, r12",
    "  mov esi, 4096",
    "  int 0x80",
    "  mov edi, 105",
    "  test rax, rax",
    "  jne 7f",
    "  mov eax, {brk}",
    "  mov rdi, r14",
    "  int 0x80",
    "  mov edi, 105",
    "  cmp rax, r14",
    "  jne 7f",
    // 93: spawn("/bin/futex-wait") は 137 を返す（子は、起こす者の居ない FUTEX_WAIT で終わらせられる）。
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + FUTEX_WAIT_PATH]",
    "  lea rsi, [rip + ARGV_FUTEX_WAIT]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  mov edi, 93",
    "  cmp rax, 137",
    "  jne 7f",
    "  add rsp, 400",
    "  add rsp, 128",

    // すべて通った。
    "  xor edi, edi",
    "  jmp 9f",
    // 80 番台の検算の失敗の出口（スタックを戻してから終わる）。
    "7:",
    "  add rsp, 400",
    "8:",
    "  add rsp, 128",
    "  jmp 9f",
    ".section .text._start,\"ax\"",

    // --- 4. exit(status)。ここから戻らない。**`exit_group` で終わる**（2026-10-06。Linux の libc が終わりに呼ぶ
    //     入口。受けられなければ `-ENOSYS` が返って受け皿へ落ちる） ---
    "9:",
    "  mov eax, {sys_exit_group}",
    "  int 0x80",
    // **`exit` が戻ってきたときの受け皿**（`hello.rs` と同じ規律）。
    // **位置はリンカが決める**（`user.ld` の `USER_RECEIVER_OFFSET`）ので、
    // ここに `.org` は要らない。**検算を足しても、この行は動かない。**
    ".section .userland.receiver,\"ax\"",
    "  ud2",

    ".section .rodata",
    "MESSAGE:",
    "  .ascii \"syscall-test wrote this\\n\"",
    "MOTD_PATH:",
    "  .asciz \"/etc/motd\"",
    "MISSING_PATH:",
    "  .asciz \"/nope\"",
    "WRITABLE_PATH:",
    "  .asciz \"/data/writable\"",
    // **23 バイト**（NEW_BODY_LEN と対）。イメージの初期の中身と違う列である。
    "NEW_BODY:",
    "  .ascii \"zi-c rewrote this file\\n\"",
    "ETC_PATH:",
    "  .asciz \"/etc\"",
    "ROOT_PATH:",
    "  .asciz \"/\"",
    "DATA_PATH:",
    "  .asciz \"/data\"",
    // **`spawn` が読むイメージのパス。** どちらも `kernel/build.rs` がイメージへ置いている。
    "HELLO_PATH:",
    "  .asciz \"/bin/hello\"",
    "PIE_HELLO_PATH:",
    "  .asciz \"/bin/pie-hello\"",
    "FUTEX_WAIT_PATH:",
    "  .asciz \"/bin/futex-wait\"",
    "SPAWN_ARG_FUTEX_WAIT:",
    "  .asciz \"futex-wait\"",
    "PROC_SELF_EXE:",
    "  .asciz \"/proc/self/exe\"",
    "SPAWN_ARG_PIE_HELLO:",
    "  .asciz \"pie-hello\"",
    "SPAWN_TEST_PATH:",
    "  .asciz \"/bin/spawn-test\"",
    // **`spawn` へ渡す `argv`。** 配列は 8 バイト境界へ揃える。
    ".balign 8",
    "ARGV_HELLO:",
    "  .quad SPAWN_ARG_HELLO",
    "  .quad 0",
    "ARGV_PIE_HELLO:",
    "  .quad SPAWN_ARG_PIE_HELLO",
    "  .quad 0",
    "ARGV_FUTEX_WAIT:",
    "  .quad SPAWN_ARG_FUTEX_WAIT",
    "  .quad 0",
    // **空の `envp`（f-2。`ADR-0053` の Decision 2）。**
    //
    // **`spawn` の第 3 引数に意味ができた。** **置かないと入口の RDX が
    // そのまま `envp` として読まれる**——**たまたま 0 でも置く。**
    // **「環境が無い」は空の配列で表す**（`argv` と同じ規則。NULL は `-EFAULT`）。
    ".balign 8",
    "ENVP_EMPTY:",
    "  .quad 0",
    // **`spawn-test` が受け取って検算する 2 本。** あちらの写しと対になっている。
    "ARGV_SPAWN_TEST:",
    "  .quad SPAWN_ARG_NAME",
    "  .quad SPAWN_ARG_BETA",
    "  .quad 0",
    // **上限（8 本）を 1 本越える。** 中身は同じで構わない——落ちるのは数である。
    "ARGV_TOO_MANY:",
    "  .rept 9",
    "  .quad SPAWN_ARG_HELLO",
    "  .endr",
    "  .quad 0",
    // **8 本で上限内だが、総バイト数が越える。** 1 本 200 バイトの 8 本である。
    "ARGV_TOO_BIG:",
    "  .rept 8",
    "  .quad SPAWN_ARG_LONG",
    "  .endr",
    "  .quad 0",
    "LS_PATH:",
    "  .asciz \"/bin/ls\"",
    "CAT_PATH:",
    "  .asciz \"/bin/cat\"",
    ".balign 8",
    "ARGV_LS:",
    "  .quad SPAWN_ARG_LS",
    "  .quad 0",
    "ARGV_CAT:",
    "  .quad SPAWN_ARG_CAT",
    "  .quad MOTD_PATH",
    "  .quad 0",
    "ARGV_CAT_MISSING:",
    "  .quad SPAWN_ARG_CAT",
    "  .quad CAT_MISSING_PATH",
    "  .quad 0",
    "CAT_MISSING_PATH:",
    "  .asciz \"/nope\"",
    // **`sockaddr_un`**（`sa_family_t` = AF_UNIX の 2 バイトと、NUL 終端の名前）。
    ".balign 2",
    "SOCKADDR_NOBODY:",
    "  .short 1",
    "  .asciz \"nobody\"",
    ".balign 8",
    "SPAWN_ARG_LS:",
    "  .asciz \"ls\"",
    "SPAWN_ARG_CAT:",
    "  .asciz \"cat\"",
    "SPAWN_ARG_HELLO:",
    "  .asciz \"hello\"",
    "SPAWN_ARG_NAME:",
    "  .asciz \"spawn-test\"",
    "SPAWN_ARG_BETA:",
    "  .asciz \"beta\"",
    "SPAWN_ARG_LONG:",
    "  .rept 199",
    "  .byte 0x41",
    "  .endr",
    "  .byte 0",
    // **カーネルが積む `argv` の写し。** 食い違えば 31 番か 32 番が落ちる。
    "ARGV0_TEXT:",
    "  .asciz \"syscall-test\"",
    "ARGV1_TEXT:",
    "  .asciz \"alpha\"",
    // **`/etc/motd` の中身の写し。** 種のファイルと食い違えば 12 番が落ちる。
    "MOTD_BYTES:",
    "  .ascii \"welco\"",
    "MOTD_REST:",
    "  .ascii \"me to ZeikOS\\n\"",
    // **64 バイトを越える 1 本。** 記録用の緩衝より長いことが主張である。
    "LONG_MESSAGE:",
    "  .ascii \"syscall-test is writing a line that does not fit the 64-byte record\\n\"",

    arg0 = const PROBE_ARG0,
    arg1 = const PROBE_ARG1,
    arg2 = const PROBE_ARG2,
    arg3 = const PROBE_ARG3,
    arg4 = const PROBE_ARG4,
    arg5 = const PROBE_ARG5,
    sentinel = const SENTINEL_RCX,
    probe = const PROBE_NUMBER,
    probe_ret = const PROBE_RETURN,
    sys_write = const SYS_WRITE,
    msg_len = const MESSAGE_LEN,
    never = const NEVER_IMPLEMENTED,
    minus_enosys = const MINUS_ENOSYS,
    sys_exit_group = const SYS_EXIT_GROUP,
    sys_set_tid_address = const 218u32,
    sys_rt_sigaction = const 13u32,
    sys_rt_sigprocmask = const 14u32,
    sys_sigaltstack = const 131u32,
    sys_prlimit64 = const 302u32,
    sys_getrandom = const 318u32,
    sys_futex = const 202u32,
    sys_uname = const 63u32,
    sys_readlink = const 89u32,
    sys_fstat = const 5u32,
    sys_fcntl = const 72u32,
    sys_munmap = const 11u32,
    minus_eexist = const MINUS_EEXIST,
    sys_open = const SYS_OPEN,
    sys_close = const SYS_CLOSE,
    o_rdonly = const O_RDONLY,
    o_wronly = const O_WRONLY,
    o_wronly_trunc = const O_WRONLY_TRUNC,
    new_body_len = const NEW_BODY_LEN,
    minus_enoent = const MINUS_ENOENT,
    minus_ebadf = const MINUS_EBADF,
    minus_enotty = const MINUS_ENOTTY,
    sys_ioctl = const SYS_IOCTL,
    tiocgwinsz = const TIOCGWINSZ,
    unknown_ioctl = const UNKNOWN_IOCTL,
    minus_erofs = const MINUS_EROFS,
    minus_efault = const MINUS_EFAULT,
    sys_read = const SYS_READ,
    minus_eisdir = const MINUS_EISDIR,
    motd_len = const MOTD_LEN,
    motd_head = const MOTD_HEAD,
    motd_tail = const MOTD_TAIL,
    over_read = const OVER_READ,
    sys_stat = const SYS_STAT,
    stat_mode_off = const STAT_MODE_OFFSET,
    stat_size_off = const STAT_SIZE_OFFSET,
    stat_blocks_off = const STAT_BLOCKS_OFFSET,
    mode_mask = const MODE_FORMAT_MASK,
    mode_regular = const MODE_REGULAR,
    mode_directory = const MODE_DIRECTORY,
    motd_blocks = const MOTD_BLOCKS,
    sys_getdents = const SYS_GETDENTS64,
    root_entries = const ROOT_ENTRIES,
    reclen_off = const DIRENT_RECLEN_OFFSET,
    type_off = const DIRENT_TYPE_OFFSET,
    dt_dir = const DT_DIR,
    dt_both = const DT_DIR | DT_REG,
    tiny = const TINY_BUFFER,
    minus_einval = const MINUS_EINVAL,
    argc = const EXPECTED_ARGC,
    argv0_len = const ARGV0_LEN,
    argv1_len = const ARGV1_LEN,
    envp_max = const ENVP_WALK_MAX,
    brk = const SYS_BRK,
    brk_growth = const BRK_GROWTH,
    brk_last = const BRK_LAST_WORD,
    brk_first_last = const BRK_FIRST_PAGE_LAST_WORD,
    brk_pattern = const BRK_PATTERN,
    brk_too_far = const BRK_TOO_FAR,
    minus_enomem = const MINUS_ENOMEM,
    sys_spawn = const SYS_SPAWN,
    minus_e2big = const MINUS_E2BIG,
    minus_eagain = const MINUS_EAGAIN,
    long_len = const LONG_MESSAGE_LEN,
    sys_memfd = const SYS_MEMFD_CREATE,
    sys_ftruncate = const SYS_FTRUNCATE,
    sys_mmap = const SYS_MMAP,
    shm_len = const SHM_TEST_LEN,
    shm_pattern = const SHM_TEST_PATTERN,
    prot_rw = const PROT_RW,
    prot_rx = const PROT_RX,
    minus_eperm = const MINUS_EPERM,
    sys_arch_prctl = const 158u32,
    arch_set_gs = const 0x1001u32,
    arch_set_fs = const 0x1002u32,
    arch_get_fs = const 0x1003u32,
    arch_get_gs = const 0x1004u32,
    tls_mark_fs = const 0x715a_11ce_0000_f5f5u64,
    tls_mark_gs = const 0x715a_11ce_0000_6565u64,
    map_shared = const MAP_SHARED,
    minus_ebadf_shm = const MINUS_EBADF_SHM,
    sys_socket = const SYS_SOCKET,
    sys_connect = const SYS_CONNECT,
    sys_clock_gettime = const SYS_CLOCK_GETTIME,
    clock_monotonic = const CLOCK_MONOTONIC,
    af_unix = const 1,
    sock_stream = const 1,
    sockaddr_nobody_len = const SOCKADDR_NOBODY_LEN,
    minus_econnrefused = const MINUS_ECONNREFUSED,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
