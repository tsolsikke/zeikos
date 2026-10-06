#!/bin/sh
# Linux 向けのプログラム（linux-programs/）を手で作る。
#
# - m1-rust: 既定の像にも入る（kernel/build.rs が rustc を直に呼ぶ）。ここでは、Linux の上で走らせて参照を控える
#   ための写しを作る。版は rust-toolchain.toml のもので、像の中身と同じバイトになる。
# - m1-c: musl-gcc で作る。版が配布物（Ubuntu の musl）に依るので、既定の像には入れない。道具が無ければ名指しで飛ばす。
#
# 使い方: tools/build-linux-programs.sh [出力先]   （既定は target/linux-programs）
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$root/target/linux-programs"}
mkdir -p "$out"

rustc --edition 2021 --target x86_64-unknown-linux-musl \
    -C target-feature=+crt-static -C opt-level=2 -C strip=symbols \
    --remap-path-prefix="$root/linux-programs=linux-programs" \
    -o "$out/m1-rust" "$root/linux-programs/m1-rust.rs"
echo "built: $out/m1-rust ($(wc -c < "$out/m1-rust") bytes)"

if command -v musl-gcc >/dev/null 2>&1; then
    musl-gcc -static -O2 -s -o "$out/m1-c" "$root/linux-programs/m1-c.c"
    echo "built: $out/m1-c ($(wc -c < "$out/m1-c") bytes)"
else
    echo "skipped: m1-c (musl-gcc is not installed; apt install musl-tools)"
fi
