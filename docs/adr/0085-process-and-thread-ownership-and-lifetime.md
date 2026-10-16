# 0085. プロセスとスレッドの所有と寿命（M3aのスレッド）

## Status

Accepted（2026-10-13に運用者が案a〔主スレッドがプロセスを持ち、終わりにほかのスレッドを待つ〕を決めた。池の数・tidの決まり・「全部BSP」の一覧の扱いは、同じ日に追記した上で承認）。

## Date

2026-10-13

## Context

SeinasはSmithay（0.7.0）の上のWaylandコンポジタで、musl（1.2.5）で静的にリンクしたRust（1.97.1）のプログラムである。Smithayは、クライアントのpoolを捨てるときに後始末のスレッドを作る。そのスレッドは終わらずにfutexで待ち続け、プロセスは`exit_group`で終わる。つまり、Seinasを動かすには、スレッドを作ること（`clone`）、futexで本当に待って起こされること、スレッドが残ったまま`exit_group`でプロセスが終わること、の3つが要る。

今のZeikOSは1プロセス1スレッドである（`docs/roadmap.md`の2026-10-07の現在地）。

- `clone`は番号が無く、`-ENOSYS`で断る。
- `FUTEX_WAIT`は、値が一致して本当に待つ場面になると、起こす者が居ないのでプロセスを137で終わらせる（`kernel/src/syscall.rs`の`sys_futex`）。
- `exit_group`は`exit`と同じに扱う。
- `set_tid_address`・`rt_sigaction`・`rt_sigprocmask`・`sigaltstack`は登録するだけである（`ADR-0081`。シグナルは配送しない）。`getpid`と`gettid`は定数1を返す。

今のプロセスの形は、タスクの上に入れ子で乗る「遠征」である（`ADR-0022`。`kernel/src/arch/x86_64/ring3.rs`の`run_excursion`）。状態の置き場は4通りある。

- タスクごと（`kernel/src/task.rs`の`Task`。6つ）——カーネルスタック、保存したRSP、状態と待つ理由の集合、TSSへ載せるRSP0の値、遠征の深さ、CR3のコピー、回復点のコピー、担当のCPU（`owner`）。FP・FSベース・GSベースの保存域（`USER_REGISTER_AREAS`）もタスクごとである。
- スロットごと（`USER_TASK_SLOTS`は2。スロット0は普通のタスク、スロット1は切り離したタスク）——`ExcursionState`（深さ、Ring 3に居るか、畳んだときの記録）、`SyscallState`（ユーザーのウィンドウ、終了の状態、futexの記録、診断の記録）、`CURRENT_FILES`、`CURRENT_HEAP`。
- スロットと深さごと（`MAX_EXCURSION_DEPTH`は2。シェルが深さ1、その子が深さ2）——`ProcessState`（sigactionの表、シグナルのマスク、代替スタック、`tid_address`、実行ファイルの名前）、`MemoryMap`（マッピングテーブル）、遠征のスタック、回復点、`spawn`の引数のコピー。
- 読み込んだ側のスタックフレームの上（`kernel/src/userland.rs`の`UserProcess`）——アドレス空間、入口、イメージの底、ユーザースタックの上端、ヒープ、fdの表、走ったCPUの印（`ran_on`）。走っている間だけ、スロットごとの欄へ入れ替える。

ユーザーのタスクはすべてBSPの上で走る。`Task::owner`の既定はBSPで、APの担当に書き換えるのは`init_ap_idle_task`だけである。`pick_next`は担当の違うタスクを選ばない。この前提に依っている所は、下の「「全部BSP」の前提に依っている所」に並べた。

BKLを手放して眠る所には、決まりが1つある——借りる前（眠る前）に読んだプロセスの状態を、起きた後に使わない（`ADR-0084`。`kernel/src/syscall.rs`の`borrow_allocator`のdoc）。今は1プロセス1スレッドなので害が無いが、スレッドが入ると、眠っている間に同じプロセスの別のスレッドがマッピングを変えうる。この決まりは、スレッドの設計の前提である。

## Decision

### 1. 共有するものと、スレッドごとのもの

**プロセスが持ち、スレッドが共有するもの。** アドレス空間とその管理（CR3の根、マッピングテーブル、ヒープ、`ran_on`、破棄の会計）、fdの表、ファイルシステムの状態（実行ファイルの名前。cwdは今は無い）、シグナルの登録（sigactionの表）、終了の状態、プロセスの番号。

**スレッドごとのもの。** 実行の状態（カーネルスタック、保存したRSP、状態と待つ理由、RSP0の値、回復点、Ring 3に居るか、畳んだときの記録）、TLS（FSベース・GSベース）、FP・SIMDの状態、シグナルのマスク、代替スタック、`tid_address`、スレッドの番号（tid）。ユーザースタックは、`clone`を呼ぶ側（musl）が`mmap`で作って渡す。カーネルはアドレスを受け取るだけである。

Linuxと同じ分け方である。今の置き場からの移し方は、下の「欄の対応表」にある。

### 2. スレッドは、固定の池から取るタスクである

