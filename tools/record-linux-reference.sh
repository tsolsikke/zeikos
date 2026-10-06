#!/bin/sh
# Linux（WSL）の上で linux-programs/ のプログラムを走らせ、stdout と終了の状態を参照として控える。
#
# ZeikOS の側は、同じプログラムを同じ引数で起こし、出た行と終了の状態をこの参照と突き合わせる。
# 環境変数は空にする（`env -i`）。像の上では `syscall-test` が空の envp で起こす——数が参照に入るためである。
# 読むファイルは、像の種（kernel/fsimage/seed/etc/motd）と同じ中身を Linux 側でも渡す。
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

record m1-rust
record m1-c
