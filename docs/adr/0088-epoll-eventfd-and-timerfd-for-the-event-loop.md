# 0088. epoll・eventfd・timerfdを足し、Seinasのイベントループを待たせる——レベルトリガーとONESHOTだけを持つ

## Status

Accepted（運用者の決定。2026-10-19。実装はC1S4で行った）

## Date

2026-10-19

## Context

Seinas（wayland-server）のイベントループは calloop 0.14 で、その下の polling 3.11 が epoll を使う。起動するとすぐ、次の形で
カーネルを呼ぶ。

- `epoll_create1(EPOLL_CLOEXEC)`
- `eventfd2(0, EFD_CLOEXEC|EFD_NONBLOCK)`（ほかのスレッドから自分を起こす合図）
- `timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC|TFD_NONBLOCK)`（待ちの期限）
- timerfd を `events` 0 と `EPOLLONESHOT` で、eventfd を `EPOLLIN|EPOLLHUP|EPOLLERR|EPOLLPRI` と `EPOLLONESHOT` で足す
- 待つたびに `timerfd_settime` で期限を据え、timerfd の項目を `EPOLL_CTL_MOD` で付け直し、`epoll_pwait`（281。
  sigmask は NULL）で待つ

wayland-backend（0.3）は自分の epoll を作り、Seinas はそれを `F_DUPFD_CLOEXEC` で複製して calloop の epoll へ入れる。
つまり**入れ子の epoll** が要る。

いまの ZeikOS には、どれも無い（`-ENOSYS`）。待つ仕組みは `poll`（7）と、オブジェクトごとの `Wait` の理由（最大 4 本を
同時に待てる。`ADR-0066` の Y-b）である。

## Decision

1. **eventfd を足す**（`eventfd2` 290 と `eventfd` 284）。値は `u64` で、`write` は足し、`read` は返して 0 に戻す
   （`EFD_SEMAPHORE` なら 1 ずつ）。最大は `2^64 - 2` で、越える `write` は待つか `-EAGAIN`、`2^64 - 1` を書けば `-EINVAL`
   （Linux と同じ）。表は 8 個で、尽きたら `-ENOMEM`。

2. **timerfd を足す**（`timerfd_create` 283、`timerfd_settime` 286、`timerfd_gettime` 287）。時計は `CLOCK_MONOTONIC`
   だけを受ける。`TFD_TIMER_ABSTIME` も受ける。表は 8 個。

3. **締切はタイマのティックで持ち、切り上げる**（100 Hz なので 10 ms）。16 ms を頼まれた締切は 20 ms になる。
   - 絶対時刻（`TFD_TIMER_ABSTIME`）は、`clock_gettime(CLOCK_MONOTONIC)` と同じティックを起点にする。切り上げるので、
     `clock_gettime` で見れば、頼んだ時刻より前に満了することは無い。
   - 相対の締切は、実時間で見ると最大 1 ティック（10 ms）未満だけ早く満了しうる。今のティックの途中から数えるため
     である（`nanosleep` と同じ）。
   - 満了は、読むときと問い合わせるときに今のティックで決める。**ティックの割り込みの道には何も足さない**——待つ側が
     締切を待ちの集合の `Wait::Timer` に入れ、既存の `wake_expired_timers` が起こす。

4. **epoll を足す**（`epoll_create1` 291、`epoll_create` 213、`epoll_ctl` 233、`epoll_pwait` 281、`epoll_wait` 232）。
   表は epoll 8 個、1 つに項目 16 個。`struct epoll_event` は x86_64 では packed の 12 バイトである。
   - **項目は「オブジェクト」と「登録した fd の番号」の組で持つ。** 準備は fd の番号を引き直さず、オブジェクトを直に
     問い合わせる。閉じた後に同じ番号で別のものが開いても、それを見張らない。
   - **オブジェクトを手放すときに、そのオブジェクトを指す項目を全部の epoll から外す。** 参照を数える種類（eventfd・
     timerfd・epoll・入力の fd）は最後のコピーを閉じたときで、Linux の「最後の参照が閉じたら黙って外れる」と同じである。
   - 見張れるのは、listen のソケット・繋がったソケット・パイプの両端・eventfd・timerfd・epoll・入力の fd である。普通の
     ファイル・端末・共有メモリー・画面・繋がっていないソケットは `-EPERM`。

