# 0081. Linuxのプログラムが起動に要る小さなシステムコールと、シグナルの登録を受ける

## Status

Accepted（2026-10-06に運用者が設計案を承認し、同じ日に実装した）。

## Date

2026-10-06

## Context

- 目指す先は、muslで静的リンクしたLinuxのプログラムをそのまま動かすことである（`ADR-0074`）。2026-10-04の`strace`の実測（Rust・musl・静的PIEの`zeyes-min`）では、起動の最初に`arch_prctl`・`set_tid_address`・`rt_sigaction`（5回）・`sigaltstack`・`rt_sigprocmask`・`brk`・`mmap`・`mprotect`が並び、終わりは`exit_group`だった。glibcの側は`prlimit64`・`uname`・`readlink`も読む。
- このカーネルには、`arch_prctl`と`brk`以外が無かった。知らない番号は`-ENOSYS`で、libcは起動の途中で止まる。
- スレッドは1本しか無く、シグナルは1つも起きない（ユーザーの例外はプロセスの終了にする）。配送の仕組みを作る前に、登録だけを受けて起動を通す必要が在る。
- `mmap`・`mprotect`・`munmap`は別の課題である（この決定の範囲の外）。

## Decision

### 1. 小さなシステムコールを足す。振る舞いは、スレッドが1本・シグナルの配送が無い前提での、Linuxの形

| 呼び出し | 受ける形 |
|---|---|
| `set_tid_address` | 番地を控えて、唯一のスレッドの番号（1）を返す。スレッドの終わりに0を書く仕組みは要らない |
| `rt_sigaction` | 下の2 |
| `rt_sigprocmask` | マスクを控え、前のマスクを返す。`SIGKILL`・`SIGSTOP`は塞げない。`sigsetsize`は8だけ |
| `sigaltstack` | 控えて、前の値を返す。`SS_DISABLE`、`MINSIGSTKSZ`未満（`-ENOMEM`）、知らないフラグ（`-EINVAL`）はLinuxと同じ |
| `exit_group` | `exit`と同じ分岐。スレッドが無い間は区別が無い |
| `futex` | `FUTEX_WAKE`は0。`FUTEX_WAIT`は値が違えば`-EAGAIN`。**値が同じ（待つ場面）なら、下の3** |
| `prlimit64` | 問い合わせだけ。`RLIMIT_STACK`は8MiB・無限、`RLIMIT_NOFILE`はfdの表の大きさ。変える求めは`-EPERM`、ほかの資源は`-EINVAL`、自分以外の`pid`は`-ESRCH` |
| `getrandom` | `AT_RANDOM`と同じ出所（`ADR-0080`）。暗号に使えない。1回に256バイトまで |
| `uname` | 下の4 |
| `readlink` | `/proc/self/exe`だけ、載せたときの名前を返す。ext2にシンボリックリンクは無いので、ほかは`-EINVAL`か`-ENOENT` |
| `fstat` | `stat`と同じ欄をfdから。開いたファイルはinodeの値、共有メモリは普通のファイル、ソケットとパイプはそれぞれの型、端末・入力・画面は文字装置 |
| `fcntl` | `F_GETFD`/`F_SETFD`/`F_GETFL`/`F_SETFL`は受けて0（`FD_CLOEXEC`も`O_NONBLOCK`も持たない——`exec`は無く、ソケットの読みはもとから待たない）。`F_DUPFD`/`F_DUPFD_CLOEXEC`は`arg`以上の最小の空き番号へ写す。`F_ADD_SEALS`/`F_GET_SEALS`は共有メモリにだけ、受けて0 |
| `sendto` | `addr`が無ければ`write`と同じ。在れば`-EISCONN`。`flags`は見ない |

### 2. シグナルは、登録と問い合わせだけを受ける。配送はしない