- スレッドは1つのタスクである。入れ子の遠征ではない（入れ子は呼んだ側へ戻るまで戻れないので、交互に走るスレッドにならない）。
- スレッドのタスクは、固定の数の池から取る（`TASK_COUNT`へ足す）。**池はシステム全体で4つで、プロセスごとではない**（Seinasの調査で見つかったのは後始末のスレッド1つである）。**数は定数1つ（`THREAD_TASKS`）で変えられる形にする。**各タスクは自分のカーネルスタック（64 KiB。番兵のページつき。`RING3_TASK_STACK`と同じ形）を持ち、それがRing 3に居る間のRSP0の先になる。
- **池が尽きたら、`clone`は`-EAGAIN`を返し、名前付きの行（`thread:`）をシリアルへ出す**（Linuxの`EAGAIN`は、スレッドの数の上限に当たったときの値である。`ADR-0084`の`spawn`と同じ）。
- スレッドのタスクは、作ったスレッドのプロセス（スロットと深さ）に属する。スロットはタスクの番号から決める今の形（`ring3_slot_of`）をやめ、`Task`の欄で持つ。スロットごとの欄（`CURRENT_FILES`・`CURRENT_HEAP`・`MemoryMap`・`ProcessState`のプロセス側）は、同じスロットのスレッドが共有する。
- `clone`で受けるフラグは、muslの`pthread_create`が使う組（`CLONE_VM`・`CLONE_FS`・`CLONE_FILES`・`CLONE_SIGHAND`・`CLONE_THREAD`・`CLONE_SYSVSEM`・`CLONE_SETTLS`・`CLONE_PARENT_SETTID`・`CLONE_CHILD_CLEARTID`、必要なら`CLONE_DETACHED`）だけである。それ以外の組（`fork`の形）は`-ENOSYS`で断る（`fork`は今までどおり無い。`ADR-0074`）。
- 新しいスレッドは、親と同じアドレス（`clone`の戻り）から、渡されたスタックとTLSで、RAX=0でRing 3へ入る。FPの状態は親のコピーである（Linuxと同じ）。
- `clone`が失敗する（池が空、スタックやTLSのアドレスがウィンドウの外）ときは、共有の資源に触れる前に断る（`-EAGAIN`か`-EINVAL`）。巻き戻すものは無い。

### 3. 寿命

- **プロセスの共有の資源は、最後のスレッドが終わるまで残す。** 共有の資源を持つのは、今までどおり主スレッド（プロセスを読み込んだ遠征）のスタックフレームである。主スレッドの終わりの処理は、ほかのスレッドが全部`Finished`になるまで、BKLを手放して譲りながら待ってから、今までどおり畳んで破棄する。
- **この形（案a）は、`fork`を入れる段で案b（プロセスをオブジェクトにし、スレッドの数で寿命を決め、最後のスレッドが破棄する）へ移る前提である。** 移る条件は、次のどれかが先に来たときである——`fork`を入れるとき（親と子が同じイメージを別々の寿命で持つので、スタックフレームの上では表せない）、主スレッドより長く生きるスレッドが要る利用者が出たとき（主スレッドの`exit`が待つ形では足りない）、プロセスの数が遠征の深さ（`MAX_EXCURSION_DEPTH`）を越えるとき。移すものは「欄の対応表」の「プロセス（そのまま）」の行で、`UserProcess`と`spawn`の入れ子の退避（`Records`・`FoldRecord`）が中心である。
- **スレッドの`exit`は、そのスレッドだけを終える。** `Finished`にして、`CLONE_CHILD_CLEARTID`なら`tid_address`に0を書いてそのアドレスを1つ起こす（muslの`pthread_join`はこれを待つ）。カーネルスタックは、刈り取ったときに池へ戻す。
- **`exit_group`は、どのスレッドから呼ばれても、プロセスを終える。** プロセスに「終わる」印と終了の状態を据え、全部のスレッドを起こす。印の付いたスレッドは、次にカーネルへ入ったとき（システムコールかティック）に戻らずに終わる。主スレッドは、上の順で待ってから畳む。Ring 3で走っていたスレッドは、最長で1ティック（10 ms）だけ走り続けうる（Linuxはすぐ止める。差として記録する）。
- **主スレッドの`exit`は、ほかのスレッドが残っていれば、それらが終わるまで待ってから終える**（Linuxはリーダーをゾンビで残すが、M3aではこの形にする。muslの`pthread_exit`を主スレッドで呼んだときだけ通る道である）。
- **スレッドを持つプロセスは、M3aの間は入れ子の`spawn`を呼べない**（`-EAGAIN`）。入れ子の子は同じスロットの欄を入れ替えて走るので、残ったスレッドが子の表を見てしまう。Seinasは子を起こさない。切り離した`spawn`（スロット1）はこの制約に当たらない。

### 4. BKLを持たない区間