5. **レベルトリガーと `EPOLLONESHOT` だけを持つ。`EPOLLET` は断る**（名前のある行 `epollet-refused:` を出して
   `-EINVAL`）。ONESHOT の項目は、一度返したら止まり、`EPOLL_CTL_MOD` で付け直すまで返さない。`EPOLLERR` と `EPOLLHUP` は
   求められていなくても返す。`EPOLLEXCLUSIVE` と `EPOLLWAKEUP` は受けて見ない。

6. **入れ子を許す。** 自分自身を足すのは `-EINVAL`、輪になる足し方と、鎖が epoll 5 個を越える足し方は `-ELOOP`。
   5 個は WSL で測った（5 個の鎖は通り、6 個目を作る足しは、上から足しても下から足しても `ELOOP`）。入れ子の epoll は、
   中に返すものがあれば `EPOLLIN` を返す。

7. **待ちは、どの epoll かを区別せずに全部起こす。** 待ちの理由 `Wait::Epoll` は番号を持たない。ソケット・パイプ・
   eventfd・timerfd の状態が変わる所と、epoll の項目の足し・付け直しで、epoll で待つ者を全部起こす。起こされても
   返すものが無いことはあり、起きた側が問い合わせ直す（`ADR-0061`）。timerfd の締切は同じ集合の `Wait::Timer` が、
   入力は `Wait::Keyboard` が受け持つ。
   - **締切に入れるのは、`EPOLLIN` を求めている項目だけである。** timerfd・入力・入れ子の epoll は `EPOLLIN` しか返さない
     ので、求めていない項目（polling は timerfd を `events` 0 で足すことがある）の締切で起きても、返すものが無い。過ぎた
     締切を入れると、ティックごとに空回りする。
   - **問い合わせから待ちを据えるまで BKL を放さない。** 状態を変える側はどれも BKL の内側で変えるので、問い合わせより
     前に変われば問い合わせが見つけ、待ちを据えた後なら変える側が起こす。取りこぼしは無い。

8. **1 回の `epoll_pwait` で返すのは 32 件まで。** `maxevents` がそれより大きくても 32 件で返す。レベルトリガーなので、
   残りは次の呼び出しで返る。`maxevents` が 0 以下なら `-EINVAL`。`timeout` は負なら待ち続け、0 なら待たず、正なら
   ティックに切り上げた締切まで待つ。

9. **`events` の書き先は、返す直前に確かめる。** 確かめ・集め直し・書きの間は BKL を放さない。書けなければ、ONESHOT の
   項目も止めずに `-EFAULT` を返す（数えるときは何も変えず、確かめた後の集め直しで ONESHOT を止める）。eventfd と
   timerfd の `read` も同じ形にした（値を取る直前に確かめる）。

10. **`epoll_pwait` の sigmask は受けて無視する。** シグナルを配送しないので、マスクを換えても道は変わらない。ただし
    sigmask が NULL でなく `sigsetsize` が 8 でなければ `-EINVAL`（Linux と同じ）。

11. **入力の fd は、開いた回ごとに番号を持つ**（`File::Input { open }`。参照を数える）。キーボードは 1 つだが、番号が
    無いと、ある者が入力の fd を閉じたときに、ほかの者の epoll の入力の項目まで外してしまう（コンポジタにキーが
    届かなくなる）。`open_input` で開くごとに番号を取り、`F_DUPFD` のコピーは同じ番号の参照を足す。表は 8 個で、
    尽きたら `-ENOMEM`。

## Alternatives Considered

- **`EPOLLET` も持つ**——却下。Seinas の道（polling は ONESHOT を使う）は打たない。エッジを正しく出すには、オブジェクト
  ごとに「前に返してから変わったか」を持つ必要があり、黙って受けてレベルで返すと、使う側が読み残しで止まる。断って
  名前のある行を出せば、打つプログラムが現れたときにすぐ分かる。
