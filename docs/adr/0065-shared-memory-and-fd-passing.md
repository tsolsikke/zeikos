# 0065. 共有メモリと fd の受け渡し——wl_shm.create_pool の逆算を、Linux の思想で

## Status

Accepted

## Date

2026-09-20

## Context

**第1段階の残りのうち、ソケット（`ADR-0064`）の次は共有メモリと fd の受け渡しである。** **Wayland が
fd を運ぶメッセージはコア全体で `wl_shm.create_pool` の1つだけである**（`docs/wayland-inventory.md`）
——**クライアントが `size` バイトの無名の共有メモリを作り（`memfd_create`＋`ftruncate`）、`mmap` して
模様を書き、fd をソケットの補助データ（`SCM_RIGHTS`）で送る。サーバーはその fd を `mmap` して読む。**

**要るのは3つ**——**(a) `size` バイトの共有メモリを持つ fd、(b) その fd をソケットの `msg_control` で
運ぶ、(c) 両側が `mmap` して同じ物理フレームを見る。** **`poll` も `shutdown` も要求されない。**

## Decision

**運用者の承認（2026-09-20。設計 Q1〜Q7 と、共有フレームの寿命の (A)）。**

1. **入口は Linux の番号で5つ**——`memfd_create`(319) / `ftruncate`(77) / `mmap`(9) / `sendmsg`(46) /
   `recvmsg`(47)。**番号・構造体レイアウト・errno・定数を Linux から採る**（`ADR-0020`。範囲の一覧は
   `docs/architecture.md` の「ABIの形は合わせる」に集約した）。
2. **共有メモリの実体は `crate::shm`**——**アロケータから取るフレームの配列を fd で持ち、参照数で
   返す。** **`mmap` は各プロセスの `MMAP_BASE`（`0x1000_0000`）から上へマップする**（PML4[0] の空き。
   4 MiB のウィンドウの拡張は要らない）。
3. **共有フレームの寿命は Linux の思想で作る（(A)）**——下の節。
4. **最初の利用者は検査用の組**（`sockd`/`sockc` を伸ばす。本番の利用者 Seinas がまだ無い例外の
   根拠は `ADR-0064` の「本番の利用者が無い、初めての段」を指す。捨てず検査に残す）。
5. **`syscall-test` に mmap の入口の検査（67/68）を足す**（起動時に毎回、入口が在ることを見る）。
6. **`mmap` のページテーブルの会計は (a)**——下の節。
7. **待つ理由を2つ持つ形は作らない**（持ち越し。見直すきっかけは画面と入力の受け渡しの段階）。

### 入口——受け付ける形（`msghdr`／`sockaddr` の絞った範囲）

**ユーザーから来る構造体を解釈する入口は、この段階でいちばん危ない面である。** **受け付ける形を絞り、
外は断る**（`bind` の `sun_path` の長さを列挙したのと同じ形。`ADR-0064`）。

| 入口 | 受け付ける | 断る |
|---|---|---|
| `socket` | `AF_UNIX`＋`SOCK_STREAM`＋protocol 0 | 他は `-EAFNOSUPPORT`/`-EINVAL`/`-EPROTONOSUPPORT` |
| `memfd_create` | 名前とフラグは見ない（無名を作る） | ——— |
| `ftruncate` | 共有メモリの fd、`MAX_SHM_PAGES` 以内、まだ据えていない | 大きすぎ `-ENOMEM`、二度目 `-EINVAL`、共有メモリでない `-EBADF` |
| `mmap` | 共有メモリの fd、`offset` 0、`len` は据えた大きさ以内 | それ以外は `-EINVAL`、共有メモリでない `-EBADF` |
| `sendmsg`/`recvmsg` の `msghdr` | **`msg_name` は NULL / `msg_iovlen` は 1 / `msg_controllen` は 20 以上（fd を運ぶとき）** | **`msg_name != 0` は `-EINVAL` / `msg_iovlen != 1` は `-EINVAL` / 補助データは `SCM_RIGHTS`＋`SOL_SOCKET` の fd 1 つだけ、それ以外は `-EINVAL`** |

