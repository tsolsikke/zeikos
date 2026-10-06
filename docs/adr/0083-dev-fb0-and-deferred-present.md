# 0083. `/dev/fb0`をLinuxのfbdevの形で開かせ、裏バッファを間隔ごとに画面へ転送する

## Status

Accepted（2026-10-06に運用者がM2の設計案を承認し、2026-10-07に最初の刻みを実装した）。

## Date

2026-10-07

## Context

- M2（`ADR-0074`の順序の1）は、Seinasの共通の描画（pixman）の結果を、fbdevの裏側（`seinas-fbdev`）を通してZeikOSの画面に出すことである。`seinas-fbdev`はLinuxのfbdevのABIに従う——`/dev/fb0`を開き、`FBIOGET_VSCREENINFO`・`FBIOGET_FSCREENINFO`で形を訊き、面を`mmap`して書く。**書いた後に何かを打つことはしない**（Linuxのfbdevは、書けばそのまま映るため。`Presenter`という差し替えの口は在るが、Linux向けの`WriteThrough`は何もしない）。
- ZeikOSの画面のfdは、`SYS_OPEN_SCREEN`（ZeikOS独自の番号）で開き、**裏バッファ**を`mmap`し、`FBIOZPRESENT`（ZeikOS独自の`ioctl`）で矩形を画面へ写す形だった（`ADR-0066`のY-c）。MMIOをRing 3へ出さない（Q1）ので、どこかで転送が要る。
- Linuxのプログラムをそのまま動かす方針（`ADR-0074`）では、`seinas-fbdev`に手を入れずに映る形が要る。

## Decision

### 1. `/dev/fb0`は、`open`の中で名前で分け、画面のfdに落とす

- `sys_open`が道を写した直後、フラグの規則の前に、`/dev/fb0`を名前で分ける（`readlink`の`/proc/self/exe`と同じ形。ext2に`/dev`は作らない——像を変えずに済み、装置の種類はカーネルの側で決まる）。開き方（`O_RDWR`・`O_RDONLY`）は見ない。
- 中身は`SYS_OPEN_SCREEN`と同じ`File::Screen`で、同じ関所（前景の系統だけ。図形モードの持ち主は1つ）を通る。`fstat`は文字装置（`S_IFCHR`）。`close`で図形モードを抜ける。
- `SYS_OPEN_SCREEN`は残す（`gfxd`・`compd`と、その破壊テストのため）。`/dev/fb0`へ寄せて消すのは別の課題（`docs/deferred-decisions.md`）。

### 2. `/dev/fb0`で開いた間は、カーネルが間隔ごとに裏バッファの全体を画面へ転送する

- 利用者は`FBIOZPRESENT`を打たない。**書けば映る**を、Linuxの`fb_deferred_io`（USBの表示装置などが使う。書いたページを一定の間隔で転送する）と同じ考えで作る。
- 間隔は50 ms（20 Hz。`kernel/src/syscall.rs`の`FB0_PRESENT_INTERVAL_TICKS`）。利用者が見る遅れは最大でこの間隔。
- **転送する所は2つで、どちらも割り込みの外でBKLを持つ**——システムコールの戻り（Ring 3から入った道の出口）と、BSPのアイドルの定常ループ（前景のプロセスが`nanosleep`で眠っている間は、ここしか走らない。BKLを取ってから転送し、放してから`hlt`する。開いていなければBKLを取らずに通る）。**割り込みの中では転送しない**——全面の転送は数ミリ秒で、割り込みの中に置く長さではない。
- `close`で、最後に1回転送してから図形モードを抜ける（間隔の途中で書いたものを捨てない）。
- `SYS_OPEN_SCREEN`で開いた図形モードでは転送しない（利用者が`FBIOZPRESENT`で矩形を写す。今までどおり）。
- 回数とサイクルを数え、判定の行（`fb0: N deferred present(s) …`）に出す。**利用者の`ioctl`の回数（`presented`）とは分けて数える**——どちらが写したかが読めるように。

### 3. 間隔ごとの転送は、全体を写す。書いたページだけを写す形は後で

- 1280×800の全面の転送は、TCGで約5.3Mサイクル（実測。`fb-test`の31回で平均5,332,420）。20 Hzなら約107Mサイクル/秒で、3.5 GHzのCPUの約3%。
- 書いたページだけを写す形（`fb_deferred_io`の本来の形。写像を書けない形にして、最初の書き込みの#PFで印を付ける）は、ユーザーの#PFを畳む今の形に「許して印を付ける」道が要るので、別の課題にする（`docs/deferred-decisions.md`）。

## Alternatives Considered

1. **MMIO（本物のフレームバッファ）を`/dev/fb0`の`mmap`でRing 3へ出す**: 採らない。Q1（`ADR-0066`）を崩す。
2. **Seinasの`Presenter`をZeikOS向けに差し替えて`FBIOZPRESENT`を打たせる**: 採らない。Seinasの文書が予告している口だが、「Linuxのプログラムをそのまま動かす」（`ADR-0074`）に反する。
3. **転送をタイマの割り込みの中で行う**: 採らない。数ミリ秒の仕事を割り込みの中に置くことになり、BKLの規律（割り込みの入口は短く）に反する。
4. **ユーザーの書き込みの#PFで印を付け、書いたページだけを転送する**: 今は採らない（上の3）。
5. **`/dev/fb0`をext2の像のノードとして作る**: 採らない。像を変えずに済む形（名前で分ける）のほうが小さく、装置の種類をカーネルが決められる。

## Consequences

- `fb-test`（`open("/dev/fb0", O_RDWR)`・fbdevの`ioctl`・`mmap`・四隅に色・`nanosleep`・`close`）が、`FBIOZPRESENT`を打たずに画面へ出る。`xtask`の`--fb-test`が、絵の在る間と抜けた後に`screendump`で四隅を読む。破壊テストは3つ（転送しない・閉じても抜けない・間隔を見ない）。
- `seinas-fbdev`（musl静的PIE）が打つ装置の列（`open(O_RDWR)`・`ioctl`×2・`mmap(MAP_SHARED)`・`nanosleep`・`munmap`・`close`）は、この決定で全部通る形になった。実際に像に入れて走らせるのは次の刻み（`ZEIKOS-75`相当の作業。ここには課題の番号を書かない）。
- アイドルの定常ループが、`/dev/fb0`を開いている間だけBKLを取る（開いていなければ原子的な印を1つ読むだけ）。
- 前景のプロセスがCPUを使い切っていて、システムコールも打たない間は、転送が起きない（アイドルが走らないため）。描いてから眠る・待つ形のプログラム（Seinasのfbdevの裏側、イベントのループ）では差が出ない。`docs/deferred-decisions.md`。
- 遠征スタックの使用量は変えない（転送はカーネルのタスクの文脈で、`Console::present`を呼ぶだけである）。
