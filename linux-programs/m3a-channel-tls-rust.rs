//! `m3a-channel-tls-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドと
//! チャネル（`std::sync::mpsc`）で値を受け渡し、`join` で待ち、TLS（`thread_local!`）の値がスレッドの間で混ざらないことを
//! 見る**（スレッドの検証の1。`ADR-0085`）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、TLS の値を 1 にして、チャネルを 2 本作り、4 つの値を送って送り手を閉じてから、ワーカーの
//!    スレッドを作る。
//! 2. ワーカーは最初に「始まった」フラグを立て、`sched_yield` を決まった回数（64 回）打つ。それから TLS の値を 2 にし、チャネルから届く値を送り手が
//!    閉じるまで足し（1+2+3+4 = 10）、合計と自分の TLS の値を返しのチャネルで送る。
//! 3. 主スレッドは「始まった」をシステムコールを打たずに待ってから、返しを受け取る。ワーカーが譲っている間に、返しの `recv` で futex に眠る（`futex:` の行に出る）。起きたら
//!    `sched_yield` を 64 回打ってから `join` し、合計が 10 であることと、TLS が主で 1・ワーカーで 2 のままであることを 1 行
//!    ずつ出す。終了ステータスは引数の数。
//!
//! # 眠る所は決まっている（2026-10-20）
//!
//! **以前は、ワーカーを作ってから値を送り、返しを受け取ってすぐ `join` していた。** そのため、どちらの `recv` が眠るか、
//! `join` が眠るかが、ティックの位置で変わった。システムコールの数は 44・46・47・48・49・51 に散った（残っていた 763 起動）。
//! `clone` の戻りや送っている途中にティックが来ると、ワーカーが空のチャネルで眠り、主スレッドの送りがそれを起こした。
//! 主スレッドが送り終えてから眠るまでの間にティックが来ると、ワーカーが先に返しを送り、主スレッドは眠らなかった。
//!
//! **今は、眠るのは主スレッドの返しの `recv` の 1 回だけである。** 値は作る前に送り終えるので、ワーカーの `recv` は眠らない。
//! ワーカーは何よりも先に 64 回譲るので、ワーカーがいつ走り始めても、主スレッドは返しの `recv` で先に眠る（主スレッドの
//! `recv` は譲らずに眠る）。
//!
//! **主スレッドは、ワーカーの closure が始まるまで malloc に触れない**（2026-10-20）。Rust のスレッドの立ち上げ
//! （closure の前）は malloc のロック（musl の `__malloc_lock`）を取る。主スレッドが `recv` の中で malloc している間
//! （mallocng はロックを持ったまま `mmap` を打つ）にティックが来ると、立ち上げ中のワーカーがそのロックで眠り、主スレッドが
//! 起こした（眠りと起こしが 1 つずつ、システムコールが 2 本多い。250 回の起動で 1 回）。`m3a-exit-group-waiters-rust` で
//! 同じ形を起こした回のアドレスを一時的な計測で見て、`__malloc_lock` だと確かめた。主スレッドは起きた後 64 回譲ってから `join` するので、ワーカーは `exit` まで進み終えている
//! （`m3a-mutex-rust` の「join は眠らない」と同じ形）。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

thread_local! {
    static SLOT: Cell<u32> = const { Cell::new(0) };
}

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-channel-tls-rust: main starts with {argument_count} argument(s)");
    SLOT.with(|slot| slot.set(1));
    let (sender, receiver) = mpsc::channel::<u32>();
    let (reply_sender, reply_receiver) = mpsc::channel::<(u32, u32)>();
    // 値は、ワーカーを作る前に送り終える（モジュールの doc の「眠る所は決まっている」）。
    for value in [1u32, 2, 3, 4] {
        sender.send(value).expect("the receiver is kept until the worker ends");
    }
    drop(sender);
    // ワーカーの closure が始まったことを示すフラグ（モジュールの doc の「眠る所は決まっている」）。
    let started = Arc::new(AtomicBool::new(false));
    let started_set = Arc::clone(&started);
    let worker = std::thread::spawn(move || {
        started_set.store(true, Ordering::SeqCst);
        // 主スレッドが返しの recv で眠るまで、何にも触れずに譲る。
        for _ in 0..64 {
            std::thread::yield_now();
        }
        SLOT.with(|slot| slot.set(2));
        let mut sum = 0u32;
        for value in receiver {
            sum += value;
        }
        let tls = SLOT.with(|slot| slot.get());
        reply_sender
            .send((sum, tls))
            .expect("the main thread keeps the reply receiver");
    });
    while !started.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    let (sum, worker_tls) = reply_receiver
        .recv()
        .expect("the worker sends one reply");
    // ワーカーが終わり切るまで譲る。
    for _ in 0..64 {
        std::thread::yield_now();
    }
    worker.join().expect("the worker did not panic");
    let main_tls = SLOT.with(|slot| slot.get());
    println!("m3a-channel-tls-rust: the worker summed the channel to 10 = {}", sum == 10);
    println!(
        "m3a-channel-tls-rust: tls kept apart (main 1, worker 2) = {}",
        main_tls == 1 && worker_tls == 2
    );
    std::process::exit(argument_count as i32);
}