**「絞る」は安全側だが、境界を書いておく**——**次に広げる者が、何を前提に広げるかを分かるように。**
**絞った範囲の外を破壊テストで1つ確かめた**——**`socket-msghdr-ignores-iovlen` は `msg_iovlen` を見ず、
`iovlen` が 2 でも1本目を送る。** **`sockc badmsg` が `iovlen=2` を送り、`-EINVAL` が返らず送った
バイト数が返ることで落ちる。**
### 共有フレームの寿命——3つが両立しない（Linux の思想で解いた）

**真の共有には、フレームが「片方のプロセスの終わり」を越えて生き、「最後の fd が閉じたとき」に
返る必要がある。** **ところが3つが両立しない。**

1. **同じ物理フレームを両方の空間にマップする**（共有の本体）。
2. **`AddressSpace::destroy` が、私物のサブツリーの present なリーフを全部集めて隔離へ入れる**
   （`kernel/src/address_space.rs`）。
3. **`spawn` の会計が「`consumed == quarantined`」と「`frames_taken == collected`」を主張する**
   （`ADR-0063` の (b1)）。

**共有フレームを普通にマップすると、片側のプロセスが終わるとき (2) がそのリーフを集めて返し、もう片側の
fd がまだ在れば二重に返る。** **避ける道は無い**（コピーにすれば衝突しないが、それは共有ではない）。

**採った手 (A)——Linux の設計思想。** **フレームはアロケータから取る（枠を固定しない）。共有で
あることをリーフの PTE の空きビット 9 で目印を付ける（`entry::PTE_SHARED`）。`destroy` は目印の立ったリーフを
集めない。会計は共有フレームを除く。`shm` が参照数を持ち、最後の fd が閉じたときにアロケータへ
返す。** **Linux も `struct page` 相当の管理で空きビットを使う思想である**——**ZaytOS は規模が
合わないので `struct page` 相当の表は持たず、`crate::shm` が参照数を持つ。**（運用者の決定。2026-09-20。
Linux 互換の範囲は `docs/architecture.md` の「ABIの形は合わせる」に集約した。）

**採らなかった手。**

- **(E) `.bss` の専用プールから取り、`destroy` が物理アドレスの範囲で飛ばす。** **却下**——**Linux の
  思想ではない**（枠を固定し、所有をフレームに持たせない）。会計に触らずに済む利点はあったが、
  運用者の決定は「今後も Linux 互換を考える。設計思想も合わせる」であった。
- **(B) 会計を通らない常駐に閉じる。** **却下**——**中途半端**（`destroy` の二重解放が残り、結局
  目印か固定枠が要る）。
- **(C) この段階は受け側だけ mmap にして真の共有を次の段階へ。** **却下**——**衝突を解いていない**
  （受け側の mmap もリーフを作り、送り側の fd がまだ在れば `destroy` と参照数の二重管理で同じ穴を踏む。
  運用者の指摘）。

### 2つ目の穴——`mmap` が「載せた後」にページテーブルを取る

**`mmap` は新しい領域（`MMAP_BASE` = `0x1000_0000`）へマップするので、その領域の中間ページテーブルを
1 枚アロケータから取る。** **`destroy` はその表を集める（アロケータのものなので正しい）が、
「取った本数」（`taken = space.frames_taken()`）は載せた時点で測るので、載せた後に取った表が
入らない**——**`collected > taken` で `frames_taken == collected` の検査が落ちる。**

**これは (A) の共有のリーフとは別の穴で、ページテーブルの構造の側である。** **実測で確かめた**——
**どの検査も起動後に PT を増やさない**（`brk` は既存の PT に収まる）。**`mmap` が初めて踏んだ。**

**手 (a)——載せた後に取った PT を数えて `taken` に足す**（スロットごとの計測
`POST_LOAD_FRAMES`。`mmap` が `map_4kib` の前後の空き数の差で数える。リーフは共有フレームで
アロケータに触らないので、差は表の分だけ）。**約6行。** **`map_4kib` の複製（手書きの walk）は
書かない**——**`map_4kib` は1箇所であるべきである**（運用者の決定）。

**暗黙の仮定を1つ外した**——**「載せた後に PT を取らない」。** **`mmap` のためではなく、載せた後に
PT を取る経路すべてのためである**——**`brk` が境を越えれば同じ穴を踏むので、同時に閉じた。**

### 会計が「何を主張しなくなったか」（A-2 / A-3）

- **(A-2) `destroy`**——**「私物のサブツリーのリーフを全部返す」を主張しなくなった。** **目印の立ったリーフは
  返さない。** **覆う判定**——**`shm` の created == released（フレームは参照数で返る。`shm:` の計測）。**
