# 0087. listenのソケットをパスにbindし、隣のロックファイルをflockで取る——Seinasの起動の一連を通す

## Status

Accepted（運用者の決定。2026-10-16。実装はC1S4で行う）

## Date

2026-10-16

## Context

Seinas（wayland-server）は起動時に、ソケットのパスの隣にロックファイルを `O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC` で開き、
`flock(LOCK_EX|LOCK_NB)` で取り、`fstat`・`stat` で確かめてから、`socket` → `bind(パス)` → `listen` と進む。listen の
ソケットは 3 つ（ふだん・シェル・IME。`s.sock`・`s-shell.sock`・`s-ime.sock` と、`s.lock`・`s-shell.lock`・`s-ime.lock`）。

いまの ZeikOS では、この一連は最初の `open` で止まる（`O_RDWR` は `-EROFS`。`kernel/src/syscall.rs` の `sys_open` は
`O_WRONLY|O_TRUNC` と `O_WRONLY|O_CREAT|O_TRUNC` の 2 つの形しか書きの形として受けない。`ADR-0037` の Addendum）。
その先も、`flock`（73）は未実装（`-ENOSYS`）、`socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC)` は `-EINVAL`
（`SOCK_CLOEXEC`・`SOCK_NONBLOCK` を断る。`ADR-0064`）、listener は 1 本（`MAX_LISTENERS`）で止まる。
`bind` と `connect` の名前はカーネルの表で、ext2 に inode を作らない（`ADR-0064` の決定 3。抽象名は `-EINVAL`）。

## Decision

1. **ソケットとロックファイルの置き場は `/tmp` の下にする**（`/tmp/seinas/` のような 1 段のディレクトリ。環境変数
   `XDG_RUNTIME_DIR=/tmp/seinas` を `/etc/environment` で渡す。`ADR-0052`）。`/run` は作らない——`ADR-0042` の場所の表に
   `/run` は無く、`/tmp` は在る。メモリの上の別のファイルシステム（tmpfs）は作らない——ext2 の像は RAM の写しで、
   書きは `close` で装置へ届く（`ADR-0037`・`ADR-0051`）。`/tmp` が起動をまたいで残る（deferred の「`/tmp` を誰が消すか」）
   ことは、ロックファイルの扱い（決定 4）で吸収する。

2. **ソケットの名前は、今までどおりカーネルの表に置く**（ext2 に inode を作らない。`ADR-0064` の決定 3 を保つ）。
   `bind(パス)` の `sun_path` をそのまま名前にし、`connect(同じパス)` が同じ表を引く。**パスに残ったソケットは無い**——
   listener は fd の `close`（プロセスの終わりを含む。`kernel/src/vfs.rs` の `File` の `Drop`）で表から消えるので、
   次の起動の `bind` は常に通る。`stat(ソケットのパス)` は `-ENOENT`、`unlink(ソケットのパス)` も `-ENOENT` で、
   wayland-server の「残ったソケットを消してから bind する」経路はそのまま通る。
   **名前の上限 31 バイト（`NAME_MAX`）を 107 バイトへ上げる**（`sun_path[108]` いっぱい。`/tmp/seinas/s-shell.sock`
   は 24 バイトで足りるが、`XDG_RUNTIME_DIR` の長さに依らない形にする）。

3. **listener を 4 本、接続を 8 本にする**（`MAX_LISTENERS` 1 → 4、`MAX_CONNECTIONS` 2 → 8）。Seinas の 3 本に 1 本の
   余り。接続は、クライアントごとに 1 本と、シェル・IME の 1 本ずつを見込む。輪は向きごとに 1,024 バイトのまま
   （8 本 × 2 KiB = 16 KiB。`.data` に載る。deferred の「接続は同時に 2 つ、listener は 1 本」の行を閉じる）。

4. **`flock` を実装する**（番号 73。`LOCK_SH`・`LOCK_EX`・`LOCK_UN` と `LOCK_NB`）。鍵は ext2 の inode 番号、持ち主は
   開いたファイル（`File::Regular` の 1 つ）である。`LOCK_EX|LOCK_NB` は、別の開いたファイルが取っていれば
   `-EWOULDBLOCK`（`-EAGAIN`）、同じ開いたファイルなら取り直しとして 0。`LOCK_NB` の無い形で取れないときは、
   `Wait::FileLock { inode }` で眠り、放されたら取り直す。**放すのは `LOCK_UN` と `close`**——プロセスの終わりは
   fd を全部閉じるので、そこで放される（Linux と同じ）。`F_DUPFD` で写した fd は、Linux では同じ open file description を
   共有してロックも共有するが、ZeikOS の `File` は写しごとに別なので、**写しは別の持ち主として扱う**（Seinas は
   ロックファイルの fd を dup しない。文書に書く）。表は 8 本（listener 4 本 × 2 の余裕）。