- **epoll ごとに起こす**（待ちの理由に epoll の番号を持たせ、オブジェクトから自分を見張る epoll を引き、入れ子の親まで
  辿って起こす）——今は採らない。epoll は 8 個、待つ者は数本で、全部起こしても費用は小さい。取りこぼしの無さを
  確かめるのも簡単である。待つ者が増えて空振りが目に見えるようになったら見直す。
- **入力の fd の数だけを数え、最後の 1 つを閉じたときに入力の項目を外す**——却下。番号を持たせる形より小さいが、
  自分の入力の fd を閉じても、ほかの者が開いている間は自分の項目が残る。残った項目は、もう読めないキーで
  レベルトリガーのまま返り続け、待つ側が空回りする。
- **ティックの割り込みで timerfd の満了を数える**——却下。割り込みの道に表の走査を足すことになる。読むときと問い合わせる
  ときに今のティックで決めれば足り、起こすのは既存の `Wait::Timer` で済む。

## Consequences

- 通る一連: polling の `Poller::new`（epoll・eventfd・timerfd を作って足す）と `wait`（`timerfd_settime` → `EPOLL_CTL_MOD`
  → `epoll_pwait`）、wayland-backend の epoll を複製して calloop の epoll へ入れる形。
- 検査: Linux 向けの確かめのイメージ `wl-epoll-rust` を 1 本足し、Linux の参照と突き合わせる（`cargo xtask run
  --linux-socket-test`）。待つ経路は時間ではなく数で見る——カーネルが出す `epoll-waits:` の行の、待ちの数と、待ちが
  ソケット・eventfd・timerfd・入れ子の epoll のそれぞれで終わった数が 1 以上であること。破壊テストは 5 件
  （`epoll-ignores-oneshot`・`timerfd-ignores-deadline`・`eventfd-write-does-not-wake-epoll`・`epoll-nested-never-ready`・
  `epoll-level-reported-once`）で、どれもイメージが自分で違いを見つけ、名指しの行を出して終わる。
- ロックの順: `epoll` の表のロックを持ったまま、ソケットの接続の欄・パイプの欄・eventfd の表・timerfd の表・
  キーボードの輪を取る（問い合わせ）。逆の順は無い——それらの側は、自分のロックを放してから `epoll::notify` と
  `epoll::forget` を呼ぶ。

既知の Linux との差と限界:

- **待っている間にプロセスが終わったら、起きた後で `-EINTR` を返す**（戻りの道でスレッドが終わるので、Ring 3 へは
  届かない）。ただし、プロセスの終わりの印が epoll の待ちを起こす形は無い（起こすのは futex の待ちだけ）。ほかの理由で
  起きたときに気づく。起こす側は別の課題で足す。
- **ソケットとパイプの端は参照を数えていない**（`ADR-0064` の 2026-10-19 の Addendum の既知の不具合）。そのため、
  `F_DUPFD` でコピーした端は、コピーのどれかを閉じたときに epoll の項目から外れる（Linux は最後のコピーを閉じたとき）。
- `timerfd_gettime` は、締切を過ぎてまだ読まれていなければ残りを 0 で返す（Linux は次の満了までを返す）。間隔のある
  timerfd を読まずに放っておく道は Seinas に無い。
- パイプの読み端は、書き手が居ないだけでは `EPOLLIN` を立てず `EPOLLHUP` だけを返す（Linux の `pipe_poll` と同じ）。
  繋がったソケットは、相手が閉じたら `EPOLLIN|EPOLLRDHUP|EPOLLHUP` を返す（Linux の `unix_poll` と同じ）。
- 時計は `CLOCK_MONOTONIC` だけで、`CLOCK_REALTIME` と `CLOCK_BOOTTIME`、`TFD_TIMER_CANCEL_ON_SET` は `-EINVAL`。
- `ppoll` と、`poll` の `POLLOUT`・`POLLERR` は、この ADR の範囲に入れていない（別の課題）。
