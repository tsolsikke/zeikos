#!/bin/sh
# Linux 向けのプログラム（linux-programs/）を手で作る。
#
# - m1-rust・m3a-*: 既定のイメージにも入る（kernel/build.rs が rustc を直に呼ぶ）。ここでは、Linux の上で走らせて参照を控える
#   ためのコピーを作る。版は rust-toolchain.toml のもので、イメージの中身と同じバイトになる。
# - m1-c: musl-gcc で作る。版が配布物（Ubuntu の musl）に依るので、既定のイメージには入れない。道具が無ければ名指しで飛ばす。
#
# 使い方: tools/build-linux-programs.sh [出力先]   （既定は target/linux-programs）
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$root/target/linux-programs"}
mkdir -p "$out"

for name in m1-rust m3a-rust m3a-mutex-rust m3a-fault-rust m3a-exit-group-rust m3a-exit-group-join-rust m3a-exit-group-waiters-rust m3a-exit-while-spinning-rust m3a-nested-clone-rust m3a-spawn-from-thread-rust m3a-exit-group-spin-rust m3a-exit-group-main-spins-rust m3a-exit-group-thread-spins-rust m3a-channel-tls-rust m3a-exit-with-waiting-thread-rust wl-socket-lock-rust wl-nonblock-rust wl-epoll-rust wl-exit-group-waits-rust wl-memfd-seals-rust wl-fault-after-wait-rust wl-dup-ends-rust wl-fd-limit-rust wl-listener-reopen-rust wl-iovec-threads-rust wl-ppoll-rust m3a-memfd-shrink-rust; do
    rustc --edition 2021 --target x86_64-unknown-linux-musl \
        -C target-feature=+crt-static -C opt-level=2 -C strip=symbols \
        --remap-path-prefix="$root/linux-programs=linux-programs" \
        -o "$out/$name" "$root/linux-programs/$name.rs"
    echo "built: $out/$name ($(wc -c < "$out/$name") bytes)"
done

if command -v musl-gcc >/dev/null 2>&1; then
    musl-gcc -static -O2 -s -o "$out/m1-c" "$root/linux-programs/m1-c.c"
    # m3a-c: musl の pthread（pthread_create と pthread_join）。m1-c と同じく linux-c-test のビルドだけが入れる。
    musl-gcc -static -O2 -s -pthread -o "$out/m3a-c" "$root/linux-programs/m3a-c.c"
    echo "built: $out/m1-c ($(wc -c < "$out/m1-c") bytes)"
    echo "built: $out/m3a-c ($(wc -c < "$out/m3a-c") bytes)"
else
    echo "skipped: m1-c (musl-gcc is not installed; apt install musl-tools)"
fi