- futexで待つのは、`ADR-0036`の順である——待つ理由（`Wait::Futex`。プロセスとアドレスの組）を書き、BKLを解き、譲る。起こされたら取り直す。空振りで起こしてよい（`ADR-0061`。起きたら値を見直す）。
- **眠る前に読んだプロセスの状態を、起きた後に使わない**（`ADR-0084`の決まりを、futexの待ちにも当てる）。futexの値の比べは、BKLを持ったまま、待つ理由を書く直前に行う。「空だと見てから`Waiting`にする」までに起こしが入らないことは、システムコールの入口がIF=0で入ることが守る（`kernel/src/syscall.rs`の`sys_read`と同じ形）。
- 「終わる」印は、BKLを取り直した直後に見る。印が付いていれば、Ring 3へ戻らずに終わる。
- futexの鍵は、プロセス（スロットと深さ）とユーザーのアドレスの組である。プロセスをまたぐfutex（共有メモリの上のもの）はM3aでは扱わない。`FUTEX_PRIVATE_FLAG`の有無は見ない（差として記録する）。

### 5. 同じプロセスのスレッドは、1つのCPUに寄せる（期限付き）

- スレッドのタスクの`owner`は、作ったスレッドの`owner`と同じにする。今はBSPなので、全部のスレッドがBSPで走る。
- これは「2 CPU以上」の段（`ADR-0074`の順の6）までの制約である。その段で、次の2つを入れてから外す。
  - ユーザーのアドレス空間のTLBシュートダウン（`munmap`・`mprotect`・`brk`の縮小が、ほかのCPUの翻訳を落とす。`docs/deferred-decisions.md`の「`munmap`のその場返しを、走ったCPUの印で分ける形に揃える」と、TLBシュートダウンの項）。
  - 別のCPUで走るスレッドを止める処理（`exit_group`で、ほかのCPUのスレッドをすぐ止める。IPI）。
- 寄せている間は、`ran_on`は1ビットのままで、破棄はその場で返す道を通る。今の`invlpg`だけの無効化も正しいままである。

### 6. マッピング中の共有メモリを縮める操作は、M3aの間は断る（期限付き）

- 2回目の`ftruncate`は、今までどおり`-EINVAL`で断る（`ADR-0065`。大きくする向きも小さくする向きも）。Linuxは縮められ、縮めた先のマッピングに触るとSIGBUSになる。
- 期限はM3bである（マッピングテーブルから該当のページを外し、触ったときにSIGBUSを配送する形で、正しく実装する）。

### 7. 「全部BSP」の前提は、M3aでは保つ

- 第5項で、スレッドも含めて全部のユーザーのタスクがBSPで走るので、前提は崩れない。前提に依っている所（下の一覧）は、M3aでは手を入れない。
- **一覧はこのADRに残す。** それぞれの「2 CPU」の列の扱いは、「2 CPU以上」の段で行う（その段の設計で、この一覧を起点にする）。M3aでは、一覧の「M3a」の列だけを行う。
- 一覧の中で、M3aで触るものが2つある。`CURRENT_RECOVERY`の入れ替え（スレッドのタスクも回復点を持つので、切り替えで入れ替える。形は今と同じ）と、`ring3_task_main`のBKLの外の書き（スレッドの終わりでも同じ形の書きをするので、BKLを持って書く形に揃える）。

### 8. 番号

- **tidは、プロセスの中で一意で、主スレッド以外は1にならない。** 主スレッドは今までどおり1（`getpid`と同じ）。スレッドのtidは、池の番号と、その池のタスクの世代（`RING3_TASK_GENERATION`と同じ形で、刈り取るたびに1つ進む）から作る——`2 + 池の番号 + THREAD_TASKS × 世代`。`gettid`はそれを返し、`tkill`は自分のプロセスのtidだけを受ける。`CLONE_PARENT_SETTID`と`CLONE_CHILD_SETTID`はこの値を書く。
- **終わったスレッドのtidは使い回さない。** 刈り取って池へ戻すと世代が進むので、同じ池のタスクを次に取ったスレッドは別のtidになる（muslは`pthread_join`の後もtidを見ることがあるので、新しいスレッドと取り違えない）。値は`pid_t`（32ビット）に収まる範囲で増え、1秒に1つ作っても17年は越えない。越えたら止まる（`[ERROR]`の行）。
- **`getpid`は、どのプロセスでも1である。** これは既存の制限で（切り離したプロセスも1を返す）、M3aでは変えない。プロセスの番号を分けるのは、`fork`を入れる段で決める。

### 欄の対応表

今の置き場から、プロセス側とスレッド側のどちらへ行くか。「そのまま」は、M3aで置き場を変えないという意味である。