- プロセスごとに64項の表（`sa_handler`・`sa_flags`・`sa_restorer`・`sa_mask`。カーネルが受ける`struct sigaction`の並び）を持ち、`rt_sigaction`で替えて、前の登録を返す。
- `SIGKILL`・`SIGSTOP`は替えられない（問い合わせはできる）。ハンドラ（`SIG_DFL`・`SIG_IGN`以外）の登録は、`SA_RESTORER`が無ければ`-EINVAL`——カーネルはハンドラから戻る道を自分では持たない（musl・glibcは必ず付ける）。
- **配送はしない。** どのシグナルも、まだ起きない。Rustの`std`が`SIGSEGV`・`SIGBUS`を代替スタックつきで登録するが、ユーザーのページフォルトは今までどおりプロセスの終了にする。`SIGPIPE`を`SIG_IGN`にする求めは控えるだけでよい——閉じた相手への`write`は、配送が無いので`-EPIPE`をそのまま返す。
- **表はプロセスの記録（`UserProcess`）に足さず、スロットと遠征の深さごとの静的な置き場に置く**（`kernel/src/process_state.rs`）。記録は載せる側の遠征スタックの上に在り、2KiBを足すと、子が走っている間ずっと残る枠が太る（`ADR-0079`）。`set_tid_address`の番地と、`readlink`が返す実行ファイルの名前も、同じ置き場に置く。

### 3. `FUTEX_WAIT`で待つ場面は、偽りの戻り値を返さず、名指ししてプロセスを終わらせる

- 値が同じなら、Linuxは誰かが起こすまで眠る。このカーネルには起こすスレッドが居ないので、眠れば永久に戻らない。
- **0や`-EINTR`を返して先へ進ませない**——行き詰まりを隠すと、ロックが壊れたまま走る。番地を控え、終了状態137（`128 + SIGKILL`。シェルが「シグナルで終わった」と見せる値）でプロセスを終わらせ、載せる側が`futex:`の行で名指しする。
- スレッド（M3a）が入ったら、本当に待つ形にする（`docs/deferred-decisions.md`）。
- **終わらせる呼び出しは、番号ではなく終了の印で見る。** `exit`だけを番号で見ていた入口の分岐を、「終了の印が立っていれば返らない」に変えた。`exit`・`exit_group`・`futex`の待つ場面が、同じ道を通る。

### 4. `uname`は、`sysname`を`Linux`、`release`を`6.1.0-zeikos`と答える

- Linuxと完全互換の方針（`ADR-0074`）である。libcは`sysname`と`release`を見て振る舞いを選ぶ——glibcは`release`の数字でカーネルの版を確かめ（`6.1`はglibc 2.3x以降が求める下限より上）、musl・Rustの`std`も`uname`の値を表示や判断に使う。
- `version`に`#1 ZeikOS`、`nodename`に`zeikos`、`machine`に`x86_64`、`domainname`に`(none)`を置く。この OS の名前は`version`で分かる。
- 欄は65バイト×6で、1欄ずつ書く（390バイトの控えを遠征スタックに置かない）。

## Alternatives Considered

1. **`FUTEX_WAIT`で0を返す**: 採らない。待つ場面を黙って通すと、ロックを取ったつもりで走る。
2. **`FUTEX_WAIT`で`-ENOSYS`を返す**: 採らない。musl・glibcは`futex`が無いことを想定せず、戻り値を見ずに進むことが在る。
3. **`uname`の`sysname`に`ZeikOS`と答える**: 採らない。libcが「知らないOS」として振る舞いを変えるか、止まる。方針（`ADR-0074`）に反する。
4. **シグナルの表を`UserProcess`に足す**: 採らない（上の2。遠征スタックの余りのため）。
5. **`fcntl`の`F_DUPFD`で、Linuxと同じにファイルの位置を共有する**: 今は採らない。`File`が位置を持つ作りで、共有するには位置を別の表へ出す必要が在る。`docs/deferred-decisions.md`に行を置いた。
6. **`sendto`の`flags`（`MSG_NOSIGNAL`・`MSG_DONTWAIT`）を見る**: 見ない。シグナルは無く、ソケットの書きは待たない。

## Consequences