5. **`open` の書きの形を広げる**——`O_RDWR`（`O_CREAT`・`O_TRUNC`・`O_CLOEXEC` の有無にかかわらず）を受け、
   `File::Regular { writable: true }` で開く。読みは今までどおり位置から、書きは `O_TRUNC` なら 0 から（今の全置換の
   部品のまま。位置書きは入れない——`ADR-0037` の「決めないこと」のまま）。`O_CLOEXEC` は `exec` が無いので無視する
   （`F_SETFD` と同じ。`sys_fcntl` の doc）。`O_CREAT` の親ディレクトリは在る必要がある（`/tmp/seinas` は
   `/etc/profile` の `mkdir` か `kernel/build.rs` の種で作る。後者を選ぶ——起動の順に依らない）。

6. **`socket` の `SOCK_CLOEXEC` を無視して受ける**（`SOCK_NONBLOCK` は断ったまま。待たない形は別の課題）。
   **`accept4`（288）を足し、`SOCK_CLOEXEC` を無視して `accept` と同じにする**。Rust の std と wayland-server は
   `socket` と `accept4` に `SOCK_CLOEXEC` を必ず付けるので、これが無いと Seinas も zeyes も `socket` で止まる。
   （受け入れ条件の外だが、同じ一連の次の 1 歩で、入れないと条件 2 の「クライアントが connect できる」を Linux の
   プログラムで確かめられない。）

## Alternatives Considered

- **ソケットの名前を ext2 の inode にする**（Linux の形）——却下。ext2 に `S_IFSOCK` の inode を作る部品が無く
  （`common/src/ext2.rs` の `create_file` は普通のファイルだけ）、残ったソケットの `unlink` と権限の話が付いてくる。
  Seinas が要るのは「同じパスで繋がる」ことだけで、表のままで満たせる。`ls` に出ないことと、`stat` が `-ENOENT` を
  返すことは、wayland-server の経路では害にならない（残ったソケットが無いので、消す経路を通らないだけ）。
  セルフホスティングの条件（`ADR-0086`）にも触れない——条件は `clone`/`execve`/`wait4`・動的リンク・シグナル・
  `/proc`・大きなファイルシステムで、ソケットの名前の置き場は含まない。ただし「数千のファイルと大きなファイルを扱える
  ファイルシステム」へ替えるときに、名前を inode へ移す選択は残る。
- **メモリの上の tmpfs を足す**——却下。ファイルシステムを 1 つ増やす（VFS の mount の概念が要る。`ADR-0025` は
  `inode`/`file` まで）。`/tmp` が RAM の写しの上に在るので、速さの差も無い。
- **`flock` を「常に 0 を返す」で済ます**——却下。Seinas は 2 重起動の検出にこれを使い、ZeikOS でも 2 つ目の Seinas が
  同じロックを取れてしまうと、ソケットの名前の `-EADDRINUSE` が先に出るだけで、ロックの意味が無くなる。実装は小さい
  （表 8 本と `Wait` の 1 種類）。
- **listener を 3 本ちょうどにする**——却下。検査用に 1 本の余りを置く（`socket-test` が listener を 1 本使う）。

## Consequences

- 通る一連: `open(O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC)` → `fcntl(F_SETFD)` → `flock(LOCK_EX|LOCK_NB)` → `fstat`・`stat` →
  `socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC)` → `bind` → `listen` → `accept4`。クライアントは `socket(…|SOCK_CLOEXEC)` →
  `connect(同じパス)`。
- 検査: `socket-test` に listener 3 本（名前 3 つで bind・connect）と、`flock` の確かめ（取る・別の fd で
  `-EWOULDBLOCK`・`close` で放す・`LOCK_UN`）と破壊（`flock-ignores-holder-test`: 持ち主を見ずに 0 を返す。
  `flock-keeps-lock-after-close-test`: `close` で放さない）を足す。`page permissions` と起動ログの参照を採り直す
  （種に `/tmp/seinas` が増える。ディスクのチェックサムが変わる）。Linux 向けの確かめのイメージ（musl の Rust）を
  1 本足し、Linux の参照を控える（`flock` と `bind` の一連）。
- 持ち越し: `SOCK_NONBLOCK`・`O_NONBLOCK` の待たない形（`F_SETFL` は受けて無視している）、`F_ADD_SEALS`（zeyes が
  memfd に打つ。`sys_fcntl` は `-EINVAL`）、`/tmp` を誰が消すか、`F_DUPFD` の写しのロックの共有。
- セルフホスティングの条件（`ADR-0086`の「これからの設計で守る条件」）に照らした——`SYS_SPAWN`の族を増やさず、Linuxの番号
  （41・43・49・50・73・288）の形で足す。ソケットの名前の置き場は条件に含まれない。ファイルシステムを替えるときに
  名前をinodeへ移す選択は残す。
