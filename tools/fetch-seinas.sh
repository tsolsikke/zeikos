#!/bin/sh
# Seinas の release から、fbdev の裏側（seinas-fbdev。musl の静的 PIE）と、コンポジタ（seinas-standalone。2026-10-21）と、
# それぞれの第三者のライセンスのアーカイブを取る。
#
# - タグ・ファイル名・SHA-256 を固定する。SHA-256 が合わなければ、置かずに止まる（終了 1）。
# - 取れた後は取り直さない（置き場に在って SHA-256 が合えば、何もせずに終わる）。CI はこの置き場をキャッシュする。
# - 置き場は target/linux-programs/（イメージへ入れるのは seinas-test の feature のビルドだけ。既定のイメージのバイトは変えない）。
#
# - ZEIKOS_SEINAS_SOURCE=<置き場> を与えると、ネットワークへは行かず、そこに在って SHA-256 が合うものだけをコピーする
#   （全検査が、作業ツリーへメインの置き場からコピーするのに使う。無ければコピーせずに終了 1。項目は今までどおり名指しで飛ぶ）。
#
# 使い方: tools/fetch-seinas.sh [置き場]   （既定は target/linux-programs）
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$root/target/linux-programs"}
mkdir -p "$out"

tag=v0.1.0

# fetch <置き場の名前> <SHA-256> [release の上の名前] [タグ]（後の 2 つを省くと、置き場の名前と上の tag を使う）
fetch() {
    name=$1
    sha=$2
    remote=${3:-$name}
    from="https://github.com/tsolsikke/seinas/releases/download/${4:-$tag}"
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
    curl -fsSL --retry 3 -o "$tmp" "$from/$remote"
    if ! printf '%s  %s\n' "$sha" "$tmp" | sha256sum -c --status; then
        echo "refused: $remote from $from does not have the pinned SHA-256 $sha (not placed)" >&2
        rm -f "$tmp"
        exit 1
    fi
    mv "$tmp" "$target"
    echo "fetched: $target ($(wc -c < "$target") bytes, SHA-256 matches)"
}

fetch seinas-fbdev a27156872a714b5ec230abf9016f42468f9d1dba35ebef3eecae6cf7ad2f575b
chmod +x "$out/seinas-fbdev"
fetch seinas-fbdev-v0.1.0-third-party.tar.gz e527742e76f4a9b02910bf6cdaca6c6b3046102adca96d75edc917cf392945d6

# **Seinas のコンポジタ（seinas-standalone）。版を上げるときは、ここの 5 つだけを差し替える**（2026-10-21。v0.2.0）。
# 置き場の名前には版を入れない（kernel/build.rs と xtask は決まった名前を読む）。
standalone_tag=v0.2.0
standalone_asset=seinas-standalone
standalone_sha=e9df943e15787fadccf623ec28475c401c2ba1be6f1b99326cc528e67a2296f7
standalone_notices_asset=seinas-standalone-v0.2.0-third-party.tar.gz
standalone_notices_sha=82a3cba45693c2b45e55fc6674377ca0e4a0c014f59fb19f70be9e90a52df3a0
fetch seinas-standalone "$standalone_sha" "$standalone_asset" "$standalone_tag"
chmod +x "$out/seinas-standalone"
fetch seinas-standalone-third-party.tar.gz "$standalone_notices_sha" "$standalone_notices_asset" "$standalone_tag"