- muslの静的なプログラムの起動の列（`arch_prctl`から`rt_sigprocmask`まで）が、`mmap`・`mprotect`を除いて通る。終わりの`exit_group`も通る。**→ `mmap`・`mprotect`は`ADR-0082`で入り、列の全部が通る**（2026-10-06）。
- `syscall-test`に検算を15本足した（80から94）。`FUTEX_WAIT`で待つ場面に入るプログラム（`/bin/futex-wait`）を`spawn`で起こし、137で終わることを見る。`syscall-test`自身も`exit_group`で終わる。
- 遠征スタックの使用量は変わっていない（実測。`syscall-test` 32,280のまま。小物の呼び出しは、載せる経路より浅い）。
- `.bss`が、プロセスごとの状態の表の分（4欄×約2.2KiB）増えた。
- 配送が無いので、`SIGSEGV`のハンドラを登録したプログラムも、ページフォルトで終わる（Linuxとの差。配送は別の課題）。

## Addendum（2026-10-06。M1の実測で足した小物）

Linux向けのmuslの静的な像（`linux-programs/m1-rust`・`m1-c`。`ADR-0074`のM1）をLinux上で`strace`して、まだ無かったものと、Linuxと違っていたものを足した。決定1の表の続きである。

| 呼び出し | 受ける形 |
|---|---|
| `writev`・`readv` | `struct iovec`を1本ずつユーザーの番地から読み、`write`・`read`を1本ずつ呼ぶ（控えの配列を遠征スタックに置かない）。長さ0の本は飛ばす。途中で失敗したら、動いた分が在ればその数を、無ければ失敗を返す。`readv`は短く終わった本で止める。本の数は1,024（`UIO_MAXIOV`）まで |
| `poll`の`events`が0 | 開いているかの問い合わせ。開いていなければ`revents`に`POLLNVAL`、開いていれば何も起きない（Linuxと同じ。Rustの`std`が起動時に0・1・2へ打つ。**以前は`-EINVAL`で、`std`は`fcntl`へ落ちていた**）。負のfdは見ない |
| `lseek`の`SEEK_CUR`・`SEEK_END` | 今の位置・末尾からの符号つきの相対。負の位置は`-EINVAL`。末尾より先は、`File::seek_to`が末尾で止める（以前からの形） |
| `getpid`・`gettid` | 1（`set_tid_address`と同じ。プロセスもスレッドも1つずつ） |
| `madvise` | 助言は受けて何もしない（0）。ページの境界に無い番地は`-EINVAL` |
| `tkill` | 自分（1）以外は`-ESRCH`。シグナル0は問い合わせで0。登録が`SIG_IGN`のものと、既定が「無視」のもの（`SIGCHLD`・`SIGCONT`・`SIGURG`・`SIGWINCH`）は0で何も起きない。**それ以外は、配送が無いので、名指しの行を出してプロセスを終わらせる**（終了状態は`128 + sig`。muslの`abort`は`tkill(gettid(), SIGABRT)`なので134） |
| `clock_nanosleep`（2026-10-07。M2の実測で足した） | 相対の眠りと、`CLOCK_MONOTONIC`の絶対の時刻（`TIMER_ABSTIME`）を受け、`nanosleep`と眠りの本体を共にする。`CLOCK_REALTIME`の絶対は、壁の時計が無いので`-EINVAL`。`rem`は書かない（割り込まれて早く戻る道が無い）。**muslの`nanosleep`とRustの`std::thread::sleep`は、`nanosleep`ではなくこれを打つ**——無いと`-ENOSYS`で`std`が止まった（`ADR-0083`のAddendum） |

知らない番号（`-ENOSYS`を返した）の回数と最後の番号を数え、プロセスの終わりの行（`spawn: … ended`・`user-run: … left Ring 3`）に出すようにした。Linuxのプログラムが、知らない番号を1つも打たずに終わったことを、起動ログで見るためである。数えはスロットの記録で、`spawn`の前後で親のものを退避して戻す（ほかの記録と同じ）。

実測（2026-10-06）: `m1-rust`はLinuxでもZeikOSでも39回のシステムコールで、同じ出力と終了の状態4。`m1-c`はLinuxで16回、ZeikOSで21回——ZeikOSではfd 1が端末なので`ioctl(TIOCGWINSZ)`が通り、muslのstdoutが行ごとの緩衝になって`writev`が増える（Linuxでも端末に出せば同じ）。