- **(A-3) 会計**——**`consumed == quarantined` の素の等式を、共有分について主張しなくなった。**
  **ウィンドウの間にアロケータから取ったまま返っていない共有フレームを `consumed` から引く。**
  **覆う判定**——**`shm` の created == released。**
- **`frames_taken == collected` は調整が要らなかった**（実測）——**共有のリーフはアロケータから直に
  取り `frames_taken` に入らず、`destroy` が飛ばして `collected` にも入らない。両方が除く。**

### detach はアロケータを自分で借りる（着手前に破棄の順序を実測した）

**`detach` は `close`（`SYS_CLOSE`）からも表の `Drop`（プロセスの終わり）からも来る。** **`Drop` は
引数でアロケータを持たない。** **破棄の順序を読んだ**（`run_loaded_program`）——**`give_back` が
`run_loaded_program` の前に済んでおり、破棄は隔離（`quarantine`）を使って live のアロケータを
借りないので、`Drop` の時点でアロケータは空いている。** **だから `detach` は自分で
`take`／`give_back` できる。**（これは (E) が避けていた点で、(A) で戻ってきた。着手前に確かめた。）

### 動いてよいもの——実装前に書き、ビルドしてから読んだ（実測。2026-09-20）

**定型の4つの形を当てた。** **(E) と違い、64 KiB の `.bss` プールは無い**（(A) はアロケータから取る）。

| 先に書いた見込み | 実測 |
|---|---|
| **ページテーブルの項がイメージの伸びぶん+1される** | **+5**——**イメージが5ページ伸びた**（`0x47d880`→`0x482890`）。**U/S監査 4255→4260、プロセスごと 2875→2880**（+5） |
| **`syscall-test` の高水位は入口が `dispatch` の中なので大きく動かない** | **31752→31864（+112。48%のまま）**——**起動時の mmap の検査（67）の分。半分の監視まで904** |
| **`.data` は少し、`.bss` は少し（プールは無い）** | **`.data` +224**（`SHMS`・`MMAP_NEXT`）、**`.bss` +16**（計測）、**`.text` +17,440**。**64 KiB のプールは無い** |
| **既定の起動ログはアドレスの行と `syscall-test` の syscall 数が動く** | **syscall 数 81→86**（検査 67/68）。アドレスの行と U/S が動く。**`shm:` の計測の行は既定では小さい値** |

### mmap のページテーブルの会計は (a)（`frames_taken == collected` を保つ）

**`mmap` は新しい領域へマップするので中間ページテーブルを1枚アロケータから取る。** **`destroy` は集めるが、
`taken`（`space.frames_taken()`。載せた時点で測る）に入らず `collected > taken` になる。** **手 (a)**
——**載せた後に取った PT を数えて `taken` に足す**（スロットごとの `POST_LOAD_FRAMES`）。**約6行。**
**`map_4kib` の複製は書かない**（1箇所であるべき。運用者の決定）。**暗黙の仮定を1つ外した**
——**「載せた後に PT を取らない」。`brk` が境を越えれば同じ穴なので同時に閉じた。**

### 検証（実測。2026-09-20）

**既定の `--socket-test` は判定13本すべて `true`、PASS**（ソケットの10本に、共有メモリの往復・計測・
`iovlen` の拒否を足した）。**共有メモリの往復**——**`sockc` が2ページ（6000バイト）の模様を書き、
fd を `SCM_RIGHTS` で送り、`sockd` が同じ物理フレームを `mmap` して読み、バイト単位で一致
（`sockd: shm 6000 bytes ok=true`）。** **破壊テストは socket 8＋shm 3＋`msghdr` 1 の12本で、3回続けて落ちた**
（`docs/verification-coverage.md` の表）。**`shm-ftruncate-ignores-size` は1ページの模様では機会が
無かったので、判定を2ページにして機会を作った**（切り詰めで `mmap` が `-EINVAL`）。

## Alternatives Considered

- **共有フレームの寿命を (E)/(B)/(C) で解く。** **却下**（上の「共有フレームの寿命」）。
- **`mmap` の PT を手書きの walk でプールから取る（会計に触らない）。** **却下**——**`map_4kib` の
  複製で繊細な `unsafe` を新規に書く。`map_4kib` は1箇所であるべき**（運用者の決定）。(a) を採った。
