# 0090. 壁の時計は、起動時にCMOSのRTCを1回読み、単調なティックの経過で進める

## Status

Accepted（運用者の決定。2026-10-20。C1S4で実装した）

## Date

2026-10-20

## Context

Seinasのコンポジタを試しに起こしたところ、起動の直後に終わった。tracingのログの時刻を取る`clock_gettime(CLOCK_REALTIME)`が`-EINVAL`を返し、Rustの`std`の`SystemTime::now`がpanicしたためである。

ZeikOSは、壁の時計の出所を持っていなかった。RTCを読む所が無く、`clock_gettime`は`CLOCK_MONOTONIC`だけを答えていた。`CLOCK_REALTIME`は`-EINVAL`である。理由は「0を返して黙って答えると嘘の時刻が広がる」だった（`kernel/src/syscall.rs`の`sys_clock_gettime`のdoc）。ext2の時刻の欄も、同じ理由で書いていない（S12-f-2。`docs/deferred-decisions.md`の「時刻の欄」の2行）。

Linuxのプログラムをそのまま動かす（`ADR-0074`）には、`CLOCK_REALTIME`が要る。案は2つあった。(a)はZeikOSが壁の時計を持つ形、(b)はSeinasの側でログの時刻を外す形である。(b)はSeinasの1か所で済むが、`SystemTime::now`を打つほかのプログラムが同じくpanicする。運用者は(a)に決めた。

## Decision

1. **出所はCMOSのRTCで、起動時に1回だけ読む**（A1）。BSPが、割り込みを許す直前（`start_timer`のstiの前）に読み、(Unix秒, そのときの単調なティック)を控える（`kernel/src/machine/pc/rtc.rs`）。
   - 読み方はLinuxと同じである。状態AのUIPが下りるまで待ち（20,000回まで）、2回続けて同じ値が読めるまで読み直す（5回まで）。状態Bに従ってBCDと2進、12時間と24時間を解く。世紀はACPIのFADTの`CENTURY`が示すレジスタから読み、無ければ20とみなす。年は2000〜2099だけを受ける。
   - ポートを叩くのは`read_register`の1関数で、`InterruptGuard`の内側で0x70に番号を書き、0x71から読む。番号のbit 7（NMI）は立てない。直にポートを読み書きする所の許可の表に1行がある。
   - CMOSの形の読み解きはPCの装置の言葉なので、`kernel/src/machine/pc/rtc.rs`に置く。どの装置でも同じ暦の計算（日付からUnix秒、壁の時刻の足し算）は`common/src/time.rs`に置く。どちらもホストのテストで固定する。
2. **壁の時計の今 = 控えた秒 + 控えたときからのティックの経過**（`common::time::wall_clock_from`）。読み直さない。
3. **読めなかったら、壁の時計を持たない。** `CLOCK_REALTIME`は今までどおり`-EINVAL`で、起動ログに`[WARN] rtc: could not read …`を1行出す。0や推した値では答えない。
4. **起動ログには値を出さない。** 形だけを出す（`rtc: the wall clock was read at boot (binary=…, 24-hour=…, century register …)`）。値を出すと、起動ログの参照が日付で揺れる。
5. **QEMUには`-rtc base=utc,clock=host`を明示する**（`xtask`の`qemu_launch_args`）。QEMUの既定も同じ形だが、既定に頼らない。カーネルはRTCをUTCとして読む。
6. **システムコール。**
   - `clock_gettime`は`CLOCK_REALTIME`を答える。ほかの時計（`_COARSE`・`BOOTTIME`など）は打つものが無いので受けない。
   - `clock_settime`（227）は、`CLOCK_REALTIME`なら`-EPERM`（Linuxの特権の無いプロセスと同じ答え）、ほかの時計なら`-EINVAL`を返す。時計は合わせないので、壁の時計は飛ばない。`timespec`は読まない。Linuxは先に読んで`-EFAULT`や`-EINVAL`を返すが、読むには新しい`unsafe`が要り、合わせない時計のために足すほどの理由が無い。
   - `gettimeofday`・`time`・`clock_getres`は足さない。muslと`std`は`clock_gettime`を打つ。
   - **`CLOCK_REALTIME`の絶対の締切は受けない。** `clock_nanosleep`の`TIMER_ABSTIME`と`timerfd_create(CLOCK_REALTIME)`は`-EINVAL`のままで、名前のある行を1行出す（`clock: … on CLOCK_REALTIME was refused with -EINVAL`）。受けるのは難しくない（時計が飛ばないので、MONOTONICの締切へ正確に直せる）。ただ、打つもの（Seinasとcalloopは`CLOCK_MONOTONIC`）が無い。
