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

# fd 0 を読むだけの読み端、fd 1 を書き端にして、3 つ目の引数を b と c に変えて 2 回起こす（ZeikOS の起動時の確かめと同じ形）。
# 読み端を読み書きで開くと、それも書き手に数えられて EOF が来ない。fd 3 で一時的に両側を持ってから exec で入れ替えるので、
# ほかのプロセスは端を持たない。行は標準エラーへ出すので、そちらを 2 回分続けて控える。
record_dup_ends() {
    name=$1
    if [ ! -x "$bin/$name" ]; then
        echo "skipped: $name (not built at $bin/$name)"
        return
    fi
    : > "$ref/$name.txt"
    for shape in b c; do
        fifo=$(mktemp -u)
        mkfifo "$fifo"
        set +e
        sh -c 'exec 3<>"$1" && exec env -i "$2" "$3" a "$4" 0<"$1" 1>"$1" 3<&-' sh "$fifo" "$bin/$name" \
            "$root/kernel/fsimage/seed/etc/motd" "$shape" 2>> "$ref/$name.txt"
        status=$?
        set -e
        rm -f "$fifo"
    done
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
record_dup_ends wl-dup-ends-rust
# fd の上限（2026-10-22）。RLIMIT_NOFILE を ZeikOS の表の大きさまで下げてから埋める。
record wl-fd-limit-rust
# 受け付ける前の接続を残して閉じた listener の後に作った listener（2026-10-23）。
record wl-listener-reopen-rust
# パイプの readv・writev で眠る間に別のスレッドが writev する形（2026-10-23）。fd 0 と 1 に同じパイプを渡す。
record_with_one_pipe wl-iovec-threads-rust
# ppoll の待つ長さ・シグナルのマスク・確かめの順（2026-10-24）。
record wl-ppoll-rust
# memfd を伸ばす 2 回目の ftruncate（2026-10-26）。
record wl-memfd-grow-rust
record m1-c
record m3a-c