- **共有メモリの実体を私物の1つの入口で作る（`shm_create(size)`）。** **却下**——**`memfd_create`＋
  `ftruncate` は Wayland が実際に打つ Linux の形で、`ADR-0020` に沿う**（入口が1つ増えるが、ABI を採る）。
- **`shm-test` を別に分ける。** **却下**——**`socket-test` に足せば起動が1つ増えない**（Q3）。

## Consequences

**得たもの。** **名前で繋ぐソケットの上で、`SCM_RIGHTS` で fd を運び、両側が `mmap` して同じ物理
フレームを共有できる。** **Wayland の `wl_shm.create_pool` の予行が動く**（`docs/wayland-inventory.md`）。
**共有フレームの寿命は Linux の思想（所有をフレームに、空きビットで目印を付け、参照数で返す）。**

**失ったもの・先送り。** **共有メモリは `Shm` の配列（`MAX_SHM`=2）と `MAX_SHM_PAGES`=8 が上限
（プールの固定枠は無くなったが、配列の長さが上限）。** **FHD の枚（8 MiB）は入らない**（見直すきっかけは
画面と入力の受け渡しの段階）。**`munmap` は無く、プロセスの終わりに外す。** **`mmap` は参照を持たない**
——**fd を開いたままマップすること**（閉じてから別の共有メモリがプールのページを使い回すと重なるが、
検査ではプロセスが直後に終わるので起きない。見直すきっかけは本番の利用者が張って外す往復をするとき）。
**待つ理由を2つ持つ形は無い**（持ち越し。画面と入力の段階）。

**会計が「何を主張しなくなったか」**（上の「共有フレームの寿命」に (A-2)/(A-3)）——**`destroy` は
目印の立ったリーフを返さず、`consumed == quarantined` は共有分について素の等式を主張しない。覆う判定は
`shm` の created == released。**

**手順（A-1〜A-4）は別々のコミットにできなかった。** **`PageAttributes` に `shared` の欄を足すと、
それを構築する11箇所が1つのコミットに巻き込まれ、その中に入口と会計が混じる**——**`git add -p` を
使わないこの体制では、ファイル単位では割れない。** **順に実装して各段階で成功を確かめ、カーネル側は
`feat` 1本にした。次に構造体へ欄を足す段階では、着手の前に「割れない」と分かる。**

## Addendum（2026-09-23）——`recvmsg` が待つ前に fd を取っていた

**`--full` で `socket-test` が 1 度だけ落ちた。** **`sockd` が `recvmsg` で fd を受け取れず、データの `shm`（3 バイト）を
ただの文字として返した。** **機序**——**`recvmsg` は入口で渡された fd を取り（`take_fd`）、その後でデータを待っていた。**
**待つ間は BKL を放すので、その間に送り手が「fd を置く→データを書く」を済ませ、起きた受け手はデータだけを返す。**
**受け手が先に待つ順序になった時だけ出る**（いつもは受け手が `accept` の後の行を出している間に送り手が送る）。

**直した**——**`recvmsg` は、読める（データか EOF）か接続が無くなるまで待ってから fd を取る。** **送り手は fd を先に置く
ので、データが読めるなら fd は既に置かれている。** **待ち方は読みと同じで、待ちの数も同じ計測へ数える。**

**検査**——**`sockc shmlate` は繋いでから 200ms 眠って送り、受け手が先に待つ順序を必ず作る。** **破壊テスト
`socket-recvmsg-takes-fd-first` は直す前の形に戻し、その判定だけが落ちる**（`docs/verification-coverage.md`）。

**残る限界**——**fd はデータの位置と結びついていない。** **受け手の手前に未読のデータが在ると、その読みに fd が付く**
（Linux は fd を運んだバイトに付ける）。**要求と応答の形では手前に未読が残らないので起きない。** **0 バイトの `sendmsg`
で fd だけを送る形は、受け手が待ち続ける**（前から同じ。`userlib` は常に 1 バイト以上を送る）。**見直すきっかけは、手前に未読を
残したまま fd を送る利用者が来たとき**（Wayland の本番のクライアントはそうしうる）。

## Addendum（2026-10-04、`mmap`の範囲と上限は、Linuxのプログラムを動かす段で広げる）