| 今の置き場 | 欄 | 行き先 | 備考 |
|---|---|---|---|
| `Task` | `saved_stack_pointer`・`stack_top`・`stack_bottom`・`state`・`kernel_entry_stack_top`・`excursion_depth`・`current_recovery`・`owner` | スレッド（そのまま） | スレッドのタスクも同じ欄を持つ。`excursion_depth`はスレッドでは固定 |
| `Task` | `page_table_root` | スレッド（コピー） | プロセスの値のコピー。`forget_page_table_root_if`は全タスクを見るので、そのまま効く |
| `Task` | （新）`process` | スレッド | 属するプロセス（スロットと深さ）。`ring3_slot_of`を置き換える |
| `Task` | （新）`tid` | スレッド | 第8項 |
| `USER_REGISTER_AREAS` | `fp`・`fs_base`・`gs_base` | スレッド（そのまま） | doc（`user_registers.rs`）が既にそう書いている |
| `ExcursionState`［スロット］ | `depth`・`in_ring3`・`folded`・`interrupted`・`fault_*` | スレッド | タスクごとへ移す。主スレッドの`depth`は今の意味のまま |
| `EXCURSION_STACKS`・`RECOVERIES`［スロット］［深さ］ | （全部） | スレッド | スレッドのタスクは自分のカーネルスタックと回復点を1つ持つ |
| `SyscallState`［スロット］ | `user_window_start`・`user_window_end` | プロセス（そのまま） | ウィンドウはアドレス空間のもの |
| `SyscallState`［スロット］ | `process_exited`・`process_exit_status` | プロセス（そのまま） | 「終わる」印を足す（第3項）。`exit`はスレッドの終わりへ分ける |
| `SyscallState`［スロット］ | `futex_deadlock_address` | プロセス（そのまま） | 本当に待つ形にしても、眠ると誰も起こせない行き詰まりの検出に使うので残す（`ADR-0081`の2026-10-14のAddendum。当初は「消す」としていた） |
| `SyscallState`［スロット］ | `handler_sp`・`in_ring3_at_entry`・`last_*`・`write_*`ほか診断の記録 | プロセス（そのまま） | 「最後に見た」の記録。スレッドで分ける必要が出たら分ける。`handler_sp`は主スレッドの入口だけが書く（3-1b。スレッドの入口も書くと、最後に入った側で`spawn:`の行が揺れた） |
| `ProcessState`［スロット］［深さ］ | `actions`・`exec_name`・`exec_name_len` | プロセス（そのまま） | |
| `ProcessState`［スロット］［深さ］ | `blocked`・`alt_stack`・`tid_address` | スレッド | Linuxではスレッドごと。シグナルのマスクと代替スタックの課題で移す |
| `MemoryMap`［スロット］［深さ］ | （全部） | プロセス（そのまま） | |
| `CURRENT_FILES`・`CURRENT_HEAP`［スロット］ | （全部） | プロセス（そのまま） | 同じスロットのスレッドが共有する。第3項の入れ子の`spawn`の制約の理由 |
| `IOV_COPIES`［スロット］ | （全部） | 確かめる | `writev`がパイプで待つなら、スレッドごとか、眠る前にコピーし終える形が要る |
| `UserProcess`（読み込んだ側のフレーム） | `space`・`entry`・`image_base`・`stack_top`・`heap`・`kind`・`ran_on`・`name`・`files` | プロセス（そのまま） | 主スレッドのフレームに置いたまま。`stack_top`は主スレッドのユーザースタックの上端で、スレッドのスタックはmuslが持つ |
| `SPAWN_*`［スロット］［深さ］・`INHERITED_ENDS`・`PENDING_STDIN`［スロット］ | （全部） | プロセス（そのまま） | 第3項の制約で、スレッドからは触らない |
| `DETACHED_REQUEST`・`DETACHED_STATUS`・`RING3_TASK_GENERATION` | （全部） | そのまま | 切り離したタスクの仕組み。スレッドとは別 |
| （新） | futexの待ち行列 | プロセス | `Wait::Futex`を`WaitSet`へ足す。鍵はプロセスとアドレス |

### 「全部BSP」の前提に依っている所と扱い

ソースのdocと`SAFETY`と、コードの形から拾った（2026-10-13）。「M3a」はスレッドを入れる段、「2 CPU」は「2 CPU以上」の段での扱いである。

