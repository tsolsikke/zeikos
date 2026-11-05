#!/bin/sh
# Seinas の release から、fbdev の裏側（seinas-fbdev。musl の静的 PIE）と、コンポジタ（seinas-standalone。2026-10-21）と、
# タスクバー（seinas-taskbar。2026-10-27）と、壁紙（seinas-wallpaper）と zeyes-min と同梱の壁紙（2026-11-05）と、それぞれの第三者の
# ライセンスのアーカイブを取る。
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

# **Seinas のコンポジタ（seinas-standalone）・タスクバー（seinas-taskbar）・壁紙（seinas-wallpaper）・目玉のウィンドウ（zeyes-min）と、
# 同梱の壁紙。版を上げるときは、ここの値だけを差し替える**（2026-10-21 に v0.2.0 のコンポジタ。2026-10-27 に v0.3.0 へ上げ、タスクバーを
# 足した。2026-11-05 に v0.4.0 へ上げ、壁紙と zeyes-min と同梱の壁紙を足した。どれも同じ release のものを組む）。置き場の名前には
# 版を入れない（kernel/build.rs と xtask は決まった名前を読む）。
seinas_tag=v0.4.0
fetch seinas-standalone 07f0d53735a5774d1b4c870bd76b43d936b02f742d2b654a0fbc9c825757226d seinas-standalone "$seinas_tag"
chmod +x "$out/seinas-standalone"
fetch seinas-standalone-third-party.tar.gz 6f77a26519db9dee8acf920efdfeffcd632fe16d45106fb93fae1c34d504b897 "seinas-standalone-$seinas_tag-third-party.tar.gz" "$seinas_tag"
fetch seinas-taskbar 836a8facf2ebe9ca69e8d4a734d38581a771065973bf127a5c857c6a9ce89b94 seinas-taskbar "$seinas_tag"
chmod +x "$out/seinas-taskbar"
fetch seinas-taskbar-third-party.tar.gz 93ef559bc2075e840fc09d01b3786371d6637ef5293ba9d5fc30f09a4c467cde "seinas-taskbar-$seinas_tag-third-party.tar.gz" "$seinas_tag"
fetch seinas-wallpaper 2a56bd91aa924d5055b7afaca388f44e48bd0a585708e8fcad73c8b0c61e6223 seinas-wallpaper "$seinas_tag"
chmod +x "$out/seinas-wallpaper"
fetch seinas-wallpaper-third-party.tar.gz d6062c2d08691647c89fd97550a6a42cefed725c936743d66bcbfcfd6b8b522e "seinas-wallpaper-$seinas_tag-third-party.tar.gz" "$seinas_tag"
fetch zeyes-min e80821a2c0409a07f21d8258844a6a9692e622a2374f80fb97b7995db17194d4 zeyes-min "$seinas_tag"
chmod +x "$out/zeyes-min"
fetch zeyes-min-third-party.tar.gz 72824cb8644d8d683a1d15644ff6a0f475a0116732cc86a444780c6c8c98e61c "zeyes-min-$seinas_tag-third-party.tar.gz" "$seinas_tag"
fetch seinas-wallpapers.tar.gz eeb0911d2b4cab582ece17a1aa34b2f3ea5d68dc2fb8338819f036a4c61e4596 "seinas-wallpapers-$seinas_tag.tar.gz" "$seinas_tag"
