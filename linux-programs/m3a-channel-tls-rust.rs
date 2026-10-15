//! `m3a-channel-tls-rust`: Linux 向けにビルドした、musl で静的リンクした Rust のプログラム（`std` を使う）。**スレッドと
//! チャネル（`std::sync::mpsc`）で値を受け渡し、`join` で待ち、TLS（`thread_local!`）の値がスレッドの間で混ざらないことを
//! 見る**（ZEIKOS-107 の 1。`ADR-0085`）。
//!
//! # 何をするか
//!
//! 1. 主スレッドが 1 行出し、TLS の値を 1 にして、チャネルを 2 本作り、ワーカーのスレッドを作る。
//! 2. ワーカーは TLS の値を 2 にし、チャネルから届く値を送り手が閉じるまで足し（1+2+3+4 = 10）、合計と自分の TLS の値を
//!    返しのチャネルで送る。待ちは musl の futex で眠る（`futex:` の行に出る）。
//! 3. 主スレッドは 4 つの値を送って送り手を閉じ、返しを受け取り、`join` し、合計が 10 であることと、TLS が主で 1・ワーカーで 2 の
//!    ままであることを 1 行ずつ出す。終了の状態は引数の数。
//!
//! # crate ではない
//!
//! `m1-rust` と同じで、`kernel/build.rs` が `rustc` を 1 回、`--target x86_64-unknown-linux-musl` で呼び、ディスクイメージの
//! `/bin/linux` に置く。

use std::cell::Cell;
use std::sync::mpsc;

thread_local! {
    static SLOT: Cell<u32> = const { Cell::new(0) };
}

fn main() {
    let argument_count = std::env::args().count();
    println!("m3a-channel-tls-rust: main starts with {argument_count} argument(s)");
    SLOT.with(|slot| slot.set(1));
    let (sender, receiver) = mpsc::channel::<u32>();
    let (reply_sender, reply_receiver) = mpsc::channel::<(u32, u32)>();
    let worker = std::thread::spawn(move || {
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
    for value in [1u32, 2, 3, 4] {
        sender.send(value).expect("the worker keeps the receiver");
    }
    drop(sender);
    let (sum, worker_tls) = reply_receiver
        .recv()
        .expect("the worker sends one reply");
    worker.join().expect("the worker did not panic");
    let main_tls = SLOT.with(|slot| slot.get());
    println!("m3a-channel-tls-rust: the worker summed the channel to 10 = {}", sum == 10);
    println!(
        "m3a-channel-tls-rust: tls kept apart (main 1, worker 2) = {}",
        main_tls == 1 && worker_tls == 2
    );
    std::process::exit(argument_count as i32);
}