| 場所 | 依っていること | M3a | 2 CPU |
|---|---|---|---|
| `kernel/src/console/mod.rs`の`present_fb0_whole`のdoc | 閉じる経路の転送がBKLの外で走っても、同じCPUなので重ならない | そのまま | 転送をBKLの中へ戻すか、転送の排他をatomicの印にする |
| `kernel/src/console/mod.rs`の`FOREGROUND`のdoc | APは画面へ書かない | そのまま | `write`の直列化をBKLで言い直す（既にBKLの下） |
| `kernel/src/arch/x86_64/ring3.rs`の`run_excursion`のdoc | 呼ぶのはBSPだけ。`CURRENT_RECOVERY`・RSP0・ウィンドウの書き手が1つ | そのまま（スレッドは遠征を始めない） | 書き手が2つになる欄を洗う |
| `kernel/src/arch/x86_64/ring3.rs`の`CURRENT_RECOVERY` | 大域に1つで、切り替えで入れ替える | スレッドのタスクも入れ替える（形は同じ） | CPUごとにする（アセンブラが`[rip + sym]`で読むので、置き方を変える） |
| `kernel/src/bkl.rs`の`NON_ACQUIRING_ENTRIES`の`exception`と`UNWINDLESS_RELEASE_ENTRIES` | 畳む経路と`SYS_EXIT`の解いた後の区間が、1つのCPUに閉じている | そのまま | 畳む経路でBKLを取るか、触る欄をCPUごとにする |
| `kernel/src/bkl.rs`の`TLB_GENERATION`のdoc（ふるいの例外）と`note_mapping_changed`のdoc | 畳んで戻る所がflushしなくてよいのは、遠征がAPより前だから。`note_mapping_changed`に本番の呼び手が無い（古い。`quarantine.rs`が呼ぶ） | docを直す（M3aの課題の中で） | `munmap`・`mprotect`・`brk`の縮小も世代を上げる |
| `kernel/src/task/scheduler.rs`の`kernel_entry_stack_top`・`excursion_depth`・`page_table_root`の`SAFETY`と`task.rs`の`switch_page_table_root_and_note` | タスクはBSPだけが持つので、同じコアの割り込みを止めれば切り替えと重ならない | そのまま | `init`の深さ0の書きもBKLの下へ |
| `kernel/src/syscall.rs`の`mprotect_from_ring3`・`release_range_and_unmap`、`paging/active.rs`の`unmap_4kib` | `invlpg`はこのCPUだけ。フレームはすぐ返す | そのまま（第5項） | TLBシュートダウン。返すのは隔離を通す |
| `kernel/src/task.rs`の`finish_threads_of_slot`と`reap_thread_task`（2026-10-14。3段目） | プロセスの終わりが、残ったスレッドのタスクを`Finished`にして刈り取る間、別のCPUがそのタスクを走らせることも刈り取ることも無い（走っているのは主スレッドだけ） | そのまま | 刈り取りをBKLの中で「どのCPUの`CURRENT`でもない」ことを見てから行うか、別のCPUのスレッドを止めてから刈り取る（IPI） |
| `kernel/src/quarantine.rs`の`Retire`と`userland.rs`の`ran_on`・`destroy_route` | 走ったCPUの印で、その場で返すかを決める | スレッドがRing 3へ入る所でも印を立てる | そのまま（既に複数CPUの形） |
| `kernel/src/task.rs`の`schedule_switch`（`CURRENT_RECOVERY`の入れ替え） | 切り替えるCPUが1つ | スレッドにも同じ入れ替え | CPUごとの回復点 |
| `kernel/src/task.rs`の`ring3_task_main` | `Finished`への書きと起こしが、BKLの外（IF=1） | BKLを持って書く形に揃える（スレッドの終わりと同じ形にするため） | そのまま |
| `kernel/src/task.rs`の`pick_next`（2026-10-13） | 状態を添字から1本ずつ読み、メインを2度読む。読む間に別のCPUが状態を書かない（状態を書くのはBSPのシステムコールと割り込みの入口と足した1本の終わりで、APはBSPのタスクの状態を書かない） | そのまま | 輪で読んだメインの値を落ち先の判定に使い回す |
| `kernel/src/task.rs`の`bsp_idle_main` | 隔離の解放・shmの解放・間隔ごとの転送はBSPのアイドルだけが行う | そのまま | APのアイドルにも回すか、BSPに任せたまま遅れを受け入れる |
| `kernel/src/arch/x86_64/idt/mod.rs`の`advance_monotonic_ticks`と`interrupts.rs`の`wake_expired_timers` | 時計を進めて期限の待ちを起こすのはBSPのティック | そのまま | APのタスクは1ティック遅れて拾われる（壊れない） |
| `kernel/src/interrupts.rs`の`should_fold_excursion` | Ctrl+Cの要求は大域に1つ | そのまま | 前景のタスクへ向ける |
| `kernel/src/syscall.rs`の`flush_root_image` | 前景のRing 3のタスクは1つで、APはユーザーを走らせない（virtioが断らない） | そのまま | `-EBUSY`が返りうる（壊れない） |
| `kernel/src/task.rs`の`ring3_slot_of`と`USER_TASK_SLOTS` | スロットはタスクの番号から決まる | `Task`の欄へ（第2項） | そのまま |
| `kernel/src/main.rs`の`run_init`のdoc、`task/scheduler.rs`のモジュールのdoc | APは画面へ書かない。`switches`・`resumes`は単一コアなのでvolatileでよい | そのまま | docを直す |
| `kernel/src/task.rs`の`require_bootstrap_processor`（`GPR_BUF`） | デモのワーカーだけ | そのまま | そのまま |

### 後の課題の順と大きさ

M3aの実装は、次の順で分ける。大きさは、1つの課題でfull checkを何回通すかの見立てである。