運用者の決定（2026-10-04。`ADR-0074`）で、Linuxのプログラムをそのまま動かすことを目指すことになった。この文書が絞っていた範囲は、その段で広げる。本文は当時の判断の記録として残す。

- **`mmap`は共有メモリと画面のfdだけ**。無名の`mmap`、アドレスを指定する形、`munmap`を足す（`ADR-0044`のAddendum）。
- **`memfd_create`は引数を見ない**。Waylandのクライアントは名前とフラグ（`MFD_CLOEXEC`と`MFD_ALLOW_SEALING`）を渡し、`fcntl`で封をする（2026-10-04の実測）。受ける形にする。
- **共有メモリは2個、1個あたり8ページまで**。800×600の画面1枚で約1.8 MiBになるので、足りない。上限の見直しは、持ち越しの行のとおりである。
- **`sendmsg`と`recvmsg`は、iovが1本でfdが1つの形だけ**。実測では最小のクライアントはこの形に収まっていた。コンポジタの側が何を使うかは、動かす段で測る。

共有のページを破棄のときに集めない目印と、ページテーブルのフレームの会計は、そのまま使う。

## Addendum（2026-10-15、マップ中のmemfdを縮める`ftruncate`は`-EINVAL`で断る）

**決めたこと。** マップ中のmemfdを縮める`ftruncate`は断る。errnoは`-EINVAL`である。今の実装は2回目の`ftruncate`を向きにかかわらず`-EINVAL`で断る（`kernel/src/shm.rs`の`set_size`の`AlreadySet`）ので、「マップ中の縮小を断る」はその一部として守られている。断るときはカーネルが名指しの行を出す（`shm: ftruncate refused: shm N already has P page(s) (R reference(s)); the shrink to S byte(s) is -EINVAL …`。`kernel/src/syscall.rs`の`report_second_ftruncate_refused`）。表はマップの数を持たないので、マップ中かどうかで分けていない。

**Linuxとの差。** Linuxは縮められ、縮めた先のマップに触るとSIGBUSになる。ZeikOSで同じ形にするのは、ページをマップの表から外してSIGBUSを配送するM3bの後である（`docs/deferred-decisions.md`）。

**待つもの。** マップしていないmemfdの縮小と拡大、アンマップした後の縮小は、2回目の`ftruncate`を受け付ける課題の完了を待つ（`docs/deferred-decisions.md`の同じ行）。マップ中に大きくする向きには手を付けない。

**確かめ。** `linux-programs/m3a-memfd-shrink-rust.rs`（`memfd_create`→2ページの`ftruncate`→`mmap`→2ページ目に書く→1ページへ縮める`ftruncate`→読み戻す）を`memfd-shrink-test`のビルドだけが起動時に起こし、`--critical-test memfd-shrink-refused`が、イメージの行（`returned -22`、`still reads back 0x5a = true`）とカーネルの名指しの行で見る。Linuxと結果が違うので、Linuxの参照とは突き合わせない。破壊は`shm-ftruncate-shrinks-mapped-test`（縮める向きならマップ中でも末尾のフレームを返して縮める）。

## Addendum（2026-10-20、memfdのフラグと名前、封のビット、`FD_CLOEXEC`、memfdの`fstat`）

上の2026-10-04のAddendumの「`memfd_create`は引数を見ない」を閉じた。Linuxのプログラムが打つ形に、答えをそろえる段である。