7. **ext2のinodeの時刻と`stat`の時刻の欄は、今までどおり書かない。** 出所が入ったので、S12-f-2の決め手（書く値が無い）は無くなった。それでも書かない。理由は3つある。
   - イメージの往復のバイト一致（`fs-extract`の判定1）と、時刻を潰して決定的にしているイメージの検査が使えなくなり、作り直しが要る。
   - 書いた時刻が`stat`に出ると、Linuxの参照（行の突き合わせ）とイメージの参照が時刻で揺れる。
   - `make`のように、時刻の新旧で動くものがまだ無い。
8. **確かめ。** `cargo xtask run --realtime-test`で行う。`realtime-test`のビルドが、起動時に`wl-realtime-rust`を起こす。
   - 標準出力の行は、Linuxの参照と突き合わせる。`SystemTime::now`が動く、1秒眠って1秒以上進む、`CLOCK_REALTIME − CLOCK_MONOTONIC`が20 ms未満しか動かない、`clock_settime`が`EPERM`の4つである。
   - `CLOCK_REALTIME`の値は、標準エラーへ出す。xtaskが、QEMUを起こす直前のホストの時刻T0と値の行を見た時刻T1のウィンドウ[T0 − 2秒, T1 + 1秒]で見る。経過時間ではなく、値そのものを見る。下の2秒は、秒の切り捨て（1秒）とティックの遅れの余裕（1秒）である。
   - 破壊テストは3件ある（`rtc-ignores-bcd`・`realtime-without-elapsed`・`realtime-drifts-from-monotonic`）。読めないときの道は、構成`--critical-test rtc-unreadable`で通す。

## Alternatives Considered

- **(b) Seinasの側でログの時刻を外す。** Seinasの1か所で済む。しかし、`SystemTime::now`を打つLinuxのプログラムは、ZeikOSでどれもpanicしたままになる。
- **A2: ブートローダがUEFIの`GetTime`を読み、`boot_info`で渡す。** ポートの`unsafe`もBCDの扱いも要らない。AArch64（AAVMF）でも同じ道で読める。ただ、読んだ時点（`ExitBootServices`の前）から、カーネルのティックが始まるまでの間を数えられない。その分（起動の数秒）が、いつも遅れとして乗る。A1は遅れが秒の切り捨て（1秒未満）だけで済み、確かめのウィンドウを狭く書ける。**AArch64の段で見直す。**
- **RTCを読み直して合わせる。** 読み直すたびに時計が飛びうる（戻る向きもある）。ティックの経過で進めれば飛ばない。
- **0を返す。** 嘘の時刻が広がる。以前からの決まりのとおり採らない。

## Consequences

- `SystemTime::now`と、tracingのログの時刻が動く。
- **相対の時刻は、秒の切り捨てで0〜1秒遅れる。** 秒の切れ目を待って合わせる形（起動が最大1秒延びる）は採らなかった。その後はティック（10 ms）で進み、ティックが落ちればその分も遅れる。
- **限り。**
  - VirtualBoxの機械（`tools/vbox-vm.py`）には`--rtcuseutc`を渡していない。RTCが地方時になりうる（日本なら+9時間）。
  - 実機も、Windowsと同じ機械ではRTCが地方時のことが多い。ZeikOSはUTCと読むので、ずれる。
  - `tools/qemu-variants.py`と`tools/stack-deepest.py`は、自分でQEMUを起こし、`-rtc`を渡していない（QEMUの既定のUTCで読む）。
  - AArch64は壁の時計を持たない（`CLOCK_REALTIME`は`-EINVAL`）。A2はその段で見直す。
- ext2の時刻を書かない理由が、「書く値が無い」から「書くと検査と参照が揺れ、使う者もまだ居ない」に変わった。見直すのは、時刻の新旧で動くもの（`make`など）が要るようになったときである。