1. **`clone`**（大。full check 2〜3回）——スレッドのタスクの池、`Task`の`process`と`tid`、スロットの決め方の置き換え、`ExcursionState`と回復点のタスクごとへの移し、`clone`の入口、スレッドの`exit`（最小。`CLONE_CHILD_CLEARTID`の書きと起こし）、`ran_on`の印、第3項の入れ子の`spawn`の制約。`ring3_task_main`の書きの形の揃え。
2. **futex**（中。1〜2回）——`Wait::Futex`、`FUTEX_WAIT`と`FUTEX_WAKE`（必要なら`WAIT_BITSET`・`WAKE_BITSET`・期限つきの待ち）、137で終わらせる形の撤去。`pthread_join`はこれで通る。
3. **終わり方**（中。1〜2回）——`exit_group`の「終わる」印と全部の起こし、主スレッドの待ち、主スレッドの`exit`の扱い、終了の状態の決め方、刈り取りと池への返し。
4. **シグナルのマスクと代替スタック**（小。1回）——`blocked`・`alt_stack`・`tid_address`をスレッドごとへ。配送はしない（`ADR-0081`のまま）。
5. **マッピング中の共有メモリの縮小を断ること**（小。1回。順は問わない）——第6項をdocと確かめの項目にする（2回目の`ftruncate`が`-EINVAL`で、マッピングが残ること）。
6. **検証**（中。1〜2回）——スレッドを作って待って起こして`exit_group`で終わるmuslのプログラムを`linux-programs/`へ足し、起動ログの参照と確かめの項目にする。破壊テストは、少なくとも、fdの表を共有しない形、`FUTEX_WAKE`が待ち手を見ない形、`exit_group`がスレッドを残す形、スレッドのタスクをAPの担当にする形（第5項の確かめ）。「全部BSP」の単体試験（`the_application_processor_does_not_choose_the_ring3_task`）をスレッドのタスクにも広げる。

## Alternatives Considered

- **スレッドも「2 CPU以上」と一緒に入れ、最初からAPで走らせる**——採らない。TLBシュートダウン、別のCPUのスレッドを止める処理、`CURRENT_RECOVERY`のCPUごと化、APでのFS・GSの保存の確かめ（`docs/deferred-decisions.md`）が全部先に要り、Seinasが動くまでが長い。`ADR-0074`の順（スレッドが5、2 CPU以上が6）のとおり、分けて入れる。
- **スレッドを使わない形でSeinasを動かす（`clone`を`-ENOSYS`のまま、ユーザー側でスレッドを避ける）**——採らない。muslとRustの標準ライブラリは`clone`を直に呼ぶ。Smithayの後始末のスレッドはライブラリの中に在り、Seinas側で避けられない。
- **プロセスをオブジェクトにして、スレッドの数で寿命を決め、最後のスレッドが破棄する（Linuxの形）**——M3aでは採らない。`UserProcess`は読み込んだ側のスタックフレームの上に在り、`spawn`の入れ子（`Records`・`FoldRecord`の退避と復元）がその形に依っている。動かすと`spawn`の経路の書き直しになる。主スレッドが待ってから破棄する形で、「最後のスレッドが終わるまで残す」は満たせる。**`fork`を入れる段で、この案へ移る**（条件は決定の第3項）。
- **スレッドのカーネルスタックをフレームのアロケータから取る**——採らない。池の数を固定の静的な配列にするほうが、今のタスクの形（`RING3_TASK_STACK`）と同じで、会計も単純である。数が足りなくなったら増やす。
- **`exit_group`で、Ring 3のスレッドをすぐ止める（切り替えの戻りで印を見る）**——M3aでは採らない。IRQの戻りの経路に判定を足すことになる。1ティックの遅れを差として記録し、要る場面が出たら足す。
- **プロセスをまたぐfutex（共有メモリの上）**——M3aでは採らない。Waylandのクライアントとの同期はソケットで行い、futexはプロセスの中だけで使う。

## Consequences

- Seinasの後始末のスレッドが、futexで待ったまま残り、`exit_group`で一緒に終わる形になる。
- タスクの数が増え（池の分）、`pick_next`が見る範囲が広がる。スレッドは全部BSPなので、APの振る舞いは変わらない。
- Linuxとの差を`docs/deferred-decisions.md`に記録する——`exit_group`の後にRing 3のスレッドが最長1ティック走ること、主スレッドの`exit`がほかのスレッドを待つこと、`FUTEX_PRIVATE_FLAG`を見ないこと、同じプロセスのスレッドが並行に走らないこと（1つのCPUに寄せる。期限は「2 CPU以上」の段）。マッピング中の共有メモリの縮小を断ること（期限はM3b）も、同じ所に記録する。
- 池が尽きた`clone`は`-EAGAIN`で、`thread:`の行で見える。池の数を増やすのは定数1つである。
- 「全部BSP」の前提は、スレッドの`owner`を作った側と同じにすることで保たれる。この決まりを守る単体試験を足す。「2 CPU以上」の段で、上の一覧の「2 CPU」の列を順に入れてから、制約を外す。
- `borrow_allocator`の決まり（眠る前に読んだ状態を使わない）が、futexの待ちにも当たるようになる。BKLを手放す所を足すときは、この決まりを見ること。

## Addendum（2026-10-14。`clone`の3段目「正常の道」で決めたこと）

`clone`（後の課題の順の1）は、池と欄（1段目）、回復点と遠征の状態の置き場（2段目）、`clone`の入口（3段目）に分けて入れた。3段目はさらに、正常の道（入口・スレッドの`exit`・刈り取り）と異常の道（スレッドの中の例外・池のタスクからの`exit_group`・Ctrl+C）に分けた。正常の道で決めたことを書く。

