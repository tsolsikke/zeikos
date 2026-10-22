#!/bin/sh
# Linux（WSL）の上で linux-programs/ のプログラムを走らせ、stdout と終了ステータスを参照として控える。
#
# ZeikOS の側は、同じプログラムを同じ引数で起こし、出た行と終了ステータスをこの参照と突き合わせる。
# 環境変数は空にする（`env -i`）。イメージの上では `syscall-test` が空の envp で起こす——数が参照に入るためである。
# 読むファイルは、イメージの seed（kernel/fsimage/seed/etc/motd）と同じ中身を Linux 側でも渡す。
#
# 使い方: tools/record-linux-reference.sh [プログラムの置き場]   （既定は target/linux-programs。先に build-linux-programs.sh）
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-"$root/target/linux-programs"}
ref="$root/linux-programs/reference"
mkdir -p "$ref"

record() {
    name=$1
    if [ ! -x "$bin/$name" ]; then
        echo "skipped: $name (not built at $bin/$name)"
        return
    fi
    set +e
    env -i "$bin/$name" "$root/kernel/fsimage/seed/etc/motd" a b > "$ref/$name.txt"
    status=$?
    set -e
    printf 'exit status: %d\n' "$status" >> "$ref/$name.txt"
    echo "recorded: $ref/$name.txt (exit status $status)"
}
record_with_open_stdin() {
    name=$1
    if [ ! -x "$bin/$name" ]; then
        echo "skipped: $name (not built at $bin/$name)"
        return
    fi
    set +e
    sleep 3 | env -i "$bin/$name" "$root/kernel/fsimage/seed/etc/motd" a b > "$ref/$name.txt"
    status=$?
    set -e
    printf 'exit status: %d\n' "$status" >> "$ref/$name.txt"
    echo "recorded: $ref/$name.txt (exit status $status)"
}

record_with_one_pipe() {
    name=$1
    if [ ! -x "$bin/$name" ]; then
        echo "skipped: $name (not built at $bin/$name)"
        return
    fi
    fifo=$(mktemp -u)
    mkfifo "$fifo"
    set +e
    env -i "$bin/$name" "$root/kernel/fsimage/seed/etc/motd" a b 0<>"$fifo" 1>"$fifo" 2> "$ref/$name.txt"
    status=$?
    set -e
    rm -f "$fifo"
    printf 'exit status: %d\n' "$status" >> "$ref/$name.txt"
    echo "recorded: $ref/$name.txt (exit status $status)"
}

record m1-rust
record m3a-rust
record m3a-mutex-rust
record m3a-fault-rust
record m3a-exit-group-rust
record m3a-exit-group-join-rust
record m3a-exit-group-waiters-rust
record m3a-exit-while-spinning-rust
record m3a-nested-clone-rust
record m3a-spawn-from-thread-rust
record m3a-exit-group-spin-rust
record m3a-exit-group-main-spins-rust
record m3a-exit-group-thread-spins-rust
record m3a-channel-tls-rust
record m3a-exit-with-waiting-thread-rust
record wl-socket-lock-rust
record wl-nonblock-rust
record wl-epoll-rust
# 主スレッドが標準入力を読んで眠るイメージ。書き手を開いたままのパイプを渡す（ZeikOS の起動時の確かめと同じ形）。
record_with_open_stdin wl-exit-group-waits-rust
record wl-memfd-seals-rust
# 壁の時計（2026-10-20）。値の行は標準エラーへ出すので、参照には入らない。
record wl-realtime-rust
# fd 0 と fd 1 に同じパイプの両端を渡すイメージ（ZeikOS の起動時の確かめと同じ形）。行は標準エラーへ出すので、そちらを控える。
record_with_one_pipe wl-fault-after-wait-rust
record m1-c
record m3a-c