- **`memfd_create`のフラグと名前。** フラグは`MFD_CLOEXEC`と`MFD_ALLOW_SEALING`だけを受け、ほかのビットは`-EINVAL`。名前はNULまで読んで長さだけを確かめ（NULを除いて249バイトを越えたら`-EINVAL`、読めなければ`-EFAULT`。Linuxと同じ順）、持たない（Linuxは`/proc`の表示に使うだけで、ZeikOSには`/proc`が無い）。ZeikOS向けのプログラム（`userlib`と`syscall-test`）は、名前にNULLを渡していたので、名前を渡す形に直した。
- **封のビットは持って答えるが、効き目はまだ無い。** 表の欄に封を持ち、`F_ADD_SEALS`と`F_GET_SEALS`はLinuxと同じ答えを返す——知らないビットは`-EINVAL`、`F_SEAL_SEAL`の後は`-EPERM`、`MFD_ALLOW_SEALING`の無いmemfdは最初から`F_SEAL_SEAL`を持つ。`F_SEAL_EXEC`を足すと、書きと伸び縮みの封（`SHRINK`・`GROW`・`WRITE`・`FUTURE_WRITE`）も一緒に立つ（Linux 6.3からの、実行のビットを持つmemfdの形。ZeikOSのmemfdはいつも0777なので、いつもこの形になる。WSLで測った値は62）。**効き目は段階的に入る**——`F_SEAL_SHRINK`・`F_SEAL_GROW`が`ftruncate`を断る形は、2回目の`ftruncate`を受け付ける段で、`F_SEAL_WRITE`を足すときの`-EBUSY`と書けるマッピングを断る形は、マッピングの数を持つ段で入る。**それまでの間は、ビットは持つが効かない**（Linuxとの差）。
- **`FD_CLOEXEC`は持って答えるが、`exec`が無いので効き目は無い。** fdの表に1ビットの印を持つ。立てるのは`fcntl(F_SETFD)`・`F_DUPFD_CLOEXEC`と、作るときの`*_CLOEXEC`（`open`の`O_CLOEXEC`〔`/dev/fb0`を含む〕、`socket`と`accept4`の`SOCK_CLOEXEC`、`EFD_CLOEXEC`・`TFD_CLOEXEC`・`EPOLL_CLOEXEC`・`MFD_CLOEXEC`、`recvmsg`の`MSG_CMSG_CLOEXEC`で受け取ったfd）で、置く・閉じるときに下ろす。印は置けた番号にだけ立てる関数（`insert_with_cloexec`）で置くので、置いた後に失敗する道は無い。
- **memfdの`fstat`。** 種類のビットは普通のファイル、許可は0777、`st_blksize`は4096、`st_blocks`は持っているページの数×8（Linuxと同じ。Linuxは書いたページだけを数えるが、ZeikOSは`ftruncate`でページを取るので、書く前から数える）。ほかの種類と`stat`の`st_blksize`は、今までどおり0である。
- **見つけたこと。** ZeikOSのソケットは、`SCM_RIGHTS`で渡すfdを向きごとに1つまでしか待たせない（`kernel/src/socket.rs`の`queue_fd`）。2つ目は1つ目が受け取られるまで送れない。
- **確かめ。** `linux-programs/wl-memfd-seals-rust.rs`（`linux-socket-test`のビルド）。フラグと名前の答え、封の答え、memfdの`fstat`、作り方ごとの`F_GETFD`を、WSLで取ったLinuxの参照と全行突き合わせる。破壊テストは2件（`memfd-ignores-allow-sealing`・`memfd-seals-not-kept`）で、どちらもイメージが名指しの行を出して終わる。


## Addendum（2026-10-26、共有メモリの上限を上げ、伸ばす`ftruncate`を受ける）

Seinasのタスクバーのpoolは`ftruncate(fd, 256000)`で始まり、1280の幅では2枚目のバッファで伸びて、2回目の`ftruncate(512000)`になる。今までの上限（2個、1個あたり8ページ）と、2回目の`ftruncate`を向きにかかわらず断る形では、どちらも通らなかった。

### 決めたこと

- **上限。** 個数は16（`MAX_SHM`）、1個あたり4,096ページ = 16 MiB（`MAX_SHM_PAGES`。FHDの2枚、約16.6 MBまで入る）、全体で16,384ページ = 64 MiB（`MAX_SHM_TOTAL_PAGES`）。
- **フレームの持ち方。** 共有メモリごとの固定の並びを広げた（`[PhysAddr; 4096]`×16で512 KiBの`.bss`）。確保の失敗の道を増やさない。`Shm`は`Copy`を外し、32 KiBの値を作ってスタックに載せる形をコンパイラが止める。欄を空けるときは、その場で書き換える（`Shm::clear`）。`mmap`は、並びを丸ごとスタックへコピーする形をやめ、ロックを取って1ページずつ引く（`frame_at`）。
- **伸ばす`ftruncate`。** 足りないページのフレームを取って0で埋め、並びの後ろへ足す。既にあるマッピングは触らない（ページテーブルは変えない）。伸ばした分は、後から`mmap`すれば見える（Linuxと同じ）。同じ大きさへの`ftruncate`は何もせずに0を返す。
- **確かめる順とerrnoはLinuxにそろえた**（`fs/open.c`の`do_ftruncate`、`fs/attr.c`の`inode_newsize_ok`、`mm/shmem.c`の`shmem_setattr`）。1個の上限を越えれば`-EFBIG`（ファイルの大きさの上限を越えるときの値）、その後に封を見て`F_SEAL_GROW`なら`-EPERM`。全体の上限を越えるときと、フレームが足りないときは`-ENOMEM`。判断は`kernel/src/shm.rs`の`truncate_plan`にまとめ、ホストのテストで順を固定した。
- **縮める`ftruncate`。** 今までどおり`-EINVAL`で断り、名指しの行を出す（`shm: ftruncate refused: … the shrink to …`）。マップ中かどうかを分けられるのは、マッピングの数を持つ課題の後である（マッピングが無いときの縮小は、そこで受ける）。

