#!/bin/sh
# Seinas の release から、fbdev の裏側（seinas-fbdev。musl の静的 PIE）と第三者のライセンスのアーカイブを取る。
#
# - タグ・ファイル名・SHA-256 を固定する。SHA-256 が合わなければ、置かずに止まる（終了 1）。
# - 取れた後は取り直さない（置き場に在って SHA-256 が合えば、何もせずに終わる）。CI はこの置き場をキャッシュする。
# - 置き場は target/linux-programs/（像へ入れるのは seinas-test の feature のビルドだけ。既定の像のバイトは変えない）。
#
# - ZEIKOS_SEINAS_SOURCE=<置き場> を与えると、ネットワークへは行かず、そこに在って SHA-256 が合うものだけを写す
#   （全検査が、作業ツリーへメインの置き場から写すのに使う。無ければ写さずに終了 1。項目は今までどおり名指しで飛ぶ）。
#
# 使い方: tools/fetch-seinas.sh [置き場]   （既定は target/linux-programs）
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$root/target/linux-programs"}
mkdir -p "$out"

tag=v0.1.0
base="https://github.com/tsolsikke/seinas/releases/download/$tag"

fetch() {
    name=$1
    sha=$2
    target="$out/$name"
    if [ -f "$target" ] && printf '%s  %s\n' "$sha" "$target" | sha256sum -c --status; then
        echo "kept: $target (SHA-256 matches; not fetched again)"
        return
    fi
    tmp="$target.part"
    if [ -n "${ZEIKOS_SEINAS_SOURCE:-}" ]; then
        src="$ZEIKOS_SEINAS_SOURCE/$name"
        if [ -f "$src" ] && printf '%s  %s\n' "$sha" "$src" | sha256sum -c --status; then
            cp "$src" "$tmp" && mv "$tmp" "$target"
            echo "copied: $target from $src (SHA-256 matches; not fetched)"
            return
        fi
        echo "not copied: $name is not at $ZEIKOS_SEINAS_SOURCE with the pinned SHA-256 (offline; not fetched either)" >&2
        return 1
    fi
    curl -fsSL --retry 3 -o "$tmp" "$base/$name"
    if ! printf '%s  %s\n' "$sha" "$tmp" | sha256sum -c --status; then
        echo "refused: $name from $base does not have the pinned SHA-256 $sha (not placed)" >&2
        rm -f "$tmp"
        exit 1
    fi
    mv "$tmp" "$target"
    echo "fetched: $target ($(wc -c < "$target") bytes, SHA-256 matches)"
}

fetch seinas-fbdev a27156872a714b5ec230abf9016f42468f9d1dba35ebef3eecae6cf7ad2f575b
chmod +x "$out/seinas-fbdev"
fetch seinas-fbdev-v0.1.0-third-party.tar.gz e527742e76f4a9b02910bf6cdaca6c6b3046102adca96d75edc917cf392945d6