- **池のタスクは、最初の文脈から直にRing 3へ入る（案A）。** `clone`が、親の文脈（`IrqContext`）のコピーにRAX=0・RSP=渡されたスタック・CSとSS=ユーザーのセレクタを入れたものを、池のカーネルスタックの上端に積む。最初の切り替えで、復元経路（15本のpopと`iretq`）がそのままRing 3へ落とす——タイマで止めたRing 3のタスクへ戻るのと同じ経路で、新しいasmは無い。`run_excursion`の枠は無く、回復点も無い（`THREAD_RECOVERIES`の欄は0のまま）。**Ring 3を出る道は切り替えである**——スレッドの`exit`は`Finished`にして`yield_now`で切り替え、二度と戻らない。捨てるカーネルスタックの枠に`Drop`の要るものを残さないことを、docと`debug_assert!`（BKLと`InterruptGuard`を持っていない）で示す。却下した案Bは、足した1本と同じ`run_excursion`の形で、回復点へlongjmpで戻るもの。親の汎用レジスタを積んで`iretq`するasmの別版と、入場のスタックの分け方（同じ64 KiBを分けるか、池ごとに遠征スタックを足すか）が要る。
- **受けるフラグ。** muslの`pthread_create`の組（`CLONE_VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|PARENT_SETTID|CHILD_CLEARTID` = 0x3d0f00）を受け、`CLONE_DETACHED`（0x400000。Linuxが無視するフラグ。muslは付け、glibcは付けない）の有無は見ない。**下位8ビット（`CSIGNAL`）も見ない**——Linuxのlegacyの`clone`は、`CLONE_THREAD`のとき`exit_signal`を-1にして、この8ビットを使わない（`kernel/fork.c`の`copy_process`）。Linuxが`-EINVAL`で断る組（`CLONE_THREAD`に`CLONE_SIGHAND`が無い、`CLONE_SIGHAND`に`CLONE_VM`が無い、など）はそのまま`-EINVAL`で、Linuxでは通るがここに無い組（`fork`の形）は決定2のとおり`-ENOSYS`。アドレスは、スタックがウィンドウの外なら`-EINVAL`、TLSがユーザーの範囲の正準なアドレスでなければ`-EPERM`（`arch_prctl`と同じ）、`parent_tid`と`child_tid`に書けなければ`-EFAULT`。
- **`tid_address`はスレッドごとに持つ**（後の課題の順の4から前倒し）。`CLONE_CHILD_CLEARTID`のアドレスを池のタスクの欄に控え、スレッドの`exit`がそこへ0を書き、そのアドレスで待つ者を1本起こす（`FUTEX_WAKE`と同じ。futexの最小は`ADR-0081`の2026-10-14のAddendum）。池のタスクからの`set_tid_address`も同じ欄に書く。主スレッドのものは今までどおりプロセスの表に控えるだけである。`blocked`と`alt_stack`は順の4のまま（プロセスに1つ。配送が無いので動きは変わらない。`docs/deferred-decisions.md`）。
- **深さの欄は1つの意味にした。** 池のタスクの`Task::excursion_depth`と`ring3`側の`ExcursionState::depth`は、どちらも属するプロセスの深さ（親が`clone`を打ったときの深さ）で、同じ値である。切り替えの検算は、池のタスクなら深さに依らず自分のカーネルスタックを期待する。2段目で「池のタスクの深さは0だけ」としていたのを直した。
- **主スレッドの`exit`に生きているスレッドが在るときは、3段目では`exit_group`と同じく終える**（決定3の「待ってから終える」は、プロセスの終わり方の課題で入れる）。futexの無い間、待たれる側はfutexで137になる道しか無く、意味のある待ちにならない。全部BSP担当で走っているのは主スレッドだけなので、残りのスレッドは欄を`Finished`にして刈り取るだけで止まる。Linuxとの差として`docs/deferred-decisions.md`に書いた。
- **刈り取りは2か所から同じ関数を呼ぶ**——`clone`の入口（`Finished`のまま残っている池のタスクを先に刈り取る）と、プロセスの終わり（空間を壊す前）。刈り取りで世代が進み、池のタスクのカーネルスタックの高水位を`thread:`の行に出す（足した1本と同じ半分の線）。
- **確かめは`linux-programs/m3a-rust.rs`と`linux-programs/m3a-mutex-rust.rs`（スレッドが必ずfutexで待つ形）**——`std::thread::spawn`でスレッドを1つ作り、主スレッドは`sched_yield`を決まった回数（64回）打ってから`join`する（スレッドが先に終わっていれば、muslの`pthread_join`はfutexを打たない）。起動のたびにカーネルが起こし、Linuxで控えた参照と突き合わせる。**`sched_yield`（24）はこのために足した**——BKLを解いて1度譲るだけで、輪の次のタスク（池のスレッド）が選ばれる。それまでは知らない番号で、譲りはティック任せになり、システムコールの数が起動ごとに揺れた（実測。754回の`-ENOSYS`）。
- **スレッドがスレッドを作ること（池のタスクからの`clone`）は許す。** Linuxと同じで、muslもライブラリの中のスレッドから`pthread_create`を呼びうる。子は作った側のプロセス（スロットと深さ）に属し、担当は作った側と同じBSPである（起こすときに確かめる）。3-2で試す項目にする。
- **プロセスの終わりの刈り取りは「全部BSP」に依る**（上の一覧に足した）。`Finished`にしてから刈り取るまでの間に別のCPUがそのタスクを走らせないことを、走っているのが主スレッドだけであることで言っている。「2 CPU以上」の段で見直す。
- **カーネルの中で止まっているスレッドの枠を捨てても安全な理由は、`finish_threads_of_slot`のdocに書いた**——スレッドから届く`yield_now`の呼び手を全部挙げ、どれも譲る前にBKLを解き、枠に`Drop`の要るものを残さない。`InterruptGuard`は`on_yield`の深さの検査が、BKLは池のタスクの譲りの検査（`yield_now`が割り込みを起こす前に見る。`report_thread_yield_with_bkl`）が機械で止める。
- **異常の道（3-1b。2026-10-14）。** 池のタスクは回復点を持たないので、どの道も「スレッドを`Finished`にして切り替え、プロセスの終わりは主スレッドに任せる」形である。
  - スレッドの中の例外（Ring 3の例外）：`record_and_fold`が記録をスレッドの置き場とプロセスのスロットの両方に書き、共通の側（`finish_thread_task_on_fault`）がプロセスを終わらせる印を立て、スレッドを`Finished`にして切り替える。主スレッドは次のシステムコールの戻りで出て、終わり方は主スレッドが同じ例外で終わるときと同じ`Folded(ベクタ)`（畳んだ印を終了の印より先に見る）。例外の入口はBKLを取らないので、この区間だけ`KernelEntry::ThreadFault`として取り、欄を書いたら解いてから切り替える（例外の入口から`yield_now`で切り替える新しい道。捨てる枠に`Drop`の要るものが無いことは、スレッドの`exit`と同じ形でdocと`debug_assert!`で示す）。カーネルの中の例外は今までどおり止まる。
  - 池のタスクからの`exit_group`・`tkill`・`futex`の行き詰まり：`dispatch`が立てた印（`process_exited`と終了の状態）をスロットの表に残し、そのスレッドを`Finished`にして切り替える（`end_thread_after_process_exit`）。主スレッドは次のシステムコールの入口で見て、実行せずに出る（印の後に入った池のスレッドのタスクも同じ）。**印を立てたスレッドは、同じスロットのfutexの待ちをアドレスを問わず全部起こす**（`wake_futex_waiters_of_slot`。例外の道も同じ）——`join`や`Mutex`のfutexで眠っている主スレッドは起きて`futex`の戻りで印を見て出る。起きた側が別のスレッドなら、入口か戻りで印を見て`Finished`になる（印の後に入ったスレッドも、入口で見て何も実行せずに終わる）。パイプ・キーボード・タイマで待っている主スレッドは起こさない（`docs/deferred-decisions.md`）。
  - Ctrl+C：割り込まれたのが池のタスクなら畳まず、フラグも消費しない。主スレッドが次にRing 3で割り込まれたときに畳む。
  - 想定の外（上の道を通らずに`leave_user_mode`へ来る形）は、今までどおり名指しして止まる。
  - **破壊テスト（3-2。2026-10-15）。** `clone`の道（RAX・CLEARTID・TLS・担当）、プロセスの終わり（残るスレッド・futexの待ち・入口の印）、スレッドを持つ間の`spawn`の8件を`--critical-test`に置き、落ち方は`docs/verification-coverage.md`のfeatureの一覧にある。起動のたびのイメージは`m3a-exit-while-spinning-rust`（主スレッドの`exit`の間に回るスレッド）、`m3a-nested-clone-rust`（スレッドからの`clone`）、`m3a-spawn-from-thread-rust`（`spawn`を`-EAGAIN`で断る）、`m3a-exit-group-spin-rust`（入口で印を見る）。
  - 確かめは`linux-programs/m3a-fault-rust.rs`（スレッドがアドレス8へ書く。`Folded(14)`）、`linux-programs/m3a-exit-group-rust.rs`（スレッドが`exit_group(7)`。`Exited(7)`）、`linux-programs/m3a-exit-group-join-rust.rs`（主スレッドが`join`のfutexで眠っている間にスレッドが`exit_group(7)`。起こさなければ起動が止まる）、`linux-programs/m3a-exit-group-waiters-rust.rs`（主スレッドと別のスレッドの2本がfutexで眠っている間に第3のスレッドが`exit_group(7)`）で、起動のたびにカーネルが起こす。

## 参照（2026-10-15）

決定3で持ち越した「主スレッドの`exit`がほかのスレッドを待つ」形は、セルフホスティングの守る条件（Linuxに合わせたプロセスの終わり方）に入る（`ADR-0086`）。

**スレッドの終わり方の課題（M3a）の完了で持ち越しにしたもの**（運用者の決定。2026-10-15。`docs/deferred-decisions.md`の3行）——主スレッドがパイプ・キーボード・タイマで眠っている間はスレッドのプロセスの終わりが起こさない、主スレッドの`exit`はほかのスレッドを待たない、主スレッドの`set_tid_address`のアドレスを終わりに0にしない。どれもC1S4以降の「プロセスの終わり方」の課題で、`execve`・`wait4`と一緒に設計する。