### Linuxとの差

合わせなかったもの:

- **ページは`ftruncate`の時点で取り、0で埋める。** Linuxのmemfdは`NORESERVE`で、書いた時点で取る（`ftruncate`ではメモリを数えない）。そのため、ZeikOSでは`ftruncate`がフレームの不足を`-ENOMEM`で返し、Linuxは通す（書いたときに足りなければ`SIGBUS`）。0で埋める間は、システムコールの中なのでBKLを持ち、割り込みも止まっている。測った時間（QEMU。2026-10-26）は、256,000バイト（63ページ）で約0.2ミリ秒、16 MiB（4,096ページ）で約12〜20ミリ秒（1〜2ティック）だった。大きいときに割り込みを止めたまま埋める形は、別に直す。
- **長さが負の`ftruncate`は`-EFBIG`になる。** 長さを符号の無い値として読むので、1個の上限を越える大きさとして断る。Linuxは`-EINVAL`を返す（`ksys_ftruncate`）。別に直す。
- **1個16 MiB・全体64 MiBの上限。** Linuxのmemfdには、この大きさの上限は無い（`RLIMIT_FSIZE`と`s_maxbytes`だけ）。
- **共有のマッピングの`munmap`は、今も`-EINVAL`で断る**（マッピングの数を持つ課題の後で受ける）。Seinasの伸ばし方では、タスクバーは「`ftruncate(512000)` → `mmap(512000)` → 前の256,000の`munmap`」、コンポジタは「前の256,000の`munmap` → `mmap(512000)`」の順で（Seinasの側のLinuxでのstrace。どちらも`mremap`は使わない）、`munmap`が断られる。呼ぶ側は、どちらも`munmap`の失敗を見ずに進む——タスクバーのmemmap2は`Drop`で答えを捨て（`memmap2`の`unix.rs`の`impl Drop for MmapInner`）、コンポジタのSmithayは`remap`で`let _ =`で捨ててから新しい大きさで`mmap`する（Smithay の shm の pool の`MemMap::remap`）。そのため、古いマッピングは仮想アドレスごと残る。ZeikOSの上で確かめるのは、タスクバーをつなぐ結合の課題である。

### 確かめ

- `linux-programs/wl-memfd-grow-rust.rs`（`linux-socket-test`のビルド）。256,000バイトの`ftruncate`→`mmap`→模様、同じ大きさの`ftruncate`、512,000バイトへ伸ばして全体を新しく`mmap`し、前のバイトが残り・伸ばした分が0で・前のマッピングからも同じバイトが読めること、`F_SEAL_SHRINK`だけなら伸ばせて`F_SEAL_GROW`なら`EPERM`、16 MiBへの`ftruncate`を、Linuxの参照と突き合わせる。古いマッピングの`munmap`は打たない。
- `-EFBIG`と全体の上限の`-ENOMEM`はLinuxでは通るので、`kernel/src/shm.rs`のホストのテスト（`truncate_plan_follows_the_linux_order`）で固定した。

## Addendum（2026-10-27、`recvmsg`の`msg_name`と書き戻し、閉じた接続に残ったfd）

Seinasのタスクバーとコンポジタは、wayland-backend（rustix）を通して`recvmsg`を打つ。rustixは`msg_name`に置き場を必ず渡し、戻った`msg_controllen`の長さを制御のデータとして読む。今までの`recvmsg`は`msg_name`がNULLでないと`-EINVAL`で断り、`msg_controllen`と`msg_flags`を書き戻さなかったので、タスクバーは最初の往復で止まっていた。

### 決めたこと

