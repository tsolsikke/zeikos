#!/bin/sh
# Seinas の release から、fbdev の裏側（seinas-fbdev。musl の静的 PIE）と、コンポジタ（seinas-standalone。2026-10-21）と、
# タスクバー（seinas-taskbar。2026-10-27）と、それぞれの第三者のライセンスのアーカイブを取る。
#
# - タグ・ファイル名・SHA-256 を固定する。SHA-256 が合わなければ、置かずに止まる（終了 1）。固定する SHA-256 は、その release の
#   SHA256SUMS の値である（版を上げるときは、SHA256SUMS を取ってコピーする）。
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

# **Seinas のコンポジタ（seinas-standalone）とタスクバー（seinas-taskbar）。版を上げるときは、ここの 7 つだけを差し替える**
# （2026-10-21 に v0.2.0 のコンポジタ。2026-10-27 に v0.3.0 へ上げ、タスクバーを足した。タスクバーは同じ release のコンポジタと
# 組む）。置き場の名前には版を入れない（kernel/build.rs と xtask は決まった名前を読む）。
standalone_tag=v0.3.0
standalone_sha=a4df394e1fcbd5fb2c2a490666e8de7434149582dda9e38b92e30beaccd55044
standalone_notices_asset=seinas-standalone-v0.3.0-third-party.tar.gz
standalone_notices_sha=1fff0478bc19d285839eff17f61b08981bc0e40e88ae530aeff7fba49b44ea2a
taskbar_sha=4328035d9d04a9e1acbdda0471d54771b522f77c3cf67ee131bb8073b13c37a2
taskbar_notices_asset=seinas-taskbar-v0.3.0-third-party.tar.gz
taskbar_notices_sha=2db60ce438fd4b7dffca5846f8f36e798ae73979e64b47166086553fa042704b
fetch seinas-standalone "$standalone_sha" seinas-standalone "$standalone_tag"
chmod +x "$out/seinas-standalone"
fetch seinas-standalone-third-party.tar.gz "$standalone_notices_sha" "$standalone_notices_asset" "$standalone_tag"
fetch seinas-taskbar "$taskbar_sha" seinas-taskbar "$standalone_tag"
chmod +x "$out/seinas-taskbar"
fetch seinas-taskbar-third-party.tar.gz "$taskbar_notices_sha" "$taskbar_notices_asset" "$standalone_tag"