- **`recvmsg`は`msg_name`の置き場を受ける。** 冒頭の表の「`msg_name`はNULL」は、`sendmsg`だけの決まりになった。置き場があって`msg_namelen`が負なら、読む前に`-EINVAL`を返す（`net/socket.c`の`__copy_msghdr`）。置き場がNULLなら、長さは見ない。
- **読めたら、Linuxの`____sys_recvmsg`と同じ順で書き戻す。** `msg_namelen`と名前、`msg_flags`、`msg_controllen`の順である。読めなければ（`-EAGAIN`も）、どの欄も書かない。
  - **名前は、送ってきた側のソケットの名前である**（`net/unix/af_unix.c`の`unix_copy_addr`）。受け付けた側は、listenerの名前を受け継ぐ（`unix_stream_connect`）。ZeikOSは`connect`の時に、listenerの名前を接続へコピーする。繋いだ側は名前を持たないので、受け付けた側（コンポジタ）が受けると`msg_namelen`は0で、置き場は書かない。繋いだ側（タスクバー）が受けると、`msg_namelen`は2＋名前＋NULの1で、名前は置き場の長さまで書く（長さは切り詰める前の値）。
  - **`msg_flags`は、渡した`MSG_CMSG_CLOEXEC`と、fdを捨てたときの`MSG_CTRUNC`である。** 渡した値は残さない。
  - **`msg_controllen`は、補助データに使った長さである。** fdを1つ受ければ24（`CMSG_SPACE(4)`。置き場が20〜23バイトなら、その長さ）、受けなければ0。
- **fdを渡せないときは、fdを捨てて`MSG_CTRUNC`を立て、受けたバイトは返す**（`net/core/scm.c`の`scm_detach_fds`）。補助データの置き場がNULLか、20バイトに足りないか、書けないとき、それにfdの表が埋まっているときである。今までは、置き場が足りないと`-EMSGSIZE`を返して、据えたfdが表に残っていた。表が埋まっていると`-EMFILE`を返して、バイトはソケットに残っていた（`ADR-0091`の「Linuxとの差」の2つ目）。
- **閉じた端へ向けて置かれたまま受け取られなかったfdは、閉じたときに手放す**（`unix_release_sock`が受信の待ち行列を捨てるのと同じ）。両端が閉じたら、もう一方へ向けて置かれたfdも手放す（閉じた相手へ送ったfd）。今までは接続の欄を上書きするだけで、共有メモリの参照が減らず、上限の16個で`memfd_create`が断られた。

### Linuxとの差

合わせなかったもの:

- **バイトの読みが失敗したときも、fdは表に残る。** fdを表へ据えてからバイトを読むので、iovが書けずに読みが失敗すると、fdは表に残り、番号は伝わらない。Linuxは、データをコピーできなかったfdを待ち行列に残す。Waylandの経路では、iovに書けない形は起きないので、今回は直さなかった。
- **`sendmsg`の`msg_name`は、今までどおりNULLだけを受ける。** Linuxは長さ0なら受け、正なら`-EISCONN`を返す。
- **閉じた相手への`sendmsg`は、fdを相手の待ち行列へ置いてから`-EPIPE`を返す。** Linuxは置かない。置いたfdは、送った側が閉じたときに手放す（上の決めたことの4つ目）。

### 確かめ

- `linux-programs/wl-recvmsg-name-rust.rs`（`linux-socket-test`のビルド）。`recvmsg`を`syscall`で直に打ち、両方の向きの`msg_namelen`と名前、短い置き場、長さ0と負、置き場がNULLで長さが負、`-EAGAIN`のときに書かないこと、補助データの置き場が64・64と`MSG_CMSG_CLOEXEC`・20・NULL・16・19バイトのときの`msg_flags`と`msg_controllen`と、fdが残らないことを、Linuxの参照と突き合わせる。
- `linux-programs/wl-close-queued-fd-rust.rs`（同じ）。受け取られないfdを載せたまま接続を閉じることを、閉じる順の3つの形で20回ずつくり返し、`memfd_create`が共有メモリの上限を越えて通ることを見る。
- `linux-programs/wl-fd-limit-rust.rs`に、表が埋まったときの`recvmsg`の戻り値・`msg_flags`・`msg_controllen`の行を足した。
- 直す前の形では、どれも名前のある判定（`printed what Linux prints = false`）で落ちることを確かめた（2026-10-26と10-27）。
