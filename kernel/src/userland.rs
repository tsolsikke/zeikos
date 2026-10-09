//! ユーザープログラムのロードと実行（S11-4 で `main.rs` から移した）。
//!
//! # なぜ lib にあるのか
//!
//! **`spawn` を処理するのは `syscall::dispatch` で、あちらは lib にある。**
//! bin 側に置いたままだと、システムコールからプロセスを作れない
//! （`ADR-0030` の Alternatives が「順序として、所有を先に決める」と書いた案である。
//! 所有が決まったので、ここへ移した）。
//!
//! # 何が移り、何が残ったか
//!
//! **境界は「機構」と「記述」である。**
//!
//! | ここ（lib） | `main.rs`（bin） |
//! |---|---|
//! | プロセスを作って走らせる機構 | 何を走らせ、何を期待するかの記述 |
//! | [`UserProcess`]・[`UserLoadError`] | `USER_PROGRAMS` の表 |
//! | [`load_user_program`] とマッピング・遠征 | 終わり方の判定と会計の判定行 |
//!
//! **`common::ext2`（パーサ）と `crate::vfs`（使い方）を分けた線と同じ形である。**
//!
//! # `errno` は知らない
//!
//! [`UserLoadError`] は `errno` を持たない。**変換するのは `syscall` の側である**
//! （`common::ext2::Ext2Error` と `vfs::FileTableError` に続く 3 つ目）。

use common::critical::Locked;
use common::log::{LogLevel, Logger};
use common::machine::pc::Serial;

use crate::arch::x86_64::MAX_EXCURSION_DEPTH;
use crate::syscall::{MAX_ARGV_BYTES, MAX_ENVP_BYTES, MAX_EXECUTABLE_SIZE, PATH_MAX};

/// ユーザープログラムを走らせる空間のユーザーサブツリーの添字（S9-b-1）。
///
/// **`USER_PML4_INDEX`（= 1。`kernel/src/main.rs`）とは別である。** あちらは本番の空間の値で、
/// 起動時の検証 3 本が使っている。**プログラムは自分の空間を持つので、添字も
/// 自分で決められる**（`AddressSpace` が添字を持つ。S7-e）。
///
/// 0 を採るのは、`hello` を `0x400000` へリンクしているからである（Linux の
/// 非 PIE の既定と同じ）。**本番の空間の `PML4[0]` には恒等除去まで恒等が居るが、
/// 新しい空間の下位は空なので関係が無い。**
pub const USER_PROGRAM_SUBTREE_INDEX: usize = 0;

/// ユーザープログラムのスタックの上端（S9-b-1）。
///
/// `hello` のイメージは `0x400000` から 2 ページなので、十分離れた位置に置く。
///
/// **位置を決めてリンクした像（`ET_EXEC`）の配置である**（[`ProcessLayout::EXECUTABLE`]）。位置独立の像は、別の配置を
/// 使う（[`ProcessLayout::POSITION_INDEPENDENT`]）。
const USER_PROGRAM_STACK_TOP: u64 = 0x0080_0000;

/// 位置を決めてリンクした像のスタックの大きさ（4 ページ。2026-10-06）。
///
/// **S9-b-1 から 1 ページだった。** 補助ベクタを積むようになって初期データが約 190 バイト増え、`/bin/ls` と `zash` の
/// 使用量が 1 ページの半分を越えた（`ADR-0041` の決定 4 の決めどき。実測で 51% と 71%）。4 ページにして、下に見張りの
/// ページを置いた（`ADR-0041` の Addendum）。
const USER_PROGRAM_STACK_BYTES: u64 = 4 * 4096;

/// プロセスの番地の配置（2026-10-05）。**像の種類ごとに 1 つ在る。**
///
/// # なぜ型にするか
///
/// **以前は、スタックの上端・ヒープの上端・`mmap` の始まりが、ばらばらの定数だった**（どれも、位置を決めてリンクした
/// 小さな像を前提にしている。像は `0x400000` から、スタックは `0x800000` の下の 1 ページ）。**Linux 向けの静的リンクの
/// 実行ファイルは、数 MiB の位置独立の像で、スタックも 1 ページでは足りない。** 同じ番地に置くと、像がスタックに
/// 重なる。配置を 1 つの値にまとめて、像の種類で選ぶ。
///
/// # 2 つの配置
///
/// | | 像 | ヒープ | `mmap` | スタック |
/// |---|---|---|---|---|
/// | `ET_EXEC`（今までの形） | リンクした番地 | 像の末尾 〜 `0x7fb000` | `0x1000_0000` 〜 | `0x7fc000..0x800000`（4 ページ） |
/// | `ET_DYN`（静的 PIE） | `0x400000` だけずらす | 像の末尾 〜 `0x0fff_f000` | `0x1000_0000` 〜 スタックの見張りの下 | `0x7f_ffff_f000` の下の 256 KiB |
///
/// **どちらの配置も、スタックの下に写さないページを 1 枚置く**（見張りのページ。`ET_EXEC` は 2026-10-06 から。
/// それまでは 1 ページのスタックのすぐ下がヒープの上限で、あふれるとヒープを黙って壊すおそれが在った）。スタックが
/// 尽きると、そこを踏んでページフォルトになり、プロセスが終わる。`ET_DYN` の `mmap` は、そのページより下までしか配らない。
///
/// **`ET_EXEC` の配置は、1 つも変えていない**（既存のプログラムは、今までと同じ番地に載る）。2 つを 1 つに揃えるか
/// どうかは、後で決める（`docs/deferred-decisions.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessLayout {
    /// スタックの上端（最初の RSP は、ここから下へ積んだ初期データの先頭）。
    pub stack_top: u64,
    /// 起動のときに写すスタックのバイト数（ページの倍数）。**後から伸ばさない。**
    pub stack_bytes: u64,
    /// スタックの下に、写さないページを 1 枚置くか。
    pub stack_guard: bool,
    /// ヒープ（`brk`）が越えられない上端。
    pub heap_limit: u64,
    /// `mmap` が配る番地の始まり。
    pub mmap_base: u64,
    /// `mmap` が配る番地の終わり。**範囲の終端で、この値そのものは含まない**（配る範囲の末尾がこの値に等しいのはよく、
    /// 越えるのは断る。`crate::mappings::MemoryMap::reserve` の `end > limit`）。[`crate::arch::x86_64::USER_ADDRESS_LIMIT`] と
    /// 同じ意味の値で、それを越えない（2026-10-09 に揃えた）。
    pub mmap_limit: u64,
}

impl ProcessLayout {
    /// 配置のページの大きさ。
    const PAGE: u64 = 4096;

    /// 位置を決めてリンクした像（`ET_EXEC`）の配置。**今までの定数と同じ値である。**
    pub const EXECUTABLE: Self = Self {
        stack_top: USER_PROGRAM_STACK_TOP,
        stack_bytes: USER_PROGRAM_STACK_BYTES,
        stack_guard: true,
        heap_limit: HEAP_LIMIT,
        mmap_base: crate::syscall::MMAP_BASE,
        // **ユーザーの番地の上限と同じ値**（どちらも範囲の終端で、その値を含まない）。以前は `1 << 47` で、上限より 1 ページ
        // 上だった——最後のページ（`0x7fff_ffff_f000` から）を配りうる形だった（今の上限の大きさの要求では届かない）。
        // `USER_ADDRESS_LIMIT - 4096` にすると、逆向きに 1 ページ狭くなる（意味を揃えずに値だけを合わせた形）。
        mmap_limit: crate::arch::x86_64::USER_ADDRESS_LIMIT,
    };

    /// 位置独立の像（`ET_DYN`。静的 PIE）をずらす量。**決まった値である**（番地を毎回変えることは、していない）。
    pub const POSITION_INDEPENDENT_BASE: u64 = 0x0040_0000;

    /// 位置独立の像の、端から端までの上限（32 MiB）。
    pub const POSITION_INDEPENDENT_MAX_SPAN: u64 = 32 * 1024 * 1024;

    /// 位置独立の像（`ET_DYN`）の配置。
    pub const POSITION_INDEPENDENT: Self = Self {
        stack_top: 0x0000_007f_ffff_f000,
        stack_bytes: 256 * 1024,
        stack_guard: true,
        heap_limit: crate::syscall::MMAP_BASE - Self::PAGE,
        mmap_base: crate::syscall::MMAP_BASE,
        // スタックの下端の、さらに 1 ページ下（見張りのページ）まで。
        mmap_limit: 0x0000_007f_ffff_f000 - 256 * 1024 - Self::PAGE,
    };

    /// 像の種類から配置を選ぶ。
    pub const fn for_kind(kind: common::elf::ElfKind) -> Self {
        match kind {
            common::elf::ElfKind::Executable => Self::EXECUTABLE,
            common::elf::ElfKind::PositionIndependent => Self::POSITION_INDEPENDENT,
        }
    }

    /// スタックの下端（写す範囲の、いちばん低い番地）。
    pub const fn stack_bottom(&self) -> u64 {
        self.stack_top - self.stack_bytes
    }

    /// 見張りのページの番地（置かない配置では `None`）。
    pub const fn stack_guard_page(&self) -> Option<u64> {
        if self.stack_guard {
            Some(self.stack_bottom() - Self::PAGE)
        } else {
            None
        }
    }

    /// 像を置いてよい範囲と上限（`common::elf::ElfHeaders::plan` に渡す）。
    ///
    /// - `ET_EXEC`——**今までと同じに、範囲では断らない**（ユーザーの番地の全体を範囲にし、大きさの上限も付けない）。
    ///   スタックやほかの区画と重なる像は、今までどおり、写す所が断る。
    /// - `ET_DYN`——ずらした後の像が、ずらす量からヒープの上端までに収まり、端から端までが上限以下であること。
    pub const fn load_policy(&self, kind: common::elf::ElfKind) -> common::elf::LoadPolicy {
        match kind {
            common::elf::ElfKind::Executable => common::elf::LoadPolicy {
                position_independent_base: 0,
                window: (0, 1 << 47),
                max_span: u64::MAX,
            },
            common::elf::ElfKind::PositionIndependent => common::elf::LoadPolicy {
                position_independent_base: Self::POSITION_INDEPENDENT_BASE,
                window: (Self::POSITION_INDEPENDENT_BASE, self.heap_limit),
                max_span: Self::POSITION_INDEPENDENT_MAX_SPAN,
            },
        }
    }
}

/// 1 プロセスに渡せる `argv` の要素数の上限（S11-1）。
///
/// **見込みの最大は 2 である**（プログラム名 + 引数 1 つ）。**8 はその 4 倍で、
/// 表と文字列がスタックの 1 ページに収まる範囲である。** 越えたら
/// [`UserLoadError::ArgumentsTooLong`] で拒む。
pub const MAX_ARGV: usize = 8;

use common::env::{classify_env_line, trim_env_line, EnvLine, EnvTable};
/// 環境の 1 行の純粋な判定は `common` に在る（f-2。`ADR-0045` の形）。
///
/// **シェルも同じものを `#[path]` で取り込む**——**カーネルが `/etc/environment` の
/// 行を受ける規則と、`export` が受ける規則を別にしない**（`ADR-0053` の Decision 3）。
pub use common::env::{ENV_LINE_MAX, MAX_ENVP};

/// すべてのプロセスへ積む環境（EV。ADR-0041）。
///
/// # なぜカーネルが 1 つ持つのか
///
/// **プロセスごとに違う環境を持たない**（ADR-0041 の Decision 2）。
/// **効果は費用よりも面にある**——**ユーザーポインタを 1 本も増やさない。**
/// `spawn` が受け取るのは今までどおり `path` と `argv` だけで、
/// **環境は user から来ない。**
///
/// # `TERM` と `PATH` の 2 つである
///
/// **読む者が居ないものを積まない。** `PS1` も `KEYMAP` も入れない
/// （`docs/verification-coverage.md` の「使う者がいない機構は検算が置けない」）。
/// **どちらも読む側を同じ段階で作った**——`zash` が `TERM` で色を決め、
/// `PATH` で語を探す。
///
/// **`PATH` は DIR-1 で入った**（ADR-0043）。**`DEFAULT_DIR` の固定の既定を
/// 置き換えたものである**——あの doc が「置き換えの条件は `envp` を開けた
/// とき」と書いており、EV で開いた。
///
/// **`PATH` はいま固定の既定と同じくらい固定である。承知のうえで採った**
/// （ADR-0043 の決定 4。**見直しの行は `docs/deferred-decisions.md` にある**）。
///
/// **配列の選択はこれでは解けない**——**配列を読むのはカーネルのデコーダで、
/// Ring 3 の `envp` を見ない**（`docs/foundation-inventory.md` の訂正）。
///
/// # 並びに意味がある
///
/// **`TERM` を先に置く。** **`syscall-test` が `envp[0]` を突き合わせている**
/// ので、入れ替えるとあちらが落ちる（**落ちてよい。契約だからである**）。
///
/// 破壊テスト (EV, env-drop-term-test): **`TERM` だけを落とす。** `zash` は `TERM` を見つけられず、
/// **プロンプトの色を既定へ落とす。**
///
/// **落ちるのは 3 本である**（実測。**「1 本だけ」ではない**）——色の判定・
/// 記号の判定・代替画面の復帰の判定。**根は 1 つで、どれも
/// 「プロンプトの色付きの連なり」を目印にしている**（`crate::console::probe` の
/// `find_colored_run_in_row`）。
///
/// **この破壊テストが固有に検出するものを書いておく。** **`TERM` を読まずに
/// 常に色を付ける形である**——**既定の構成ではどの判定も落ちないので、
/// この破壊テストが無ければ「環境が色を決めている」ことを誰も主張していない。**
/// **`zash-prompt-drop-color` は送る側を壊す**ので、こちらとは別の形である。
///
/// 破壊テスト (DIR-1, env-drop-path-test): **`PATH` だけを落とす。** **名前だけで
/// 打った語が起動できなくなる**——**`--shell-test` の
/// 「bare names resolved under /bin」がそのまま受け止める。**
/// **`/bin/ls` のようにパスを直に打つ形は動く**ので、**落ちるのは
/// 探索の判定だけである。**
#[cfg(all(
    not(feature = "env-drop-term-test"),
    not(feature = "env-drop-path-test")
))]
const DEFAULT_ENVIRONMENT: &[&[u8]] = &[b"TERM=zeikos", b"PATH=/bin", b"HOME=/root"];

#[cfg(all(feature = "env-drop-term-test", not(feature = "env-drop-path-test")))]
const DEFAULT_ENVIRONMENT: &[&[u8]] = &[b"PATH=/bin", b"HOME=/root"];

#[cfg(all(not(feature = "env-drop-term-test"), feature = "env-drop-path-test"))]
const DEFAULT_ENVIRONMENT: &[&[u8]] = &[b"TERM=zeikos", b"HOME=/root"];

#[cfg(all(feature = "env-drop-term-test", feature = "env-drop-path-test"))]
const DEFAULT_ENVIRONMENT: &[&[u8]] = &[b"HOME=/root"];

/// 環境の出どころのパス（f-1。`ADR-0052` の Decision 1）。
const ENV_SOURCE: &[u8] = b"/etc/environment";

/// 積む環境の実体（f-1。`ADR-0052`）。
///
/// # なぜ `static mut` にしないのか
///
/// **`Locked<T>` の裏に閉じる**（`ADR-0023` の seam 整備の決まり）。
/// **新しい `unsafe` を作らない。**
struct Environment {
    /// 表そのもの。**純粋な論理は `common` に在る**（f-2。`ADR-0053`）。
    table: EnvTable,
    /// **ファイルから読めたか。** **判定はこちらを見る**
    /// （`ADR-0052` の Decision 4。落とす前を見る）。
    from_file: bool,
}

impl Environment {
    const fn new() -> Self {
        Self {
            table: EnvTable::new(),
            from_file: false,
        }
    }
}

static ENVIRONMENT: Locked<Environment> = Locked::new(Environment::new());

/// 環境の出どころを読む（f-1。`ADR-0052`）。
///
/// # 呼ぶ位置
///
/// **イメージの複製の直後・最初の Ring 3 の前である**（`ADR-0052` の Decision 2）。
/// **ウィンドウは実測で挟まっている**——`kernel/src/main.rs` で、イメージの複製が
/// `copy_fs_image_to_frames`、最初の Ring 3 が `verify_bss_is_mapped` である。
///
/// **置き場が主張を決める。** **P-e でイメージの検査を 1 つ後ろに置いていたために
/// `exercise` が書き換えた後を見ていた、という種類と同じである**
/// （`docs/troubleshooting.md`）。**動かすときは何が変わるかを見ること。**
///
/// # 落ちる道は 1 つに閉じる
///
/// **開けない・読めない・1 行も採れない、のどれでも既定へ落ちる。**
/// **止めない**——**利用者が `rm /etc/environment` を打てる**
/// （`ADR-0052` の Decision 3）。
pub fn load_environment(logger: &mut Logger<Serial>) {
    let mut taken = 0usize;
    let mut dropped = 0usize;
    let mut from_file = false;

    // 破壊テスト (f-1, env-ignore-file-test): 出どころを読まず、既定へ落ちる。
    // **`ADR-0052` の Decision 1 が主張しているのは「源はファイルである」で、
    // それを直接否定する形である。** **出る環境は種と同じなので、
    // 値を見る判定は 1 つも落ちない**——**`from_file` を見る判定と、
    // 書き換えが 2 度目に効く判定でしか検出されない。**
    #[cfg(feature = "env-ignore-file-test")]
    let source: Option<&'static [u8]> = None;
    #[cfg(not(feature = "env-ignore-file-test"))]
    let source = read_env_source(logger);

    if let Some(contents) = source {
        from_file = true;
        let mut environment = ENVIRONMENT.lock();
        for line in contents.split(|byte| *byte == b'\n') {
            let line = trim_env_line(line);
            match classify_env_line(line) {
                EnvLine::Ignore => {}
                EnvLine::Take => {
                    // 破壊テスト (EV, env-drop-term-test / env-drop-path-test):
                    // **その名前の行を落とす。**
                    //
                    // **f-1 で場所を移した。** **以前は既定の表の側だけを
                    // 削っていたが、出どころがファイルになって効かなくなった**
                    // ——**ファイルが在れば既定は使われない**（実測。
                    // 2026-08-31。**破壊テストを入れても `envc=3` のままだった**）。
                    // **SE-d の種類である**——**性質が構造的に真になった破壊テストは、
                    // 残すと嘘の安心になる。** **ここは出どころを問わず必ず通る。**
                    if drop_by_sabotage(line) {
                        dropped += 1;
                        continue;
                    }
                    if environment.table.push(line) {
                        taken += 1;
                    } else {
                        dropped += 1;
                        logger.info(format_args!(
                            "env-source: dropped a line; the table already holds {MAX_ENVP}"
                        ));
                    }
                }
                EnvLine::Reject(reason) => {
                    dropped += 1;
                    logger.info(format_args!("env-source: dropped a line; {reason:?}"));
                }
            }
        }
    }

    // **1 行も採れなければ既定へ落ちる。** **「読めたが空だった」も同じ扱い
    // である**——**環境が空のまま Ring 3 を起動すると、`PATH` が無くなって
    // 名前でコマンドを引けなくなる。**
    if taken == 0 {
        let mut environment = ENVIRONMENT.lock();
        environment.table.clear();
        for line in DEFAULT_ENVIRONMENT {
            let _ = environment.table.push(line);
        }
        environment.from_file = false;
    } else {
        ENVIRONMENT.lock().from_file = from_file;
    }

    apply_keymap(logger);

    let environment = ENVIRONMENT.lock();
    logger.info(format_args!(
        "env-source: {} took {taken} line(s) and dropped {dropped}; the table holds {} (from_file={})",
        core::str::from_utf8(ENV_SOURCE).unwrap_or("?"),
        environment.table.count(),
        environment.from_file
    ));
}

/// `KEYMAP` を読んでキーボードの配列を選ぶ（f-1b）。
///
/// # 値は `jis` と `us` の 2 つである
///
/// **名指すのは配列であって、キーボードの型番ではない**——**表は 64 個の
/// スキャンコードと 2 キーぶんしか持たず、`jp106` / `us101` が主張する
/// キー数の精度を持っていない。**
///
/// # 未知の値は既定へ落ちる
///
/// **既定は `jis` である**（いまの振る舞いを変えない。**既定を US にすると、
/// 設定ファイルが無いときに手元の刻印どおりに打てなくなる**）。
/// **落としたことは出す**——`ADR-0052` の Decision 3 と同じ形で、
/// **新しい規則を作らない。**
///
/// # 読む位置
///
/// **環境を組んだ直後である。** **キー割り込みが来るのは `start_timer` より
/// 後なので、それより前に決まっていれば足りる**（実測。
/// `kernel/src/main.rs` で、環境を読むのがイメージの複製の直後、
/// `start_timer` はその下である）。
fn apply_keymap(logger: &mut Logger<Serial>) {
    const KEYMAP: &[u8] = b"KEYMAP=";
    // **ロックの下ではコピーだけを取る。** **借りたまま `drop` できない。**
    let mut value = [0u8; ENV_LINE_MAX];
    let mut length = None;
    {
        let environment = ENVIRONMENT.lock();
        for index in 0..environment.table.count() {
            let line = environment.table.line(index);
            if let Some(tail) = line.strip_prefix(KEYMAP) {
                value[..tail.len()].copy_from_slice(tail);
                length = Some(tail.len());
            }
        }
    }
    let chosen = length.map(|length| &value[..length]);

    let us = match chosen {
        None => false,
        Some(b"jis") => false,
        Some(b"us") => true,
        Some(other) => {
            logger.info(format_args!(
                "keymap: {:?} is not a layout; falling back to jis",
                core::str::from_utf8(other).unwrap_or("?")
            ));
            false
        }
    };
    crate::keyboard::decode::set_us_layout(us);
    logger.info(format_args!(
        "keymap: the keyboard layout is {} (KEYMAP was {})",
        if us { "us" } else { "jis" },
        match chosen {
            Some(value) => core::str::from_utf8(value).unwrap_or("?"),
            None => "not set",
        }
    ));
}

/// 破壊テストが落とす名前か（EV。f-1 で場所を移した）。
///
/// **既定の構成では常に偽である。**
fn drop_by_sabotage(line: &[u8]) -> bool {
    #[cfg(feature = "env-drop-term-test")]
    if line.starts_with(b"TERM=") {
        return true;
    }
    #[cfg(feature = "env-drop-path-test")]
    if line.starts_with(b"PATH=") {
        return true;
    }
    let _ = line;
    false
}

/// 出どころを読む（f-1）。**開けなければ `None` で、そのことを出す。**
///
/// # 器を作らない
///
/// **イメージはカーネルが抱えている複製で、寿命は `'static` である**
/// （`crate::vfs::root_image`）。**ブロックをそのまま借りればよい。**
/// **カーネルにヒープが無いので、コピー先を固定で取る形も考えたが、要らない。**
///
/// # 先頭の 1 ブロックだけを読む
///
/// **`/etc/environment` が 4096 バイトを超える形は読まない。**
/// **`MAX_ENVP` が 8 で 1 行が [`ENV_LINE_MAX`] なので、採れるのは
/// 高々 1KiB ぶんである**——**4096 バイトの中に、採れる行はすべて入る。**
/// **越えたぶんは黙って読まれない**ので、**そのことを出す。**
fn read_env_source(logger: &mut Logger<Serial>) -> Option<&'static [u8]> {
    let filesystem = match crate::vfs::root_filesystem() {
        Ok(filesystem) => filesystem,
        Err(error) => {
            logger.info(format_args!(
                "env-source: the root filesystem did not parse ({error:?}); falling back"
            ));
            return None;
        }
    };
    let inode = match filesystem.lookup(ENV_SOURCE) {
        Ok(inode) => inode,
        Err(error) => {
            logger.info(format_args!(
                "env-source: {} is not there ({error:?}); falling back",
                core::str::from_utf8(ENV_SOURCE).unwrap_or("?")
            ));
            return None;
        }
    };
    let block = match filesystem.file_block(&inode, 0) {
        Ok(block) => block,
        Err(error) => {
            logger.info(format_args!(
                "env-source: could not read {} ({error:?}); falling back",
                core::str::from_utf8(ENV_SOURCE).unwrap_or("?")
            ));
            return None;
        }
    };
    let size = inode.size as usize;
    if size > block.len() {
        logger.info(format_args!(
            "env-source: {} is {size} byte(s); only the first {} are read",
            core::str::from_utf8(ENV_SOURCE).unwrap_or("?"),
            block.len()
        ));
    }
    Some(&block[..size.min(block.len())])
}

/// ページの大きさ（H-a）。**関数の中に同じ定数が 3 つあるが、
/// ヒープの上端は関数の外で要るので、モジュールの高さに 1 つ置く。**
const HEAP_PAGE_SIZE: u64 = 4096;

/// いま走っているプロセスのヒープ（H-a。ADR-0044）。
///
/// # 深さの配列にしない
///
/// **最初は `MAX_EXCURSION_DEPTH` の配列にした。** **書く側（イメージを読む時点）と
/// 読む側（システムコールの中）で深さが違い、索引がずれた**（実測。
/// `brk(0)` が答えられなかった）。
///
/// **据える側が戻す形にする**——`crate::vfs::swap_current_files` と
/// `crate::syscall::set_user_window` と同じ形である（S9-b-3-2b から続く形）。
/// **`run_loaded_program` が入る直前に据え、戻ったら引き取る。**
/// **深さの算術が消えるので、ずれようが無い。**
///
/// **スロットごとに持つ（W1-c-3）。** **今のタスクのスロットで引く**（`crate::vfs` の
/// `CURRENT_FILES` と同じ形）。**既定の起動では必ずスロット 0 である**（W1-c-4 の
/// `concurrent-test` では、足した 1 本がスロット 1 を使う）。
static CURRENT_HEAP: [Locked<Heap>; crate::arch::x86_64::USER_TASK_SLOTS] =
    [const { Locked::new(Heap::EMPTY) }; crate::arch::x86_64::USER_TASK_SLOTS];

/// 載せた後に取ったフレームの数を足す（`crate::syscall` の `mmap` と `brk` の伸ばす側が呼ぶ。`ADR-0065` の (a)）。
///
/// **`mmap` が新しい領域へマップすると中間表を取るが、`AddressSpace::frames_taken` は載せた時点で測るので入らない。**
/// **破棄の会計の `taken` にこれを足す**——**さもないと `collected > taken` になる。** **載せた後にページや表を取る
/// 経路すべてのためである。** **`brk` の伸ばす側は、葉と、境を越えて新しく取った中間表を一緒に足す**（2026-10-03。
/// 空きフレームの差で数える）。**以前は `brk` が数えておらず、伸ばしたまま終わるプログラム（`/bin/ttfglyph`）で、破棄が
/// 集めた数が取った数を 129 上回り、正常に終わったのにシェルが「cannot run」と表示した**（`docs/troubleshooting.md` の
/// 2026-10-03 の項）。
///
/// **数はプロセスごとに持つ**（[`Heap::post_load_frames`]。2026-10-03）。**以前はスロットごとの `static` だった**——
/// **親と、親が `spawn` した子は同じスロットを使うので、子の破棄の会計が親の分を取ってしまう**（伸ばしたまま子を起動した
/// `syscall-test` の破壊テストの形で踏んだ。`brk` を数えるようになって表に出た）。**[`Heap`] は Ring 3 を走らせる間だけ
/// 据えられ、戻るときに親のものへ戻る**（`run_loaded_program`）ので、同じ置き場に入れれば混ざらない。
pub fn note_post_load_frames(count: usize) {
    with_current_heap(|heap| heap.post_load_frames += count);
}

/// 載せた後に取ったフレームのうち、アロケータへ直に返した数を引く（`brk` の縮める側が呼ぶ。2026-10-03）。
///
/// **縮める側が外した葉のフレームは、隔離を通らずにその場でアロケータへ戻る。** **破棄が集める数には入らないので、
/// 取った数からも引く**——**引かないと、伸ばして縮めたプログラムで `taken > collected` になる。** **中間表は外さない
/// ので引かない**（破棄が集める）。**0 を下回らない**（この数より多く返すことは無いが、数え方の誤りで会計を
/// 負にしない）。
pub fn note_post_load_frames_returned(count: usize) {
    with_current_heap(|heap| heap.post_load_frames = heap.post_load_frames.saturating_sub(count));
}

/// ヒープの下端と上端、載せた後に取ったフレームの数、`mmap` の次の番地（H-a）。**どれもプロセスごとで、Ring 3 を
/// 走らせる間だけ据えられる**（[`swap_current_heap`]）。
#[derive(Clone, Copy)]
pub struct Heap {
    /// イメージの末尾の次のページ。**`brk` はここより下げられない。**
    start: u64,
    /// いまの上端。
    break_at: u64,
    /// `brk` が越えられない上端（2026-10-05。配置ごとに違う。[`ProcessLayout`]）。
    limit: u64,
    /// 載せた後にアロケータから取ったフレームの数（`mmap` の中間表と、`brk` の葉と中間表。`ADR-0065` の (a)）。
    /// **破棄の会計が `AddressSpace::frames_taken` に足す**（[`note_post_load_frames`] の doc）。
    post_load_frames: usize,
    /// `brk` が取ったフレームの数（H-a）。
    ///
    /// # 空きフレームの全体を数えない
    ///
    /// **最初は遠征の前後で `free_frame_count()` を比べた。** **釣り合わなかった**
    /// ——**`syscall-test` は子を起動するので、子の空間のフレームが隔離
    /// （quarantine）へ入り、まだ空きへ戻っていない**（実測で 44 フレームの差）。
    ///
    /// **`brk` 自身が取った数と返した数を数える。** **他の活動に汚されない。**
    /// **主張は同じである**——ADR-0044 の到達条件（伸ばして縮めたら戻る）を、
    /// **測れる形にしたものである。**
    taken: u32,
    /// `brk` が返したフレームの数（H-a）。
    given: u32,
}

impl Heap {
    /// マップしていない状態。**`start` が 0 である。**
    pub const EMPTY: Self = Self {
        start: 0,
        break_at: 0,
        limit: HEAP_LIMIT,
        post_load_frames: 0,
        taken: 0,
        given: 0,
    };

    /// イメージの末尾から作る（H-a）。**次のページの先頭から始まる。**
    ///
    /// **固定のアドレスにしない**——**イメージの大きさはプログラムごとに違う**
    /// （実測で `hello` が `0x401012`、`zi` が `0x40a289`。ADR-0044）。
    ///
    /// **上端と、`mmap` の範囲は、配置から取る**（2026-10-05。[`ProcessLayout`]）。
    pub fn from_image_end(image_end: u64, layout: &ProcessLayout) -> Self {
        let start = (image_end + HEAP_PAGE_SIZE - 1) & !(HEAP_PAGE_SIZE - 1);
        Self {
            start,
            break_at: start,
            limit: layout.heap_limit,
            post_load_frames: 0,
            taken: 0,
            given: 0,
        }
    }

    /// `brk` が越えられない上端。
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// 下端。
    pub const fn start(&self) -> u64 {
        self.start
    }

    /// いまの上端。
    pub const fn break_at(&self) -> u64 {
        self.break_at
    }

    /// 上端を置く。**マッピングを変えた後で呼ぶ。**
    pub fn set_break(&mut self, value: u64) {
        self.break_at = value;
    }

    /// マップしているか。
    pub const fn is_mapped(&self) -> bool {
        self.start != 0
    }

    /// 取ったフレームを 1 つ数える（H-a）。
    pub fn note_taken(&mut self) {
        self.taken += 1;
    }

    /// 返したフレームを 1 つ数える（H-a）。
    pub fn note_given(&mut self) {
        self.given += 1;
    }

    /// 取った数と返した数（H-a）。**判定行に出す。**
    pub const fn frames(&self) -> (u32, u32) {
        (self.taken, self.given)
    }

    /// 載せた後に取ったフレームの数（破棄の会計が読む）。
    pub const fn post_load_frames(&self) -> usize {
        self.post_load_frames
    }
}

/// 今のヒープを据え、前のものを返す（H-a）。**`swap_current_files` と同じ形。**
pub fn swap_current_heap(heap: Heap) -> Heap {
    core::mem::replace(
        &mut CURRENT_HEAP[crate::arch::x86_64::current_excursion_slot()].lock(),
        heap,
    )
}

/// 今のヒープへ触る（H-a）。**`sys_brk` が使う。**
pub fn with_current_heap<R>(body: impl FnOnce(&mut Heap) -> R) -> R {
    body(&mut CURRENT_HEAP[crate::arch::x86_64::current_excursion_slot()].lock())
}

/// ヒープが越えられない上端（H-a。ADR-0044 の決定 4）。
///
/// **ユーザースタックの見張りのページの下端である**（2026-10-06 まではスタックの下端そのもので、見張りのページは
/// 無かった）。ヒープはここで断られ、スタックは見張りのページで止まる——**どちらが尽きても、もう一方を黙って壊さない。**
pub const HEAP_LIMIT: u64 = USER_PROGRAM_STACK_TOP - USER_PROGRAM_STACK_BYTES - HEAP_PAGE_SIZE;

/// ユーザースタックの未使用部分を埋める既知のバイト（EV。ADR-0041）。
///
/// # なぜ測るのか
///
/// **ユーザースタックは 1 ページで、`argv` と `envp` の文字列も同じページに
/// 載る。** **増やすかどうかを決めるには、プログラム自身がどれだけ使うかが
/// 要る**——**それを誰も測っていなかった**（遠征スタックには高水位が在るのに、
/// こちらには無かった。実測）。
///
/// # 遠征スタックと値を変えてある
///
/// あちらは `0xE5` である（`crate::arch::x86_64::ring3` の `EXCURSION_STACK_FILL`）。
/// **迷子の模様を見たときに、どちらのスタックから来たかが分かるようにする。**
///
/// # 限界
///
/// **プログラムがこの値そのものを書いたら、使ったとは数えられない。**
/// **遠征スタックの測りかたと同じ限界である**（あちらの doc に同じ注記がある）。
const USER_STACK_FILL: u8 = 0xA5;

/// 埋め込んだユーザープログラムのロードと実行が失敗する形（S9-b-2）。
///
/// # なぜ `Result` にしたか
///
/// **S9-b-1 では失敗のたびに `halt_forever` していた。** 相手が自分のビルドの
/// 作ったイメージだったので、壊れていればカーネルの不具合であり、止まるのが正しかった。
/// **S9-b-2 は壊したイメージを意図的に渡すので、止まってはいけない。**
///
/// **この段階では呼び出し側がまだ止める。** 既定の `hello` は成功するので、
/// 振る舞いは変わらない。壊したイメージを渡すのは S9-b-2 の 2 つ目である。
///
/// # `ElfError` の 11 種をどう扱うか
///
/// [`common::elf::ElfError`] が返るのは [`Self::Parse`]（`ElfHeaders::parse`）で、**そのまま持ち上げる。**
/// **ローダーは種類で分岐しない。** 内訳は次のとおりで、**すべて「像が壊れている」
/// に落ちる。**（2026-10-05 より前は、区画の中身を切り出す所にも同じ誤りの口が在った。像を範囲で読む形にして、
/// 区画のファイルの中の範囲は、ヘッダの検査の 1 か所だけが確かめる。）
///
/// - `TooShort` / `BadMagic` / `NotElf64` / `NotLittleEndian` / `NotExecutable` /
///   `NotX86_64`: ヘッダの形。**`parse` の最初の 6 つで、いずれも 1 バイトの
///   書き換えで作れる**
/// - `ProgramHeaderOutOfBounds` / `BadProgramHeaderEntrySize`: 表の位置と 1 エントリの
///   大きさ。**`e_phoff` と `e_phentsize` の書き換えで作れる**
/// - `SegmentFileRangeOutOfBounds` / `SegmentMemorySmallerThanFile` /
///   `SegmentAddressOverflow`: 区画の数値。**`p_offset` / `p_filesz` / `p_memsz` /
///   `p_vaddr` の書き換えで作れる**
///
/// **11 種とも、既定のイメージの 1 バイトから 8 バイトを書き換えれば作れる。**
/// S9-b-2 の 2 つ目で壊し方を選ぶときの材料である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserLoadError {
    /// フレームアロケータを借りられなかった（S11-3。`ADR-0030`）。
    ///
    /// **起動シーケンスは単一コアの直線なので、ここへ来ること自体が異常である**
    /// ——誰かが借りたまま返していない。
    AllocatorUnavailable,
    /// `argv` と表と文字列が、スタックの 1 ページに収まらない（S11-1）。
    ArgumentsTooLong,
    /// `Elf::parse` が拒んだ。**イメージがバイト列として壊れている。**
    Parse(common::elf::ElfError),
    /// 区画の並びを受け付けられない（2026-10-01。`common::elf::check_load_layout`）。**番地の順でない、
    /// 重なる、計算があふれる、同じページに権限の違う区画が載る、のどれかである。**
    /// **写す前に断るので、ページは 1 枚も写していない。**
    Layout(common::elf::LayoutError),
    /// 載せる位置を決められない（2026-10-05。`common::elf::ElfHeaders::plan`）。**動的リンクを求める、実行できる
    /// スタックを求める、`PT_LOAD` が無い、置いてよい範囲に収まらない、のどれかである。** 写す前に断るので、ページは
    /// 1 枚も写していない。
    Placement(common::elf::PlacementError),
    /// 像の範囲を読めなかった（2026-10-05。`common::image_source`）。**ファイルシステムが、そのブロックを読めない。**
    ///
    /// **大きすぎて読めないファイルは、ここへ来る**——ext2 の 2 段目の間接ブロックを使うファイル（約 4.05 MiB を
    /// 越える）は、最初の範囲から `Ext2(IndirectBlockUnsupported)` で断られる。**黙って途中で切らない。**
    Read(common::image_source::ImageReadError),
    /// 新しいアドレス空間を作れなかった。**イメージではなくカーネル側の事情である。**
    AddressSpace(crate::arch::x86_64::AddressSpaceError),
    /// フレームが尽きた。**イメージではなくカーネル側の事情である。**
    OutOfFrames,
    /// マップしようとした仮想アドレスが正準形でない。
    NotCanonical(u64),
    /// マッピングに失敗した。**区画が同じページを共有していると、後から来たほうがここへ来る。**
    Mapping {
        virt: u64,
        error: crate::arch::x86_64::AddressSpaceError,
    },
    /// マップした葉のフラグが、区画の権限と食い違った。**カーネル側の不具合である。**
    LeafFlags { count: usize },
    /// 終了せずに例外で終了させられた（S9-b-3-1）。**`exit` が効かなかったということである。**
    ///
    /// `hello` は `exit` の直後に `ud2` を置いてあるので、**効かなければ確定的に
    /// ここへ来る**（`HELLO_UD2_OFFSET` の doc）。
    DidNotExit,
    /// 終了でも例外による終了処理でもなく Ring 3 から戻ってきた。**カーネル側の不具合である。**
    ///
    /// `ring3::run_excursion` はこの 2 つの longjmp でしか戻らないので、**通常は構成でき
    /// ない。** 記録の側が壊れたときの受け皿である。
    NoExitNoFold,
    /// 終了状態が予期と違った。`hello` は 0 で終わる。
    ExitStatus(u64),
    /// 終了させられるはずのプロセスが、終了して戻った（S9-b-3-2a）。
    ///
    /// **起こすはずの違反が起きなかったということである。**
    DidNotFold,
    /// 終了させられたが、ベクタ・RIP・CS・CR2・エラーコードのどれかが予期と違った
    /// （S9-b-3-2a）。**どれが違うかは直前の ERROR 行に出ている。**
    FoldMismatch,
    /// ユーザーが組み立てた引数が、`ADR-0020` の規約どおりに届かなかった
    /// （S9-b-3-2a）。**probe が呼ばれなかった場合も含む。**
    AbiMismatch,
    /// 破棄の会計が合わなかった（S9-b-3-1）。**空間を破棄しても、消えたフレームが
    /// 隔離へ届いていない。**
    DestroyAccounting {
        consumed: usize,
        quarantined: usize,
        leaked: usize,
    },
    /// `write` が届けたバイト列が予期と違った。
    WriteMismatch,
}

/// ユーザープログラムの初期スタックを **Linux と同じ形で**積む（S11-1）。**形と積むものは
/// [`crate::abi::linux::build_initial_stack`] にある**（`ADR-0071` の決定 1 の 2 で分けた。2026-09-30）。ここは `argv` と
/// `envp` の数の上限を見て、スタックページを切り出して渡す。
///
/// 返すのは entry へ入るときの `rsp`（`argc` を指す）。
///
/// # Safety
///
/// `page` がスタックページの先頭を direct map 越しに指しており、
/// 4096 バイト書けること。単一実行文脈から呼ぶこと。**このページを指す参照がほかに無いこと**——
/// 呼び出し元が新しく確保し、まだ稼働していない空間にマップしただけで、どこにも渡していないページであること
/// （`argv` と `envp` の文字列も、このページを指さないこと）。
unsafe fn build_initial_stack(
    page: *mut u8,
    page_base: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &crate::abi::linux::Auxv<'_>,
) -> Option<u64> {
    /// スタックページの大きさ。**1 枚だけマップしてある**（呼び出し側）。
    const PAGE_SIZE: usize = 4096;

    if argv.len() > MAX_ARGV || envp.len() > MAX_ENVP {
        return None;
    }
    // SAFETY: 呼び出し元の契約により、`page` はスタックページの先頭を direct map 越しに指しており、`PAGE_SIZE`
    // バイト書ける。新しく確保して、まだどこにも渡していないページなので、切り出した `&mut [u8]` を使う間、
    // このページを指す参照はほかに無い（`argv` と `envp` もこのページを指さない）。
    let page = unsafe { core::slice::from_raw_parts_mut(page, PAGE_SIZE) };
    crate::abi::linux::build_initial_stack(page, page_base, argv, envp, aux)
}

/// 同時に飛べる `spawn` の本数（S11-5）。
///
/// # 引き算に理由がある
///
/// **`spawn` は遠征の中からしか呼べない**（`dispatch` へ来るのは Ring 3 からだけで、
/// Ring 3 は遠征の中にしかない）。**したがって呼ばれた時点の深さは 1 以上である。**
/// そして [`spawn`] は深さが [`MAX_EXCURSION_DEPTH`] 以上なら断るので、
/// **実際に子を起動できるのは深さ 1 から `MAX_EXCURSION_DEPTH - 1` までである。**
///
/// **その本数だけ緩衝を持てば、入れ子で上書きされない。**
/// **`MAX_EXCURSION_DEPTH` を上げれば、ここも自動で増える。**
///
/// **S11-11 で 1 本増えた。** `init` がカーネルの直線上（深さ 0）から
/// シェルを起動するようになったので、**深さ 0 から `MAX_EXCURSION_DEPTH - 1` まで
/// が起動する側になる。**
const MAX_SPAWN_IN_FLIGHT: usize = MAX_EXCURSION_DEPTH;

/// `spawn` が受け取ったパスを置く場所（S11-5）。
///
/// # `&'static str` が要る
///
/// [`load_user_program`] の `name` と `argv` は `&'static str` である
/// （判定行に出す名前と、初期スタックへ積む `argv[0]`）。**ユーザーから来た
/// パスはカーネルスタックのローカルなので、そのままでは渡せない。**
///
/// **スロットと深さごとに 1 本ずつ持つ**（[`MAX_SPAWN_IN_FLIGHT`]）。**深さだけで引くと、同時に走る 2 本が同じ深さの
/// 緩衝を使う**——`init` は深さ 0 から IF=1 のまま読み込むので、読み込みの途中でもタイマが切り替えうる（W1-c-1）。
static mut SPAWN_PATHS: [[[u8; PATH_MAX]; MAX_SPAWN_IN_FLIGHT];
    crate::arch::x86_64::USER_TASK_SLOTS] =
    [[[0; PATH_MAX]; MAX_SPAWN_IN_FLIGHT]; crate::arch::x86_64::USER_TASK_SLOTS];

/// [`spawn`] が起動した子が隔離へ入れたフレームの累計（S11-5）。
///
/// # なぜ要るのか。**親の会計が閉じなくなる**
///
/// 起動時の会計は「このプログラムを走らせる前後で空きフレームがいくつ減ったか」と
/// 「そのプログラムの空間を畳んで隔離へ何枚入れたか」を突き合わせる。
/// **子を起動すると、子のぶんも前者に乗る**——隔離へ入ったフレームは世代が退くまで
/// アロケータへ戻らないので、**親から見ると「消えたまま」である。**
///
/// **実測で踏んだ**（S11-5）。`syscall-test` が 2 本の子を起動したところ、
/// **24 枚消えて自分の隔離は 8 枚**になった。差の 16 枚が子 2 本のぶんである。
///
/// **子の側で数えて、親が足す。** 親が子の内訳を知る必要はない。
static SPAWN_QUARANTINED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 空間を破棄するときの隔離の置き場（B-d で静的へ移した）。
///
/// # なぜスタックに置かないか
///
/// **ここは遠征スタックの上である**（`run_loaded_program` は `spawn` の
/// 経路で遠征スタックに乗る）。**`Quarantine` は容量に比例して太る**——
/// **B-d で 64 から 256 へ上げたとき、遠征スタックの高水位が 30,376 から
/// 34,984 バイト（46% から 53%）へ跳ね、「半分を越えたら決める」という
/// 持ち越しの行が発火して起動が止まった**（実測。差の 4,608 バイトは
/// ちょうど `(256 - 64) * 24` である）。
///
/// **持ち越しの行は発火させない**——**遠征スタックの要件が変わったのでは
/// なく、ここが太っただけである。** **静的へ移せば、容量をいくつにしても
/// 遠征スタックは 1 バイトも増えない。**
///
/// # 同時に 2 つ走らない
///
/// **触るのは BKL の内側だけである。** **破棄は入れ子にならない**——
/// **子の破棄は、親の破棄が始まる前に終わっている**（`spawn` は同期である）。
/// **使う前に [`crate::quarantine::Quarantine::reset`] で空にする。**
///
/// # スロットごとに持つ（W1-c-3）
///
/// **同時に走る 2 本は、それぞれ自分の破棄を持つ。** **1 つだけだと、片方の `reset` が
/// もう片方の途中の破棄を空にする。** **今のタスクのスロットで引く。** **既定の起動では必ずスロット 0
/// である**（W1-c-4 の `concurrent-test` では、足した 1 本がスロット 1 を使う）。
static mut SPAWN_QUARANTINE: [crate::quarantine::Quarantine; crate::arch::x86_64::USER_TASK_SLOTS] =
    [const { crate::quarantine::Quarantine::new() }; crate::arch::x86_64::USER_TASK_SLOTS];

/// [`spawn`] が起動した子が漏らしたフレームの累計（S11-5）。
static SPAWN_LEAKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// [`spawn`] が起動した子の破棄が、隔離に残っていた前の破棄のフレームを返した本数の累計（2026-10-07。
/// `crate::quarantine::Retired::released_others`）。**親の窓の中で空きフレームが増える**ので、親は消えた数へ足し戻す。
static SPAWN_RELEASED_OTHERS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// 子の会計を 0 に戻す（S11-5）。**プログラムを 1 本走らせる直前に呼ぶ。**
pub fn reset_spawn_accounting() {
    SPAWN_QUARANTINED.store(0, core::sync::atomic::Ordering::SeqCst);
    SPAWN_LEAKED.store(0, core::sync::atomic::Ordering::SeqCst);
    SPAWN_RELEASED_OTHERS.store(0, core::sync::atomic::Ordering::SeqCst);
}

/// 子の破棄が、隔離に残っていた前の破棄のフレームを返した本数の累計（2026-10-07）。
pub fn spawn_released_others() -> usize {
    SPAWN_RELEASED_OTHERS.load(core::sync::atomic::Ordering::SeqCst)
}

/// 子が隔離へ入れた枚数と漏らした枚数（S11-5）。
pub fn spawn_accounting() -> (usize, usize) {
    (
        SPAWN_QUARANTINED.load(core::sync::atomic::Ordering::SeqCst),
        SPAWN_LEAKED.load(core::sync::atomic::Ordering::SeqCst),
    )
}

/// 破棄の結果（スロットごと。2026-10-07）。**[`run_loaded_program`] が書き、会計（`spawn` と起動時の表）が
/// [`last_destroy`] で読む。**
static mut LAST_DESTROY: [crate::quarantine::Retired; crate::arch::x86_64::USER_TASK_SLOTS] =
    [crate::quarantine::Retired::ZERO; crate::arch::x86_64::USER_TASK_SLOTS];

/// 隔離（[`SPAWN_QUARANTINE`]）にフレームが残っているか（2026-10-07）。**BSP のアイドルが BKL を取らずに読む印**——
/// 残っていれば BKL を取って [`release_retired_quarantines`] を呼ぶ。隔離の道を通った破棄の後だけ立つ。
static QUARANTINE_HOLDS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 今まさに空間を壊している数（2026-10-07）。**隔離の道は、一杯になると BKL を放して待つ**ので、その間にアイドルが
/// 同じ隔離を触らないように、[`release_retired_quarantines`] はこれが 0 のときだけ動く。
static DESTROYS_IN_PROGRESS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// 空間をどの道で壊すか（2026-10-07。`crate::quarantine::Retire` の doc）。
fn destroy_route(ran_on: u64) -> crate::quarantine::Retire {
    // 確かめ (2026-10-07, destroy-through-quarantine-test): どの空間も隔離の道を通す（一杯になって待つ形を踏ませる）。
    if cfg!(feature = "destroy-through-quarantine-test") {
        return crate::quarantine::Retire::ThroughQuarantine;
    }
    let here = 1u64 << common::percpu::cpu_id();
    if ran_on & !here == 0 {
        crate::quarantine::Retire::Directly
    } else {
        crate::quarantine::Retire::ThroughQuarantine
    }
}

fn note_destroy(slot: usize, retired: crate::quarantine::Retired) {
    // SAFETY: BKL の内側で、自分のスロットの欄だけを書く。
    unsafe { (*core::ptr::addr_of_mut!(LAST_DESTROY))[slot] = retired };
    if retired.still_quarantined() > 0 {
        QUARANTINE_HOLDS.store(true, core::sync::atomic::Ordering::SeqCst);
    }
}

/// 今のスロットの、直近の破棄の結果（2026-10-07）。
pub fn last_destroy() -> crate::quarantine::Retired {
    let slot = crate::arch::x86_64::current_excursion_slot();
    // SAFETY: 自分のスロットの欄を読むだけ。書くのは同じスロットの破棄だけである。
    unsafe { (*core::ptr::addr_of!(LAST_DESTROY))[slot] }
}

/// 隔離にフレームが残っているか（BKL を取らずに読める印。2026-10-07）。
pub fn quarantine_holds_frames() -> bool {
    QUARANTINE_HOLDS.load(core::sync::atomic::Ordering::SeqCst)
}

/// 隔離に残るフレームのうち、世代が退いたものを返す（2026-10-07。BSP のアイドルの定常経路から）。**BKL を保持したまま
/// 呼ぶこと。** 返した本数を返す。アロケータが借りられているか、破棄の途中なら何もしない。
pub fn release_retired_quarantines() -> usize {
    if DESTROYS_IN_PROGRESS.load(core::sync::atomic::Ordering::SeqCst) != 0 {
        return 0;
    }
    let Some(allocator) = crate::frame_allocator::take() else {
        return 0;
    };
    let mut released = 0;
    let mut remaining = 0;
    // SAFETY: BKL の内側で、破棄の途中でない（上で確かめた）ので、ほかに触る者は居ない。
    let quarantines = unsafe { &mut *core::ptr::addr_of_mut!(SPAWN_QUARANTINE) };
    for quarantine in quarantines.iter_mut() {
        released += quarantine.release_retired(allocator, crate::bkl::generation_is_retired);
        remaining += quarantine.held_count();
    }
    crate::frame_allocator::give_back(allocator);
    if remaining == 0 {
        QUARANTINE_HOLDS.store(false, core::sync::atomic::Ordering::SeqCst);
    }
    released
}

/// `spawn` が受け取った `argv` を置く場所（S11-7）。
///
/// **NUL 区切りで並べたバイト列である。** [`load_user_program`] が要求するのは
/// `&[&[u8]]` で、**要素は `'static` でなければならない**（[`SPAWN_PATHS`] と
/// 同じ理由）。**スロットと深さごとに 1 本ずつ持つ**（[`MAX_SPAWN_IN_FLIGHT`]。
/// スロットは W1-c-1 で足した。[`SPAWN_PATHS`] の doc）。
static mut SPAWN_ARGVS: [[[u8; MAX_ARGV_BYTES]; MAX_SPAWN_IN_FLIGHT];
    crate::arch::x86_64::USER_TASK_SLOTS] =
    [[[0; MAX_ARGV_BYTES]; MAX_SPAWN_IN_FLIGHT]; crate::arch::x86_64::USER_TASK_SLOTS];

/// `spawn` が受け取った `envp` を置く場所（f-2。`ADR-0053`）。
///
/// **[`SPAWN_ARGVS`] と同じ形である**——**NUL 区切りで並べたバイト列を、
/// スロットと深さごとに 1 本ずつ持つ。** **別に持つ理由は、`argv` と `envp` の上限が
/// 別の理由で決まっているからである**（語の数と、環境の本数）。スロットは W1-c-1 で
/// 足した（[`SPAWN_PATHS`] の doc）。
static mut SPAWN_ENVPS: [[[u8; MAX_ENVP_BYTES]; MAX_SPAWN_IN_FLIGHT];
    crate::arch::x86_64::USER_TASK_SLOTS] =
    [[[0; MAX_ENVP_BYTES]; MAX_SPAWN_IN_FLIGHT]; crate::arch::x86_64::USER_TASK_SLOTS];

/// [`spawn`] が拒む形（S11-5）。
///
/// **`errno` を知らない。** 変換するのは `crate::syscall` の側である
/// （[`UserLoadError`] と同じ線。module の doc）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// 遠征の深さが上限に達している。**これ以上は入れ子にできない。**
    TooDeep,
    /// パスを引けなかった（無い、途中がディレクトリでない、像が壊れている）。
    Lookup(common::ext2::Ext2Error),
    /// 引けたがディレクトリだった。
    IsDirectory,
    /// 引けたが通常ファイルではなかった（デバイスファイル等）。
    NotRegularFile,
    /// コピーした `argv` のバイト列が、要素数と食い違った（S11-7）。
    ///
    /// **カーネル側の不具合である**——`copy_user_string_array` は要素ごとに NUL を付けて
    /// 並べるので、**要素数だけ NUL があるはずである。**
    ArgvMalformed,
    /// イメージが [`MAX_EXECUTABLE_SIZE`] に収まらない。
    TooLarge(u64),
    /// 像を読めなかった（`common::image_source`）。**大きすぎて読めないファイルも、ここへ来る**（ext2 の 2 段目の
    /// 間接ブロック）。
    Read(common::image_source::ImageReadError),
    /// 載せられなかった、または期待どおりに終わらなかった。
    Load(UserLoadError),
    /// 子を破棄した会計が合わなかった。**カーネル側の不具合である。**
    DestroyAccounting {
        consumed: usize,
        quarantined: usize,
        leaked: usize,
    },
}

/// 子プロセスの終わり方（S11-5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// `exit(status)` で終わった。
    Exited(u64),
    /// Ring 3 の違反が終了処理されて終わった。**ベクタを持つ。**
    Folded(u64),
    /// 外から止められた（Ctrl+C。S12 前の手当て、C）。
    ///
    /// **`Folded` と分けてある。** あちらは**子が違反した**で、
    /// **こちらは子に落ち度が無い。** 混ぜると、判定行から
    /// 「落ちた」と「止めた」の区別が付かなくなる。
    ///
    /// **ベクタを持たない。** 止めた地点はタイマ割り込みで、
    /// **どのベクタで止めたかは子について何も語らない。**
    Interrupted,
}

/// ページの権限の一覧に、名前を付けて出す区画の数。
const MAX_REPORTED_SEGMENTS: usize = 8;

/// 走らせるプロセス 1 つ分（S9-b-3-1）。
///
/// # 表を持たない
///
/// **起動時の 3 本は、同時に 1 つずつ生きる。** 3 本を順に走らせ、1 本ずつ
/// 終わらせる。**表を先に作るのは先回りの抽象化になる**（`vision.md` の規定）。
/// **以前ここは「同時に生きているプロセスは 1 つである」と言い切っていた**——**S11 の入れ子の
/// `spawn` で親と子が並び、W1-c-4 で 2 本が同時に走る。** **表はまだ作っていない**（各 `UserProcess` は
/// 起動した側のフレームが持つ）。
/// 同時生存が複数になる段階（S11 のシェル）で表にすればよく、**そのときこの型は
/// そのまま使える。**
///
/// # 「走らせるプログラムの一覧」とは別物である
///
/// あちらは `USER_PROGRAMS`（`kernel/src/main.rs`）で、**静的な記述**である（`TASK_COUNT` や
/// `build.rs` の `PROGRAMS` と同じ性質）。**混ぜると、一覧の長さが管理構造の
/// 容量に見える。** こちらは管理構造で、長さは常に 1 である。
pub struct UserProcess {
    /// このプロセスのアドレス空間。**終了で破棄する。**
    space: crate::arch::x86_64::AddressSpace,
    /// 最初に飛ぶ先（ELF の entry）。
    entry: u64,
    /// 像の始まり（最初の区画が載るページの先頭。ずらした後。2026-10-06。写像の表に登録する）。
    image_base: u64,
    /// ユーザースタックの上端。
    stack_top: u64,
    /// このプロセスのヒープ（H-a。ADR-0044）。
    ///
    /// **`run_loaded_program` が [`swap_current_heap`] で据え、戻ったら
    /// 引き取る**（`files` と同じ形）。
    heap: Heap,
    /// ユーザースタックのページを、direct map 越しに指すアドレス（EV）。
    ///
    /// # なぜ持つのか
    ///
    /// **戻ってから高水位を読むためである**（[`USER_STACK_FILL`]）。
    /// **プログラムが走っている間は CR3 が別なので、ユーザー VA では読めない。**
    /// **direct map はどの空間からも同じ場所を指すので、こちらを控える。**
    ///
    /// **0 は「まだ張っていない」である**（[`load_user_program`] が 0 で作り、
    /// マップした側が埋める）。
    stack_scratch: u64,
    /// 像の種類（2026-10-05）。**番地の配置は、ここから決まる**（[`ProcessLayout::for_kind`]）。像を読むまでは
    /// `Executable` が入っている。
    ///
    /// **配置の値そのものは持たない。** この値は、プログラムが走っている間ずっと、遠征スタックの上に在る。遠征スタックの
    /// 使用量は、容量の半分の手前に在る（`docs/deferred-decisions.md` の「遠征スタックにガードページが無い」）。
    kind: common::elf::ElfKind,
    /// このプロセスが走った CPU の印（ビット = CPU の番号。2026-10-07）。**空間を壊すときに、その場で返してよいか
    /// （この CPU でしか走らなかったか）を決める**（`crate::quarantine::Retire` の doc）。Ring 3 へ入る所で立てる。
    /// **スレッド（M3a）を入れるときは、別の CPU で走らせる所でも立てること**——立て忘れると、ほかの CPU の TLB に
    /// 翻訳が残ったままフレームが配られる。
    ran_on: u64,
    /// 判定行に出す名前。
    name: &'static str,
    /// このプロセスが開いているファイルの表（S10-b）。
    ///
    /// # まだ誰も開かない
    ///
    /// **この手順では表を置くだけである。** 開くのは次の手順（`open`/`close`）で、
    /// **ここが未使用なのはそのためである。** `#[allow(dead_code)]` を付けている
    /// のは、**「要らないものを置いた」のではなく「使い方をまだ実装していない」**
    /// 側だからである（S9-b-3-1 で立てた判定。未使用の警告はそのどちらかを指す）。
    /// # プロセスの持ち物である
    ///
    /// **ここへ置いたのは S10-b で、そのときは同時に生きているプロセスが 1 つだった。**
    /// **固定配列なので費用が変わらず、グローバルに置くと S11 で作り直しになる**と見て、ここへ置いた。
    /// **実際に S11 で親と子が並び、W1-c-4 で 2 本が同時に走る**——**作り直しは要らなかった。**
    ///
    /// # 遠征の間だけ `crate::vfs` へ据える
    ///
    /// **`syscall::dispatch` はプロセスを知らない**ので、Ring 3 へ落ちる直前に
    /// [`crate::vfs::swap_current_files`] で据え、戻ったら引き取る
    /// （`syscall::set_user_window` と同じ形である）。
    files: crate::vfs::FileTable,
}

/// イメージを 1 つ、専用のアドレス空間へ載せて（`run` なら走らせて）破棄する（S9-b-2）。
///
/// **失敗しても空間を破棄する。** 途中で落ちた場合、**そこまでに作ったマッピングと
/// 中間テーブルが残っている。** 破棄せずに戻ると、そのフレームは誰にも
/// 返らない。**「壊した像でカーネルが止まらない」は、後始末まで含めて
/// 初めて言える。**
///
/// 破棄した結果（隔離へ入れた本数と、隔離が溢れて漏らした本数）を返す。
/// **後始末が正しいことは、この会計で主張する。**
///
/// # `envp` は「誰が積むか」で分かれる（f-2。`ADR-0053`）
///
/// **`None` なら起動時の環境を積む**——**カーネルの表である。**
/// **`init` が起動する `/bin/zash` と、起動シーケンスの直線上のプログラムが
/// これに当たる。**
///
/// **`Some` なら親が渡したものを積む**——**シェルが `spawn` の第 3 引数で
/// 渡した表である。** **`export` で変えられるのはこちらだけである。**
///
/// # 終わり方は判定しない（S9-b-3-2a）
///
/// 走らせた場合、返すのはイメージの entry である。**どう終わったかの判定は
/// `check_user_program_outcome`（`kernel/src/main.rs`）が行う**——プログラムごとに正しい終わり方が
/// 違い、それは呼び出し側の知識だからである（`ring3::run_excursion` が終了させた位置を
/// 主張しないのと同じ形）。`run` が偽なら 0 を返す。
pub fn load_user_program(
    logger: &mut Logger<Serial>,
    image: &[u8],
    run: bool,
    name: &'static str,
    argv: &[&[u8]],
    envp: Option<&[&[u8]]>,
) -> (Result<u64, UserLoadError>, usize, usize, usize) {
    // **メモリに在る像も、範囲を読む口を通す**（2026-10-05）。載せる道は 1 本である。
    load_user_program_from(
        logger,
        &common::image_source::SliceImage(image),
        run,
        name,
        argv,
        envp,
    )
}

/// [`load_user_program`] の本体（2026-10-05）。**像は「範囲を読む口」で受ける**（`common::image_source`）。
///
/// **実行ファイルの全体を、1 本のバイト列として持たない。** 読むのは、先頭の部分（ELF のヘッダとプログラムヘッダ）と、
/// 区画ごとの、ページに写す分だけである。ファイルシステムの上のファイルは、ブロックごとに引いて、写す先のフレームへ
/// 直に読む（[`spawn`]）。
pub fn load_user_program_from(
    logger: &mut Logger<Serial>,
    image: &dyn common::image_source::ImageSource,
    run: bool,
    name: &'static str,
    argv: &[&[u8]],
    envp: Option<&[&[u8]]>,
) -> (Result<u64, UserLoadError>, usize, usize, usize) {
    use crate::arch::x86_64::AddressSpace;

    let direct_map = common::addr::direct_map();

    // **アロケータを借りる（S11-3。`ADR-0030`）。** マッピングの間だけ持ち、
    // **Ring 3 へ落ちる前に返す。**
    let Some(allocator) = crate::frame_allocator::take() else {
        return (Err(UserLoadError::AllocatorUnavailable), 0, 0, 0);
    };

    // 破壊テスト (`ADR-0063` の (b3), spawn-detached-returns-early): **切り離して起動するスロット（左）の
    // 読み込みが、貸し出しを持ったまま 2 ティック空回りする。** **起動する入口が入場を待たずに譲る形と
    // 組で、貸し出しが重なる機会を作る**——**このタスクはカーネルのタスクで IF=1 なので、
    // ティックでシェルへ切り替わり、シェルが右を `spawn` して `AllocatorUnavailable` に当たる。**
    // **右（スロット 0。システムコールの中）に置くと IF=0 で永久に空回りする**（実測）。
    // **`wait-window-is-wide` と同じ「機会を作る」形である。**
    #[cfg(feature = "spawn-detached-returns-early")]
    if crate::arch::x86_64::current_excursion_slot() == crate::task::detached_slot() {
        let opened = crate::arch::x86_64::monotonic_ticks();
        while crate::arch::x86_64::monotonic_ticks().saturating_sub(opened) < 2 {
            core::hint::spin_loop();
        }
    }

    // SAFETY: direct_map は登録済みのウィンドウ（コピー元は、`new` が稼働中の表を自分で読む）。**起動の後なので、カーネル側の
    // PML4 の項目は誰も変えない**（`AddressSpace::new` の前提。起動の終わりの指紋と突き合わせる）。BKL は持っていない。
    let space =
        match unsafe { AddressSpace::new(allocator, direct_map, USER_PROGRAM_SUBTREE_INDEX) } {
            Ok(space) => space,
            Err(e) => {
                // **失敗の経路でも返す（S11-3）。** ここで持ったまま抜けると、
                // 以後の確保がすべて `None` になる。**S9-b-2 で「失敗の途中で取った
                // フレームは呼び出し側が返す」と決めた場所と同じ関数で、
                // 今度はアロケータ自体を返す。**
                crate::frame_allocator::give_back(allocator);
                return (Err(UserLoadError::AddressSpace(e)), 0, 0, 0);
            }
        };

    // **ここからプロセスである。** stack_top はマップする前から決まっているが、entry は
    // イメージを読むまで分からないので、0 で作り load_user_program_into が埋める。
    let mut process = UserProcess {
        space,
        entry: 0,
        image_base: 0,
        stack_top: USER_PROGRAM_STACK_TOP,
        stack_scratch: 0,
        heap: Heap::EMPTY,
        kind: common::elf::ElfKind::Executable,
        ran_on: 0,
        name,
        files: {
            // **次の `spawn` へ渡す端を据える（`ADR-0063` の (b3)）。** **`a | b` の右は
            // fd 0 が読み端、左は fd 1 が書き端になる。** **端末の欄を差し替える。**
            let mut files = crate::vfs::FileTable::new();
            let slot = crate::arch::x86_64::current_excursion_slot();
            if let Some(pipe) = take_inherit_stdin(slot) {
                files.replace(crate::vfs::STDIN_FD, crate::vfs::File::PipeRead { pipe });
            }
            if let Some(pipe) = take_inherit_stdout(slot) {
                files.replace(crate::vfs::STDOUT_FD, crate::vfs::File::PipeWrite { pipe });
            }
            files
        },
    };

    // **マッピングまではアロケータが要る。遠征では要らない。**
    let mapped = load_user_program_into(logger, allocator, image, &mut process, argv, envp);
    // **ここで返す。** 以降は Ring 3 の遠征があり、**その間はアロケータが
    // `static` に在るので、システムコールから取り出せる**（`ADR-0030` の要）。
    //
    // **失敗の経路でも必ず通る**——`mapped` はまだ判定していない。
    crate::frame_allocator::give_back(allocator);

    let outcome = mapped
        .and_then(|()| {
            if run {
                // SAFETY: マッピングは済んでおり、entry と stack はマップしたユーザーページ。
                unsafe { run_loaded_program(logger, &mut process) }
            } else {
                Ok(())
            }
        })
        .map(|()| process.entry);

    // **`brk` が取った数と返した数を出す（H-a。ADR-0044 の到達条件）。**
    //
    // **空きフレームの全体を数えない**——**子を起動するプログラムでは、
    // 子の空間のフレームが隔離へ入り、まだ空きへ戻っていない**
    // （実測で 44 フレームの差が出た）。**`brk` 自身を数えれば、他の活動に
    // 汚されない。**
    //
    // **伸ばして縮めないプログラムでは、当然合わない**（取っただけで終わる）。
    // **合わないことを主張しない——出すだけである。** **判定はホスト側が行う**
    // （`syscall-test` は伸ばして縮めるので、そこだけが一致を主張する）。
    if run {
        let (taken, given) = process.heap.frames();
        logger.info(format_args!(
            "user-heap: {} had brk take {taken} frame(s) and give back {given} \
             (equal means the shrink actually returned them; a program that only grows \
             will not be equal, and that is not a failure)",
            process.name
        ));
        // **`mmap` の番地を出す。** **写像の表はプロセスごとに持ち、どのプロセスも基点から配り始める**
        // （`crate::mappings`。2026-10-06 に、次の番地を 1 つ持つ形から、表へ替えた）。**`mmap` しなかった
        // プロセスでは出さない**——起動ログの行を増やさない。**判定はホスト側が行う**（`socket-test`。2 本の
        // プロセスが同じ共有メモリを順にマップする）。行の形は変えていない——「次に配る番地」は、写像の
        // いちばん高い終わりである（空いた所を使い直す形になったので、次の `mmap` がそこへ行くとは限らない）。
        let summary = crate::mappings::with_loaded(|map| map.summary());
        if let (Some(first), Some(next)) = (summary.first, summary.highest) {
            logger.info(format_args!(
                "user-mmap: {} had its first mmap at {first:#x} and would map next at {next:#x} \
                 (every process starts at {:#x}; a first address above it means the addresses \
                 were shared with another process); {} mapping(s) still held, {} anonymous page(s)",
                process.name,
                crate::syscall::MMAP_BASE,
                summary.count,
                summary.anonymous_pages
            ));
        }
        // **書き戻しの計測（P-c-1）。**
        //
        // **回数と量は揺れない**（書きで開いたファイルを閉じた数と、イメージの長さで決まる）。
        // **サイクルと `hlt` の数は揺れる**ので `(info)` の側に置く
        // ——**判定に載せない**（`docs/coding-standards.md` の「揺れる値と主張は、
        // 同じ行に載せない」）。
        let (flushes, flushed_bytes, cycles, halts) = crate::virtio::take_flush_stats();
        if flushes > 0 {
            logger.info(format_args!(
                "user-flush: {} wrote the image back {flushes} time(s), {flushed_bytes} byte(s)",
                process.name
            ));
            logger.info(format_args!(
                "user-flush: {} spent {cycles} cycle(s) and {halts} halt(s) on those writes \
                 (both vary with the host and the device; they are not judged)",
                process.name
            ));
        }
    }

    // **成否によらず破棄する。** 破棄は S7-d の経路（下位を隔離へ入れ、世代が
    // 退くまで返さない）をそのまま通る。**プロセスが終了したなら、破棄するのはここ
    // である**（S9-b-3-1。終了の記録は `syscall` 側、空間の始末はこちら）。
    //
    // 破壊テスト (S9-b-3-1, user-exit-keep-space): 破棄しない。**消えたフレーム数と隔離へ
    // 入れた数の会計が合わなくなり、呼び出し側が検出する**（`AddressSpace` は
    // `Drop` を持たないので、落とすだけではフレームは戻らない）。
    //
    // **走らせたときだけ飛ばす。** 壊したイメージの後始末（S9-b-2）はこの破壊テストの対象では
    // なく、そちらまで飛ばすと**あちらの会計が先に落ちて、終了の側を観測できない。**
    // **実測で踏んだ**——先に落ちるほうだけを見ていた。
    // **破棄の前に、この空間が取った本数を聞く（`ADR-0063` の (b1)）。**
    // **`AddressSpace::detach` は自分を取るので、後からは聞けない。**
    // **載せた後に `mmap` と `brk` が取ったフレームを足す（`ADR-0065` の (a)）。** **`frames_taken` は
    // 載せた時点の分だけなので、これを足さないと `collected > taken` になる。** **数はこのプロセスの `Heap` に
    // 在る**（走らせた後は `process.heap` へ戻っている。[`note_post_load_frames`] の doc）。
    let taken = process.space.frames_taken() + process.heap.post_load_frames();
    // **破棄する前に、この空間のページの権限の一覧を出す**（`crate::page_survey`。`ADR-0071` の手順 3 の道具）。
    report_user_mappings(logger, image, &process, direct_map);
    let keep_space = cfg!(feature = "user-exit-keep-space") && run;
    let (held, leaked) = if keep_space {
        (0, 0)
    } else {
        let mut bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop));
        let slot = crate::arch::x86_64::current_excursion_slot();
        // SAFETY: BKL を保持している。**隔離はスロットごとで、破棄は入れ子にならない**（[`SPAWN_QUARANTINE`] の doc）。
        // **空にはしない**（2026-10-07）——前の破棄が隔離の道を通していれば、退いた分を破棄の始めに返す。
        let quarantine = unsafe { &mut (*core::ptr::addr_of_mut!(SPAWN_QUARANTINE))[slot] };
        // **破棄する前に、この空間を指したままのタスクが無いかを見る（W1-b-2）。**
        // **遠征の戻りが元の値へ戻していれば、何も見つからない。** **見つかったら
        // 不変条件が破れかけていたので、消してから声を出す**——**死んだテーブルを
        // 載せる形は静かに効くので、黙って直さない。**
        forget_task_root_before_destroy(logger, &process);
        // **この CPU でしか走らなかった空間は、その場で返す。ほかの CPU でも走った空間は隔離を通す**（2026-10-07。
        // `crate::quarantine::Retire` の doc）。
        let how = destroy_route(process.ran_on);
        DESTROYS_IN_PROGRESS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        // **アロケータは使うときだけ借りる**（`Frames::OnLoan`。待つ間は持たない）。借りられなければ、隔離の道へ倒れる。
        // SAFETY: この空間はどのコアでも稼働していない。direct map は覆っている。BKL を持っている。
        let retired = unsafe {
            crate::quarantine::retire_address_space(
                process.space,
                direct_map,
                how,
                quarantine,
                &mut crate::quarantine::Frames::OnLoan,
                &mut bkl,
            )
        };
        DESTROYS_IN_PROGRESS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
        note_destroy(slot, retired);
        drop(bkl);
        // **大きな空間の破棄は、道と掛かった時間を出す**（1,024 本以上。既定の起動には出ない）。揺れる値なので判定には使わない。
        if retired.collected() >= 1024 {
            logger.info(format_args!(
                "user-destroy: {} returned {} frame(s) at once and quarantined {} ({} wait(s), {} released \
                 meanwhile, {} leaked) in {} cycle(s)",
                process.name,
                retired.returned,
                retired.quarantined,
                retired.waits,
                retired.released_meanwhile,
                retired.leaked,
                retired.cycles
            ));
        }
        (retired.returned + retired.quarantined, retired.leaked)
    };

    // **空間ごとの会計（`ADR-0063` の (b1)）。** **取った本数と、破棄が集めた本数が
    // 一致すること。** **大域の空きフレーム数の差と違って、2 本のウィンドウが交差しても閉じる。**
    //
    // **破壊テスト `user-exit-keep-space` では破棄しないので、集めた本数が 0 になって落ちる**
    // ——**大域の差の側と同じ形で検出される。**
    let collected = held + leaked;
    if !keep_space && collected != taken {
        SPACE_ACCOUNTING_MISMATCHES.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        logger.error(format_args!(
            "user-space: the space of {name} took {taken} frame(s) but the destroy collected \
             {collected} ({held} quarantined + {leaked} leaked)"
        ));
    }
    (outcome, held, leaked, taken)
}

/// 破棄する前の空間のページの権限の一覧を出す（`crate::page_survey`）。
///
/// **領域は、像の区画（`PT_LOAD`）・スタック・ヒープ・`mmap` の範囲である。** 区画の範囲は像を読み直して得る
/// （読み込みが通った像だけを見る。区画の並びを受け付けられない像は、何も写していないので出さない）。
/// **歩くのはユーザーの側の添字だけである**（カーネルと共有している側は、カーネルの表の一覧が見る）。
fn report_user_mappings(
    logger: &mut Logger<Serial>,
    image: &dyn common::image_source::ImageSource,
    process: &UserProcess,
    direct_map: common::addr::DirectMap,
) {
    use crate::page_survey::Region;

    // **一覧を出さない回は、何もしない**（起動時のプログラムが済んだ後の、既定のビルド）。像を読み直さずに済む。
    if !crate::page_survey::user_report_wanted() {
        return;
    }

    /// 区画の名前。
    const SEGMENT_NAMES: [&str; MAX_REPORTED_SEGMENTS] = [
        "segment 0",
        "segment 1",
        "segment 2",
        "segment 3",
        "segment 4",
        "segment 5",
        "segment 6",
        "segment 7",
    ];

    // **区画の範囲は、像の先頭の部分を読み直して得る**（2026-10-05）。像の全体は手元に無いので、載せたときと同じに、
    // フレームを 1 枚借りて先頭を読む。**載せたときに控えておく形は採らなかった**——控えは、プログラムが走っている間
    // ずっと遠征スタックの上に在り、使用量が容量の半分を越えた（実測。`docs/deferred-decisions.md` の
    // 「遠征スタックにガードページが無い」）。
    let mut regions = [Region::mapped("", 0, 0, false); SEGMENT_NAMES.len() + 3];
    let mut count = 0;
    {
        const PAGE_SIZE: u64 = 4096;
        let Some(allocator) = crate::frame_allocator::take() else {
            return;
        };
        let Some(head_frame) = allocator.allocate_frame() else {
            crate::frame_allocator::give_back(allocator);
            return;
        };
        let head_len = image.len().min(PAGE_SIZE) as usize;
        let head_at = direct_map.phys_to_virt(head_frame).as_u64() as *mut u8;
        // SAFETY: いま取ったフレームで、direct map が覆っている。下で返すまで、この関数だけが持つ。長さは 1 ページ
        // ちょうどである。`u8` はどのビット列も妥当である。
        let head = unsafe { core::slice::from_raw_parts_mut(head_at, PAGE_SIZE as usize) };
        let read = image.read_at(0, &mut head[..head_len]);
        if read.is_ok() {
            // **読み込みが通った像だけを見る。** 検査も、置く位置の計画も、載せたときと同じものを通す。
            if let Ok(elf) = common::elf::ElfHeaders::parse(&head[..head_len], image.len()) {
                let layout = ProcessLayout::for_kind(elf.kind());
                if elf.check_load_layout().is_ok() {
                    if let Ok(plan) = elf.plan(&layout.load_policy(elf.kind())) {
                        for (name, ph) in SEGMENT_NAMES.iter().zip(elf.load_segments()) {
                            let start = ph.p_vaddr + plan.bias;
                            regions[count] = Region::mapped(name, start, start + ph.p_memsz, true);
                            count += 1;
                        }
                    }
                }
            }
        }
        let _ = allocator.deallocate_frame(head_frame);
        crate::frame_allocator::give_back(allocator);
    }
    if count == 0 {
        return;
    }
    // **スタックと、ヒープの上端と、`mmap` の範囲は、配置から取る**（2026-10-05）。位置を決めてリンクした像では、
    // 今までと同じ値である。
    let layout = ProcessLayout::for_kind(process.kind);
    let stack = layout.stack_bottom();
    regions[count] = Region::mapped("stack", stack, layout.stack_top, true);
    count += 1;
    // ヒープは像の末尾の次のページから、スタックの手前までを範囲にする（`brk` が伸ばした分だけが写っている）。
    let heap = process.heap.start();
    if heap != 0 && heap < layout.heap_limit {
        regions[count] = Region::mapped("heap", heap, layout.heap_limit, true);
        count += 1;
    }
    regions[count] = Region::mapped(
        "mapped by request",
        layout.mmap_base,
        layout.mmap_limit,
        true,
    );
    count += 1;

    /// 時点の名前。
    struct Moment(&'static str);
    impl core::fmt::Display for Moment {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "user program {}", self.0)
        }
    }

    // SAFETY: この空間の根は有効で、直接写像が配下の表を覆っている。読み取りのみ。この空間はいま稼働して
    // おらず、破棄もこの後なので、歩いている間に表は変わらない。
    unsafe {
        crate::page_survey::report_user(
            logger,
            &Moment(process.name),
            process.space.root(),
            direct_map,
            USER_PROGRAM_SUBTREE_INDEX..USER_PROGRAM_SUBTREE_INDEX + 1,
            &regions[..count],
        )
    };
}

/// 空間ごとの会計が合わなかった回数（`ADR-0063` の (b1)）。**本番では 0 である。**
static SPACE_ACCOUNTING_MISMATCHES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// 空間ごとの会計が合わなかった回数（`ADR-0063` の (b1)）。
pub fn space_accounting_mismatches() -> u64 {
    SPACE_ACCOUNTING_MISMATCHES.load(core::sync::atomic::Ordering::SeqCst)
}

/// いま開いている `spawn` のウィンドウの数、Ring 3 のスロットごと（`ADR-0063` の (b1)）。
///
/// # なぜスロットごとに数えるのか
///
/// **入れ子と交差を分けるためである。** **入れ子（親のウィンドウの中で子のウィンドウが開く）では、大域の差は
/// 閉じる**——**外側の差に内側の分が両辺とも入る**（`spawn` の doc）。**閉じないのは交差、
/// すなわち別のタスクのウィンドウと半分だけ重なる形である。**
///
/// **入れ子は同じスロットの中で起きる**（`spawn` は同じタスクの上で遠征が入れ子になる）。
/// **交差は別のスロットどうしで起きる**（足した 1 本はスロット 1 を使う）。
/// **実測で踏んだ**——**スロットを見ずに数えたら、既定の起動の入れ子 6 つが「交差」と出た。**
static SPAWN_WINDOWS_OPEN: [core::sync::atomic::AtomicUsize; crate::arch::x86_64::USER_TASK_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; crate::arch::x86_64::USER_TASK_SLOTS];

/// これまでに開いた `spawn` のウィンドウの数、スロットごと（`ADR-0063` の (b1)）。
static SPAWN_WINDOW_STARTS: [core::sync::atomic::AtomicU64; crate::arch::x86_64::USER_TASK_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::arch::x86_64::USER_TASK_SLOTS];

/// 大域の差を実際に主張した回数（`ADR-0063` の (b1)）。
///
/// # 数える理由
///
/// **条件つきの判定は、条件が満たされなくなれば「確かめなかった」と書きながら成功のまま死ぬ**
/// （運用者の指摘。2026-09-18）。**`FLAKY_EXCLUDED` と手で回す形の前例と同じ族である。**
/// **回数を行へ出し、参照が固定する**——**0 になれば差分で出る。**
static GLOBAL_DIFFERENCE_CHECKS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// `spawn` の会計のウィンドウ（`ADR-0063` の (b1)）。**開いたときの様子を控える。**
pub struct SpawnWindow {
    /// 開いたときのスロット。**閉じるときも同じスロットである。**
    slot: usize,
    /// 開いたとき、他のスロットで開いていたウィンドウの数。
    open_elsewhere_at_entry: usize,
    /// 開いたときの、他のスロットのウィンドウの累計。
    starts_elsewhere_at_entry: u64,
}

/// 他のスロットで開いているウィンドウの数と、その累計（`ADR-0063` の (b1)）。
fn windows_elsewhere(slot: usize) -> (usize, u64) {
    let mut open = 0usize;
    let mut starts = 0u64;
    for other in 0..crate::arch::x86_64::USER_TASK_SLOTS {
        if other == slot {
            continue;
        }
        open += SPAWN_WINDOWS_OPEN[other].load(core::sync::atomic::Ordering::SeqCst);
        starts += SPAWN_WINDOW_STARTS[other].load(core::sync::atomic::Ordering::SeqCst);
    }
    (open, starts)
}

/// 会計のウィンドウを開く（`ADR-0063` の (b1)）。
pub fn open_spawn_window() -> SpawnWindow {
    let slot = crate::arch::x86_64::current_excursion_slot();
    let (open_elsewhere_at_entry, starts_elsewhere_at_entry) = windows_elsewhere(slot);
    SPAWN_WINDOW_STARTS[slot].fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    SPAWN_WINDOWS_OPEN[slot].fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    SpawnWindow {
        slot,
        open_elsewhere_at_entry,
        starts_elsewhere_at_entry,
    }
}

/// 会計のウィンドウを閉じる（`ADR-0063` の (b1)）。**他のウィンドウと交差したかを返す。**
///
/// **交差の見方は 3 つである**——**開いたときに他のスロットのウィンドウが開いていたか、閉じるときに
/// 開いているか、自分の間に他のスロットのウィンドウが開いたか。** **どれかなら、大域の差は相手の分を
/// 取り込んでいる。** **同じスロットの入れ子は交差ではない**（`SPAWN_WINDOWS_OPEN` の doc）。
pub fn close_spawn_window(window: SpawnWindow) -> bool {
    SPAWN_WINDOWS_OPEN[window.slot].fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    let (open_elsewhere, starts_elsewhere) = windows_elsewhere(window.slot);
    let crossed = window.open_elsewhere_at_entry > 0
        || open_elsewhere > 0
        || starts_elsewhere != window.starts_elsewhere_at_entry;
    if !crossed {
        GLOBAL_DIFFERENCE_CHECKS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }
    crossed
}

/// 大域の差を主張した回数（`ADR-0063` の (b1)）。
pub fn global_difference_checks() -> u64 {
    GLOBAL_DIFFERENCE_CHECKS.load(core::sync::atomic::Ordering::SeqCst)
}

/// 破棄する空間を指したままのタスクがあれば消し、声を出す（W1-b-2）。
///
/// # 別の関数にしてある理由——**遠征スタックのフレームを広げないため**
///
/// **`load_user_program` のフレームは、`spawn` の子が走っている間ずっと深さ 0 の
/// 遠征スタックに載る。** **`dev` では、通らない分岐の `format_args!` の一時値も
/// そのフレームに場所を取る。** **ここへ出して `#[inline(never)]` にすれば、一時値は
/// この関数のフレームにだけ載り、呼んでいる間しか場所を取らない**——**呼ぶのは子が
/// 走り終えた後である。**
///
/// **引数は `&UserProcess` の 1 つだけにしてある。** **名前と PML4 を呼ぶ側で
/// 取り出して渡すと、その一時値が `load_user_program` のフレームを 16 バイト広げた**
/// （実測。`objdump` で前置きの `sub rsp` を読んだ。4,288 → 4,304 バイト）。
#[inline(never)]
fn forget_task_root_before_destroy(logger: &mut Logger<Serial>, process: &UserProcess) {
    let name = process.name;
    let root = process.space.root().as_u64();
    if crate::task::forget_page_table_root_if(root) {
        logger.error(format_args!(
            "task: a task still pointed its cr3 at {name}'s address space {root:#x} when the \
             space was about to be destroyed; the field was cleared (the excursion return should \
             have restored it)"
        ));
    }
}

/// イメージを新しいアドレス空間へマップし、`run` なら Ring 3 で走らせる（S9-b-1）。
///
/// # 走らせるかは引数で決まる
///
/// `run` が偽ならマップして戻る。**壊したイメージの扱い（S9-b-2）がこちらを使う**
/// ——マップできるところまでマップして拒まれることを見るので、走らせる必要が無い。
///
/// # 区画の権限をそのまま葉へ落とす
///
/// `PT_LOAD` の `p_flags` の W と X を、権限（`PagePermissions::user_program`）へ渡す。`hello` の
/// 2 区画はどちらも書き込み不可なので、**ここが、書けない葉の最初の実利用に
/// なる**（S9-a で足した引数が、S9-b の本命の経路で使われる）。
/// スタックだけは、書けるデータ（`PagePermissions::user_data`）としてマップする。
///
/// # 失敗しても止まらない。**呼び出し側が決める**
///
/// **かつてはここで止めていた**（相手が自分のビルドのイメージだけだった S9-b-1 まで）。
/// **S9-b-2 で `Result` にした。** 信頼できないイメージを読む経路ができたので、
/// **止めるかどうかはイメージの出所を知っている側の判断になった**——埋め込んだ 3 本の
/// 失敗はカーネルの不具合なので呼び出し側が止め、壊したイメージの失敗は期待どおりなので
/// 止めない。**`docs/roadmap.md` の S9 が「いかなる入力に対しても fail-fast
/// させない」と言っているのは、後者についてである。**
///
/// # 区画の並びは、写す前に確かめる（2026-10-01）
///
/// **`Elf::parse` の後、1 枚も写す前に `Elf::check_load_layout` を通す。** 通らない並びは
/// [`UserLoadError::Layout`] で断る——番地の順でない、重なる、計算があふれる（`p_memsz` が 0 の区画は
/// 最終ページの引き算が成り立たない）、同じページに権限の違う区画が載る、共有するページに後ろの区画が
/// ファイルの中身を持つ、のどれかである。**下のループの計算（最終ページ、ページを進める加算、区画の終わり）は、
/// この確かめが通ったことを前提にしている。**
///
/// **以前は確かめていなかった。** 同じページに権限の違う区画が載る像は、前の区画の権限のまま読み込まれ、
/// 後ろの区画の中身は写されなかった。番地の順が逆の像も読み込まれた。番地 0 で大きさ 0 の区画は、
/// 最終ページの引き算があふれてカーネルが止まった（どれも起動の途中の検査で実測した）。
///
/// # 区画が同じページを共有しているとマップできない
///
/// **後から来た区画が `Mapping { AlreadyMapped }` で拒まれる**（S9-b-3-2b）。**並びの確かめが先に断るので、
/// 像からこの経路へ来ることは無くなった。`map_user_4kib` の判定は 2 枚目の守りとして残る。**
///
/// **一度は誤っていた。** ここには以前も「`AlreadyMapped` 相当で弾かれる」と
/// 書いてあったが、**それを持っていたのは
/// [`crate::arch::x86_64::ActivePageTable::map_4kib`] の側だけで、ローダーが
/// 使う [`crate::arch::x86_64::AddressSpace::map_user_4kib`] は葉の present を
/// 見ずに書いていた。** 契約を片側だけ見て、もう片側のものとして書いていた形で
/// ある（S9-b-3-2b の数え直しで実測した）。**実測では両方「張れた」ことになり、
/// 1 つ目のフレームがマッピングから外れて 1 枚漏れた**（14 枚消えて隔離へ 13 枚）。
/// **漏れは会計に出てカーネルを止めるので、S9 の「いかなる入力でもカーネルを
/// fail-fast させない」に反していた。** 判定を足して直してある。
///
/// **`userland/user.ld` が区画をページ境界へ揃えているのは、この形を避けるため
/// である**（揃えないと実際に重なった。実測で `.text` が `0x400000..0x400030`、
/// `.rodata` が `0x400030` からになった）。**実際のツールチェインも同じ理由で
/// 揃える。**
///
fn load_user_program_into(
    logger: &mut Logger<Serial>,
    allocator: &mut crate::frame_allocator::FrameAllocator,
    image: &dyn common::image_source::ImageSource,
    process: &mut UserProcess,
    argv: &[&[u8]],
    envp: Option<&[&[u8]]>,
) -> Result<(), UserLoadError> {
    const PAGE_SIZE: u64 = 4096;

    // **像の先頭の部分を、フレームを 1 枚借りて読む**（2026-10-05）。ELF のヘッダとプログラムヘッダの表は、ここに在る
    // （`common::elf::ElfHeaders`。表が先頭の部分に収まらない像は、名前のある理由で断られる）。
    //
    // **スタックにも、静的な領域にも置かない。** スタックに 4 KiB の配列を置くと、遠征スタックの使用量が跳ねる
    // （`docs/deferred-decisions.md` の「大きなスタック配列とガード幅」）。静的な領域は、同時に載せる数だけ要る。
    // **フレームは、載せ終えたら（失敗の経路でも）アロケータへ返す**——このプロセスの空間には繋がない。
    let Some(head_frame) = allocator.allocate_frame() else {
        return Err(UserLoadError::OutOfFrames);
    };
    let head_len = image.len().min(PAGE_SIZE) as usize;
    let head_at = common::addr::direct_map().phys_to_virt(head_frame).as_u64() as *mut u8;
    // SAFETY: いま取ったフレームで、direct map が覆っている。ほかに指す者は居ない（空間へ繋がず、下で返すまで
    // この関数だけが持つ）。長さは 1 ページちょうどである。`u8` はどのビット列も妥当である。
    let head = unsafe { core::slice::from_raw_parts_mut(head_at, PAGE_SIZE as usize) };
    let outcome = match image.read_at(0, &mut head[..head_len]) {
        Ok(()) => load_segments_and_stack(
            logger,
            allocator,
            image,
            &head[..head_len],
            process,
            argv,
            envp,
        ),
        Err(e) => Err(UserLoadError::Read(e)),
    };
    let _ = allocator.deallocate_frame(head_frame);
    outcome
}

/// 位置独立の像を、どこへ置いたかを 1 行出す。
///
/// **`#[inline(never)]` にしてある**——書式の一時値を、載せる関数のフレームへ持ち込まない（位置を決めてリンクした像では、
/// この行は出ないのに、フレームだけが太る。遠征スタックの使用量は、容量の半分の手前に在る）。
#[inline(never)]
fn note_position_independent_placement(
    logger: &mut Logger<Serial>,
    name: &str,
    plan: &common::elf::LoadPlan,
) {
    logger.info(format_args!(
        "user-load: {name} is position-independent; placed at {:#x} (image {:#x}..{:#x}, entry {:#x})",
        plan.bias, plan.base, plan.end, plan.entry
    ));
}

/// スタックの下の見張りのページを 1 行出す（[`note_position_independent_placement`] と同じ理由で、関数を分けてある）。
#[inline(never)]
fn note_stack_guard_page(logger: &mut Logger<Serial>, guard: u64) {
    logger.info(format_args!(
        "user-load: the page below the stack ({guard:#x}) is left unmapped as a guard; neither the \
         heap nor mmap is placed on it"
    ));
}

/// [`load_user_program_into`] の中身——区画を写し、スタックを張り、葉を読み戻す。**`head` は像の先頭の部分である。**
fn load_segments_and_stack(
    logger: &mut Logger<Serial>,
    allocator: &mut crate::frame_allocator::FrameAllocator,
    image: &dyn common::image_source::ImageSource,
    head: &[u8],
    process: &mut UserProcess,
    argv: &[&[u8]],
    envp: Option<&[&[u8]]>,
) -> Result<(), UserLoadError> {
    use crate::arch::x86_64::walk_page_table;
    use crate::paging::permissions::PagePermissions;
    use common::elf::ElfHeaders;

    const PAGE_SIZE: u64 = 4096;
    // 並びの確かめが前提にするページの大きさと、ここで写すページの大きさは同じでなければならない。
    const _: () = assert!(PAGE_SIZE == common::elf::LOAD_PAGE_SIZE);

    let direct_map = common::addr::direct_map();

    // **先頭の部分と、ファイルの長さの数から検査する**（`common::elf::ElfHeaders`。全体を読む入口 `Elf::parse` と、
    // 同じ像を同じ理由で断ることを、ホストの試験が確かめている）。
    let elf = match ElfHeaders::parse(head, image.len()) {
        Ok(elf) => elf,
        Err(e) => return Err(UserLoadError::Parse(e)),
    };
    // **写す前に区画の並びを確かめる**（この関数の doc の「区画の並びは、写す前に確かめる」）。
    //
    // 破壊テスト (2026-10-01, user-load-skip-layout-check): 確かめの答えを捨てて写す。**起動の途中の
    // 壊した像の検査が、並びの確かめで断られるはずの像を別の場所で断られるのを見て止まる。**
    match elf.check_load_layout() {
        Err(e) if !cfg!(feature = "user-load-skip-layout-check") => {
            return Err(UserLoadError::Layout(e));
        }
        _ => {}
    }

    // マップした VA と、期待する W を覚えておく（後で読み戻して照合する）。
    let mut mapped: [(u64, bool); 8] = [(0, false); 8];
    let mut mapped_count = 0usize;
    // 実際にマップし終えた区画の本数（ADR-0039 の判定行）。
    let mut loaded_segments = 0usize;
    // 共有として飛ばしたページの数（ADR-0039 の判定行）。
    let mut shared_pages = 0usize;
    // **直前の区画の最終ページと終端アドレス。** 共有を許す条件に両方が要る
    // （ADR-0039）。**最初の区画には直前が無いので、共有は起こりえない。**
    let mut previous_last_page: Option<u64> = None;
    let mut previous_end: u64 = 0;

    // **ELF が持つ区画の本数を先に数える（ADR-0039 の切り分け）。**
    //
    // **「張った本数」だけでは足りない。** 3 行出たとき、**それが正しい 3 本
    // なのか、4 本のうち 1 本を落とした 3 本なのかが行から読めない**
    // ——実際にその区別が付かず、切り分けが遠回りになった。
    // **本数の対（持っている / マップした）を同じ行に出す。**
    let declared_segments = elf.load_segments().count();

    // **載せる位置を決める**（2026-10-05。`common::elf::ElfHeaders::plan`）。**配置は像の種類で選ぶ**
    // （[`ProcessLayout`]）。位置を決めてリンクした像は、今までどおり、ずらさない。位置独立の像は、決まった量だけ
    // ずらす。**動的リンクを求める像、実行できるスタックを求める像、置いてよい範囲に収まらない像は、ここで断る**
    // ——ページは、まだ 1 枚も写していない。
    let layout = ProcessLayout::for_kind(elf.kind());
    let plan = match elf.plan(&layout.load_policy(elf.kind())) {
        Ok(plan) => plan,
        Err(e) => return Err(UserLoadError::Placement(e)),
    };
    process.kind = elf.kind();
    process.image_base = plan.base;
    if plan.bias != 0 {
        note_position_independent_placement(logger, process.name, &plan);
    }

    for ph in elf.load_segments() {
        // **区画の番地を、ずらした後の値にする。** 下の計算は、どれもこの値を使う。ずらしてもあふれないことは、
        // 計画が確かめている。ファイルの中の位置（`p_offset`）は、ずらさない。
        //
        // 破壊テスト (2026-10-06, pie-load-without-bias-test): 区画を、ずらさずに載せる（入口だけが、ずれた番地を
        // 指す）。位置独立の像は、リンクした番地（0 から）に載ることになり、載せる途中か、入口で落ちる。
        #[cfg(feature = "pie-load-without-bias-test")]
        let plan = common::elf::LoadPlan { bias: 0, ..plan };
        let ph = common::elf::ProgramHeader {
            p_vaddr: ph.p_vaddr + plan.bias,
            ..ph
        };
        let writable = ph.p_flags & common::elf::PF_W != 0;
        // **実行できるかも、区画のフラグから取る**（2026-10-02）。実行しない区画の葉には、実行禁止のビットが付く
        // （2026-10-03）。**書けて実行もできる区画は、ここへ来る前に、並びの確かめが断っている。**
        // 破壊テスト (2026-10-03, user-load-ignores-execute): 書けない区画を、フラグに依らず実行できる形で写す。
        let executable = ph.p_flags & common::elf::PF_X != 0
            || (cfg!(feature = "user-load-ignores-execute-test") && !writable);
        let first_page = ph.p_vaddr & !(PAGE_SIZE - 1);
        // 破壊テスト (ADR-0039, user-load-filesz-only): `memsz` ではなく `filesz` で
        // 最終ページを出す。**`.bss` がマップされない**——`/bin/bss-test` が
        // ゼロを読もうとして落ちる（この変更が入る前の欠落そのものである）。
        #[cfg(feature = "user-load-filesz-only")]
        let last_page = (ph.p_vaddr + ph.p_filesz.max(1) - 1) & !(PAGE_SIZE - 1);
        #[cfg(not(feature = "user-load-filesz-only"))]
        let last_page = (ph.p_vaddr + ph.p_memsz - 1) & !(PAGE_SIZE - 1);

        let mut page = first_page;
        while page <= last_page {
            // **正当な共有ページは飛ばす（ADR-0039）。**
            //
            // **条件は 2 つで、どちらも ELF の仕様から導ける。**
            //
            // 1. **そのページが直前の区画の最終ページと一致すること**
            // 2. **この区画の先頭が、直前の区画の終端以降であること**
            //    （`p_vaddr >= previous_end`）——**区画どうしがアドレスの上で
            //    重ならないこと**である。`lld` は `.bss` を `.data` の直後
            //    （同じページの途中）から始めるので、**境界のページだけを
            //    共有する形は正当である。**
            //
            // **2 つ目が要る。** 1 つ目だけだと、`user-load-corrupt` の
            // 「前の区画のページへ `p_vaddr` を動かす」破壊テストが通ってしまう
            // （**実測でそうなった**）——`hello` の 1 本目は 0x42 バイトしか
            // 無いので**最終ページが先頭ページと同じ**で、破壊テストが狙う
            // `0x400030` も同じページに落ちる。**違うのは、あちらが直前の
            // 区画の中身の内側（終端 0x400042 より前）を指すことである。**
            //
            // **確保もゼロ埋めもやり直さない。** そのページは直前の区画が
            // 既にゼロ埋めしてファイルの中身を重ねてあり、**新しい区画は
            // その中身の直後から始まる**ので、**残りは既にゼロである。**
            // **やり直すと直前の区画の中身を消す。**
            //
            // **このページの権限は直前の区画のもので、新しい区画の中身もここへは写さない。**
            // **それでよいことは、並びの確かめが保証している**——共有するページでは 2 つの区画の
            // 権限が同じで、後ろの区画はファイルの中身を持たない（`common::elf::check_load_layout`）。
            if previous_last_page == Some(page) && page == first_page && ph.p_vaddr >= previous_end
            {
                shared_pages += 1;
                page += PAGE_SIZE;
                continue;
            }

            let Some(frame) = allocator.allocate_frame() else {
                return Err(UserLoadError::OutOfFrames);
            };

            // **ゼロ埋めしてからファイルの中身を重ねる。** `p_memsz` が `p_filesz` より
            // 大きい分（.bss）はゼロのまま残る。
            let dst = direct_map.phys_to_virt(frame).as_u64() as *mut u8;
            // SAFETY: いま取ったフレームで、direct map が覆っている。誰も使っていない。
            unsafe { core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize) };

            // このページが覆うファイル内の範囲を切り出して書く。
            let page_start_in_segment = page.saturating_sub(ph.p_vaddr);
            let offset_in_page = ph.p_vaddr.saturating_sub(page);
            // 破壊テスト (S9-b-1, user-run-skip-load): ファイルの中身をコピーしない。ページは
            // ゼロのままになり、Ring 3 が entry からゼロを実行して ud2 へ届かない。
            if page_start_in_segment < ph.p_filesz && !cfg!(feature = "user-run-skip-load") {
                let remaining = ph.p_filesz - page_start_in_segment;
                let room = PAGE_SIZE - offset_in_page;
                let count = core::cmp::min(remaining, room) as usize;
                // SAFETY: `dst` は、いま取ってゼロ埋めしたフレームの先頭（direct map 越し）で、`PAGE_SIZE` バイト
                // 書ける。`offset_in_page + count <= PAGE_SIZE` は上で押さえてある。このフレームを指す者は、まだ
                // 居ない（空間へ繋ぐのは、この後である）。
                let target = unsafe {
                    core::slice::from_raw_parts_mut(dst.add(offset_in_page as usize), count)
                };
                // **像から、このページの分だけを読む**（2026-10-05）。ファイルの中の位置は、区画の始まりに、区画の中の
                // 位置を足したものである。`p_offset + p_filesz` がファイルに収まることは、ヘッダの検査が確かめて
                // あるので、足し算はあふれない。
                if let Err(e) = image.read_at(ph.p_offset + page_start_in_segment, target) {
                    // **繋いでいないフレームは、ここで返す**（下の、写せなかったときと同じ理由）。
                    let _ = allocator.deallocate_frame(frame);
                    return Err(UserLoadError::Read(e));
                }
            }

            let Some(virt) = common::addr::VirtAddr::new(page) else {
                return Err(UserLoadError::NotCanonical(page));
            };
            // 破壊テスト (S9-b-1, user-run-writable-text): 区画の権限を無視して書けるように
            // マップする。**読み取り専用のはずの葉が W=1 になり、下の読み戻しが検出する。**
            // **書けるようにするのは、実行しない区画だけである**（2026-10-03）——実行する区画を書けるようにすると、
            // 書けて実行もできる権限になり、ページを足す入口が先に断る（読み戻しまで届かない）。
            let attributes = PagePermissions::user_program(
                writable || (cfg!(feature = "user-run-writable-text") && !executable),
                executable,
            );
            // SAFETY: この空間はまだ稼働していない。direct map は覆っている。
            if let Err(e) = unsafe {
                process
                    .space
                    .map_user_4kib(allocator, direct_map, virt, frame, attributes)
            } {
                // **マップできなかったフレームは、ここで返す。** 空間へ繋がっていないので
                // `AddressSpace::detach` からは見えず、返さないと誰にも戻らない。
                // **実測で気づいた**——失敗の経路で空きフレームが 7 枚減るのに、
                // 隔離へ入ったのは 6 枚だった。差の 1 枚がこれである。
                let _ = allocator.deallocate_frame(frame);
                return Err(UserLoadError::Mapping {
                    virt: page,
                    error: e,
                });
            }

            if mapped_count < mapped.len() {
                mapped[mapped_count] = (page, writable);
                mapped_count += 1;
            }
            page += PAGE_SIZE;
        }

        // **ページ範囲も出す（ADR-0039）。** 区画どうしがページを共有する形は
        // ELF では正当なので、**重なりが行から読める**ようにしておく。
        logger.info(format_args!(
            "user-load: {} mapped PT_LOAD {:#x}..{:#x} (filesz={:#x} memsz={:#x} w={writable}) \
             pages {:#x}..{:#x}",
            process.name,
            ph.p_vaddr,
            ph.p_vaddr + ph.p_memsz,
            ph.p_filesz,
            ph.p_memsz,
            first_page,
            last_page
        ));
        loaded_segments += 1;
        previous_last_page = Some(last_page);
        previous_end = ph.p_vaddr + ph.p_memsz;
    }

    // **ヒープの初期値を控える（H-a。ADR-0044）。** **イメージの末尾の次のページである。**
    //
    // **`previous_end` は最後の区画の末尾である**（上のループが毎回入れている）。
    // **区画はアドレスの順に並んでいる**ので、これがイメージの末尾になる
    // （並びは、上で通した `Elf::check_load_layout` が確かめている。`Elf::load_segments` は
    // `PT_LOAD` を選り分けるだけである）。
    process.heap = Heap::from_image_end(previous_end, &layout);

    // **本数の対。** 落ちた区画があれば、この 1 行で分かる。
    logger.info(format_args!(
        "user-load: {} PT_LOAD segments: declared={declared_segments} mapped={loaded_segments} \
         (they must match; a gap means a segment was skipped), pages shared with the previous \
         segment={shared_pages}",
        process.name
    ));

    // ユーザースタックを写す。**こちらは書ける。** **枚数は配置が決める**（2026-10-05。位置を決めてリンクした像は
    // 1 枚、位置独立の像は 256 KiB）。**起動のときに全部を写す**——後から伸ばさない。
    //
    // **初期データ（argc・argv・envp）を積むのは、いちばん上のページである。** `dst` と `stack_page` は、そのページを指す。
    let stack_bottom = layout.stack_bottom();
    let stack_top = layout.stack_top;
    let stack_page = stack_top - PAGE_SIZE;
    let stack_attributes = PagePermissions::user_data();
    let mut dst = core::ptr::null_mut::<u8>();
    let mut page = stack_bottom;
    while page < stack_top {
        let Some(frame) = allocator.allocate_frame() else {
            return Err(UserLoadError::OutOfFrames);
        };
        let at = direct_map.phys_to_virt(frame).as_u64() as *mut u8;
        // **いちばん上のページは 0 で埋め（初期データを積んだ後に、その下を既知のバイトで埋める）、下のページは
        // 初めから既知のバイトで埋める**（2026-10-06。使用量を、スタックの全部で測るため）。
        let fill = if page == stack_page {
            0
        } else {
            USER_STACK_FILL
        };
        // SAFETY: いま取ったフレーム。direct map が覆っている。
        unsafe { core::ptr::write_bytes(at, fill, PAGE_SIZE as usize) };
        let Some(virt) = common::addr::VirtAddr::new(page) else {
            let _ = allocator.deallocate_frame(frame);
            return Err(UserLoadError::NotCanonical(page));
        };
        // SAFETY: この空間はまだ稼働していない。direct map は覆っている。
        if let Err(e) = unsafe {
            process
                .space
                .map_user_4kib(allocator, direct_map, virt, frame, stack_attributes)
        } {
            // **繋いでいないフレームは、ここで返す**（区画を写せなかったときと同じ理由）。
            let _ = allocator.deallocate_frame(frame);
            return Err(UserLoadError::Mapping {
                virt: page,
                error: e,
            });
        }
        if page == stack_page {
            dst = at;
            // **読み戻して確かめるのは、いちばん上のページである**（今までと同じ 1 枚）。
            if mapped_count < mapped.len() {
                mapped[mapped_count] = (page, true);
                mapped_count += 1;
            }
        }
        page += PAGE_SIZE;
    }
    logger.info(format_args!(
        "user-load: mapped the user stack {stack_bottom:#x}..{stack_top:#x} (w=true)"
    ));
    // **見張りのページは、写さないことで置く。** スタックが尽きると、ここを踏んでページフォルトになる。
    if let Some(guard) = layout.stack_guard_page() {
        note_stack_guard_page(logger, guard);
    }

    // **環境を積む前に、ロックの外へコピーする（f-1）。**
    //
    // **`build_initial_stack` をロックの下で呼ばない**——**`Locked` は持っている
    // 間ずっと割り込みを止める**ので、1 ページを書く間ずっと止めることになる。
    // **コピーは 1KiB で、カーネルスタックの余裕（実測で 65,328 バイト）の
    // 中に収まる。**
    let mut env_store = [[0u8; ENV_LINE_MAX]; MAX_ENVP];
    let mut env_lens = [0usize; MAX_ENVP];
    let mut env_slices: [&[u8]; MAX_ENVP] = [b""; MAX_ENVP];
    let envp: &[&[u8]] = match envp {
        // **親が渡したものをそのまま積む（f-2）。** コピーはもう取ってある
        // （`spawn` が `SPAWN_ENVPS` へ控えている）ので、ここではコピーしない。
        Some(from_parent) => from_parent,
        None => {
            let env_count = {
                let environment = ENVIRONMENT.lock();
                for index in 0..environment.table.count() {
                    let line = environment.table.line(index);
                    env_store[index][..line.len()].copy_from_slice(line);
                    env_lens[index] = line.len();
                }
                environment.table.count()
            };
            for index in 0..env_count {
                env_slices[index] = &env_store[index][..env_lens[index]];
            }
            &env_slices[..env_count]
        }
    };

    // **初期スタックを Linux の形で積む（S11-1）。**
    //
    // **補助ベクタ（`auxv`）の値は、載せた結果から作る**（2026-10-06）。入口とプログラムヘッダの表の番地は、
    // ずらした後の値である。`AT_RANDOM` の 16 バイトは、暗号に使える値ではない（CPU の置き場の `random` の doc）。
    //
    // 破壊テスト (2026-10-06, auxv-entry-not-biased-test): `AT_ENTRY` に、ずらす前の番地を渡す。位置を決めて
    // リンクした像では値が変わらないので、気づくのは位置独立の像だけである（`pie-hello` が、終了状態 1 で言う）。
    let aux_entry = if cfg!(feature = "auxv-entry-not-biased-test") {
        plan.entry - plan.bias
    } else {
        plan.entry
    };
    // 破壊テスト (2026-10-06, auxv-phdr-not-biased-test): `AT_PHDR` に、ずらす前の番地を渡す（`pie-hello` が、終了状態 2 で
    // 言う。自分の ELF ヘッダから求めた番地と比べるので、ずれた番地を読みには行かない）。
    let aux_phdr = if cfg!(feature = "auxv-phdr-not-biased-test") {
        plan.program_headers_at.map(|at| at - plan.bias)
    } else {
        plan.program_headers_at
    };
    let aux = crate::abi::linux::Auxv {
        program_headers_at: aux_phdr,
        program_header_count: plan.program_header_count,
        entry: aux_entry,
        random: crate::arch::x86_64::weak_random_bytes().0,
        exec_name: process.name.as_bytes(),
    };
    // SAFETY: `dst` はいまマップしたスタックページの direct map 越しの先頭で、
    // 1 ページぶん書ける。単一実行文脈である。フレームは上で確保したばかりで、ほかに指しているのは
    // まだ稼働していない空間のマップだけである。`argv` と `envp` と `aux` の文字列は、このフレームを確保する前に
    // 作ったカーネルの側の控えで、このページを指さない。
    let Some(initial_sp) = (unsafe { build_initial_stack(dst, stack_page, argv, envp, &aux) })
    else {
        return Err(UserLoadError::ArgumentsTooLong);
    };
    process.stack_top = initial_sp;

    // **いちばん上のページの未使用部分を既知のバイトで埋める（EV）。** **初期データの下は、
    // これからプログラムが使う領域である。** 遠征スタックと同じ形で、
    // **戻ってから高水位を読む**（[`USER_STACK_FILL`]。下のページは、写すときに埋めてある）。
    //
    // **順序に理由がある。** **積んだ後に埋める**——先に埋めると、
    // 積んだ文字列と表を毒値が上書きする。
    let initial_bytes = (stack_top - initial_sp) as usize;
    // SAFETY: `dst` はスタックページの先頭で、`PAGE_SIZE` バイト書ける。
    // 埋めるのは初期データより下だけである。
    unsafe { core::ptr::write_bytes(dst, USER_STACK_FILL, PAGE_SIZE as usize - initial_bytes) };
    process.stack_scratch = dst as u64;

    logger.info(format_args!(
        "user-load: {} initial stack at {initial_sp:#x} (argc={}, envc={}, 16-byte aligned={},          initial data {initial_bytes} of {PAGE_SIZE} byte(s))",
        process.name,
        argv.len(),
        envp.len(),
        initial_sp % 16 == 0
    ));

    // **マップした側とは独立に降りて、葉のフラグを読み戻す。**
    // これが S9-a で足した `writable` が実際に W を落としていることの、
    // この経路での観測である（`ring3-vectors` の 6 本目はもう一方の経路を見ている）。
    let mut mismatches = 0usize;
    for &(virt_value, expected_writable) in mapped.iter().take(mapped_count) {
        let Some(virt) = common::addr::VirtAddr::new(virt_value) else {
            continue;
        };
        // SAFETY: この空間の PML4 は有効で、direct map が配下を覆っている。読み取りのみ。
        match unsafe { walk_page_table(process.space.root(), direct_map, virt) } {
            Ok(resolved) => {
                let writable = resolved.leaf_writable();
                let user = resolved.leaf_user_accessible();
                if writable != expected_writable || !user {
                    mismatches += 1;
                    logger.error(format_args!(
                        "user-load: {virt_value:#x} has w={writable} (expected \
                         {expected_writable}) u={user} (expected true)"
                    ));
                }
            }
            Err(e) => {
                mismatches += 1;
                logger.error(format_args!(
                    "user-load: {virt_value:#x} did not walk: {e:?}"
                ));
            }
        }
    }

    if mismatches != 0 {
        return Err(UserLoadError::LeafFlags { count: mismatches });
    }

    // === Ring 3 で走らせる（S9-b-1 の 4 つ目） ===
    //
    // **CR3 を差し替えてから iretq で落ちる。** 上位は共有なのでカーネルは動き
    // 続ける（S7-c の到達条件 4 が、実プログラムで初めて使われる）。
    // 戻りは `ud2` の #UD を S8 の例外による終了処理が受ける。
    // 破壊テスト (S9-b-1, user-run-wrong-entry): entry ではなく最初の PT_LOAD の先頭へ
    // 飛ぶ。**詰め物の ud2 で即座に #UD になり、フォルト RIP が期待と食い違う。**
    // 詰め物が生きていることは verify_embedded_user_elf が主張している。
    //
    // **入口は、ずらした後の番地である**（計画が持つ）。
    #[cfg(not(feature = "user-run-wrong-entry"))]
    let entry = plan.entry;
    #[cfg(feature = "user-run-wrong-entry")]
    let entry = elf
        .load_segments()
        .next()
        .map(|ph| ph.p_vaddr + plan.bias)
        .unwrap_or(plan.entry);

    // **ここで entry が確定する。** 呼び出し側は `UserProcess` から読む。
    process.entry = entry;

    Ok(())
}

/// ユーザースタックの高水位を出す（EV。ADR-0041 の決定 4）。
///
/// **スタックの全部のページを、下から読む**（2026-10-06。それまでは 1 ページだった）。各ページは、載せるときに既知の
/// バイトで埋めてある（いちばん上のページは、初期データの下だけ）。**既知のバイトでない最初の位置から上端までが、
/// 使った量である。**
///
/// **ページの物理の在りかは、この空間のページテーブルを降りて求める**（載せた側の控えを持たない。`stack_scratch` は、
/// 載せたかどうかの印としてだけ使う）。**プログラムはもう走っていない**ので、読む間に書き手は居ない。
///
/// # 限界
///
/// **プログラムが既知のバイトそのものを書いたら、使ったとは数えられない。** 下から数えるので、間に既知のバイトが
/// 挟まっても影響しない。**壊れる側（見張りのページを踏む）は、そもそもその下が写っていないので `#PF` になる。**
fn report_user_stack_high_water(logger: &mut Logger<Serial>, process: &UserProcess) {
    const PAGE_SIZE: u64 = 4096;
    let direct_map = common::addr::direct_map();

    if process.stack_scratch == 0 {
        // **マップしていない。** 走らせずに戻る経路（`run` が偽）がここへ来る。
        return;
    }
    let layout = ProcessLayout::for_kind(process.kind);
    let (bottom, top) = (layout.stack_bottom(), layout.stack_top);
    let stack_bytes = layout.stack_bytes;

    // **下のページから順に、既知のバイトでない最初の位置を探す。**
    let mut lowest_used = None;
    let mut unresolved = 0usize;
    let mut page = bottom;
    'pages: while page < top {
        let Some(virt) = common::addr::VirtAddr::new(page) else {
            unresolved += 1;
            page += PAGE_SIZE;
            continue;
        };
        // SAFETY: この空間の PML4 は有効で、direct map が配下を覆っている。読み取りのみ。
        let Ok(resolved) = (unsafe {
            crate::arch::x86_64::walk_page_table(process.space.root(), direct_map, virt)
        }) else {
            unresolved += 1;
            page += PAGE_SIZE;
            continue;
        };
        let bytes = direct_map.phys_to_virt(resolved.phys).as_u64() as *const u8;
        for offset in 0..PAGE_SIZE {
            // SAFETY: `bytes` は、この空間に写してあるスタックのページを direct map 越しに指しており、
            // `PAGE_SIZE` バイト読める。書き手はもう走っていない。
            if unsafe { bytes.add(offset as usize).read() } != USER_STACK_FILL {
                lowest_used = Some(page + offset);
                break 'pages;
            }
        }
        page += PAGE_SIZE;
    }
    let used = lowest_used.map_or(0, |at| top - at);
    let over_half = used * 2 > stack_bytes;

    logger.info(format_args!(
        "user-stack: {} used {used} of {stack_bytes} byte(s) ({}%), over half={over_half}, pages that did not \
         resolve={unresolved} (the initial argv/envp/auxv table is counted in; ADR-0041 says to decide about \
         growing the stack when this goes over half)",
        process.name,
        used * 100 / stack_bytes
    ));
}

/// 写像の表を据え、載せた像・スタック・見張りのページ・ヒープを登録する（2026-10-06）。**`#[inline(never)]`** で、
/// 一時値を [`run_loaded_program`] のフレームへ乗せない。登録に失敗したら、名指しして止まる（載せたものどうしが
/// 重なることは無いはずで、重なれば載せる側の誤りである）。
#[inline(never)]
fn register_loaded_mappings(logger: &mut Logger<Serial>, process: &UserProcess) {
    use crate::mappings::MappingKind;

    let layout = ProcessLayout::for_kind(process.kind);
    crate::mappings::activate_for_next_process(layout.mmap_base, layout.mmap_limit);
    let heap_start = process.heap.start();
    let outcome = crate::mappings::with_loaded(|map| {
        map.register(
            process.image_base,
            heap_start,
            MappingKind::Image,
            true,
            true,
        )?;
        map.register(
            layout.stack_bottom(),
            layout.stack_top,
            MappingKind::Stack,
            true,
            true,
        )?;
        if let Some(guard) = layout.stack_guard_page() {
            map.register(guard, guard + 4096, MappingKind::Guard, false, false)?;
        }
        map.set_heap_end(heap_start, process.heap.break_at())
    });
    if let Err(error) = outcome {
        logger.error(format_args!(
            "mappings: {} could not register what the loader placed ({error:?}; image \
             {:#x}..{heap_start:#x}, stack {:#x}..{:#x}); the table would not know where the \
             program lives. halting",
            process.name,
            process.image_base,
            layout.stack_bottom(),
            layout.stack_top
        ));
        common::arch::x86_64::halt_forever();
    }
}

/// `tkill` で自分へ送ったシグナルで、プロセスを終わらせたことを 1 行出す（2026-10-06）。**`#[inline(never)]`** で、
/// `format_args!` の一時値を [`run_loaded_program`] のフレームへ乗せない。
#[inline(never)]
fn report_signal_that_ended_the_process(logger: &mut Logger<Serial>, name: &str, signal: u64) {
    logger.warn(format_args!(
        "tkill: {name} sent itself signal {signal} and the kernel does not deliver signals, so the \
         process was ended with status {} (128 + the signal)",
        128 + signal
    ));
}

/// `futex` で待つ場面に入ったプロセスを終わらせたことを 1 行出す（2026-10-06）。**`#[inline(never)]`** で、
/// `format_args!` の一時値を [`run_loaded_program`] のフレームへ乗せない。
#[inline(never)]
fn report_futex_deadlock(logger: &mut Logger<Serial>, name: &str, address: u64) {
    logger.warn(format_args!(
        "futex: {name} waited on {address:#x} (FUTEX_WAIT with the expected value) and there is no \
         other thread to wake it; the process was ended with status {} instead of sleeping forever",
        crate::syscall::FUTEX_DEADLOCK_STATUS
    ));
}

/// カーネルスタックの高水位を 1 行出す（`ADR-0068` の (c)）。
///
/// **`#[inline(never)]` にしてある**——**`format_args!` の一時値を [`run_loaded_program`] のフレームへ
/// 乗せないためである。** **`dev` では、通らない分岐の一時値も呼び出しの引数の一時値も、そのままフレームを
/// 広げる**（`docs/coding-standards.md` の「番地以外が動いたら、まず枠の大きさを静的に読む」）。
/// **乗せた形を 1 度実測した**——**計測自身がカーネルスタックの高水位と遠征スタックの高水位を
/// 208 バイトずつ深くした**（2026-09-23。`tools/frame-sizes.py` でフレームを読んだ）。
#[inline(never)]
fn report_stack_water_before_ring3(logger: &mut Logger<Serial>, name: &'static str) {
    let used = crate::arch::x86_64::kernel_stack_high_water();
    let capacity = crate::arch::x86_64::kernel_stack_capacity();
    logger.info(format_args!(
        "stack-water: before entering {name} in Ring 3, the kernel stack used {used} of \
         {capacity} byte(s); {} left",
        capacity.saturating_sub(used)
    ));
}

/// マップ済みのプロセスを Ring 3 で走らせる（S11-3 で切り出した）。
///
/// # なぜ切り出したか
///
/// **アロケータを遠征の前に返すためである**（`ADR-0030`）。マッピングには要るが、
/// 遠征には要らない。**切り口は元からあった `if !run` の位置である。**
///
/// **あの分岐は S9-b-2 で「壊した像を写像だけして走らせない」ために作った。**
/// **「写像と実行を分ける」という同じ軸なので、所有の境界とも一致した**
/// ——別々の目的で引いた線が、同じ場所を通っている。
///
/// # Safety
///
/// `process` のマッピングが済んでおり、entry と stack がマップしたユーザーページであること。
/// 起動時の単一実行文脈から呼ぶこと。
unsafe fn run_loaded_program(
    logger: &mut Logger<Serial>,
    process: &mut UserProcess,
) -> Result<(), UserLoadError> {
    let production = crate::arch::x86_64::active_page_table_root();

    crate::syscall::reset_counters();
    // **戻す RSP0 は「今この処理が乗っているカーネルスタックの上端」である。**
    //
    // 深さ 0 なら、ここはカーネルの直線上なのでメインのスタックである。
    // **深さが 1 以上なら、`spawn` が親の遠征の中から呼んでいる**——親は
    // その深さの遠征スタックの上でこの処理をしているので、**そこへ戻さないと
    // 親のカーネルスタックが変わってしまう**（S11-2 の入れ子の検証で同じ判断をした）。
    //
    // **深さから引ける値なので、引数で受け取らない。**
    //
    // 破壊テスト (S11-5, spawn-child-rsp0): 親の遠征スタックではなく、**子自身の**
    // 遠征スタックの上端へ戻す。**入れ子でないうちはこの行を通らないので、
    // 入れ子になった瞬間だけ壊れる。** 親が次にカーネルへ入るときの RSP0 が
    // 子のスタックを指し、**次に子を起動したときに親のフレームを踏む。**
    // **`spawn` が戻り先の RSP0 を突き合わせて検出する。**
    let main_entry_stack_top = if crate::arch::x86_64::excursion_depth() == 0 {
        crate::arch::x86_64::active_kernel_entry_stack_top()
    } else if cfg!(feature = "spawn-child-rsp0") {
        crate::arch::x86_64::excursion_stack_range_at(crate::arch::x86_64::excursion_depth()).1
    } else {
        crate::arch::x86_64::excursion_stack_range().1
    };

    // **載せて、タスクの CR3 の欄を据える（W1-b-2。W1-c-2 で割り込みを止めて一続きにした）。**
    // SAFETY: この空間はカーネルの上位を共有しており、切り替えても実行中の
    // コードとスタックは見え続ける。
    unsafe {
        crate::task::switch_page_table_root_and_note(
            process.space.root(),
            process.space.root().as_u64(),
        )
    };
    // **このプロセスの fd の表を据える（S10-b）。** `dispatch` はプロセスを
    // 知らないので、遠征の間だけ `crate::vfs` が持つ
    // （`syscall::set_user_window` と同じ形。据えるのは Ring 3 へ落ちる側である）。
    let previous_files = crate::vfs::swap_current_files(core::mem::take(&mut process.files));
    // **ヒープも据える（H-a）。** **据える側が戻す**（`files` と同じ形）。
    let previous_heap = swap_current_heap(process.heap);
    // **プロセスごとの小さな状態（シグナルの登録など）を初めの形に戻し、名前を控える**（2026-10-06）。スロットと
    // 深さで引くので、据え替えの写しは要らない（`crate::process_state`）。
    crate::process_state::reset_for_next_process(process.name.as_bytes());
    // **写像の表も、配置の値で据え、載せたものを登録する**（2026-10-06。`crate::mappings`）。像・スタック・見張りの
    // ページ・ヒープが表に在ることで、`MAP_FIXED` がそこへ置かず、`brk` が無名の写像へ伸びない。
    register_loaded_mappings(logger, process);
    // **前景を取る（S11-10）。** 取っているあいだ、カーネル側の消費者
    // （`interrupts::drain_keyboard`）はスキャンコードを取り出さない。
    // **入力の消費者は同時に 1 つである**（`crate::input` の不変条件）。
    //
    // **入れ子でも取れる。** 親は遠征の中で `spawn` を呼んでおり、
    // **その間ずっと前景を持っている。** 子が取ろうとすると偽が返るので、
    // **親が持ったままにして、子はその前景を通して読む**——
    // **持ち主は 1 人という不変条件は保たれる。**
    let claimed_foreground = crate::input::claim_foreground();
    // **前の中断要求を持ち越さない（S12 前の手当て、C）。**
    //
    // **深さ 1 で Ctrl+C を押すと、フラグは立つが誰も消費しない**——
    // **終了させる地点は深さ 2 以上でしか発火しない**（`crate::interrupts` の
    // `should_fold_excursion`）。**降ろさずに子を起動すると、その子が
    // 起動した瞬間に止まる。**
    //
    // 破壊テスト (S12 前の手当て C, kill-keep-stale-interrupt): 降ろさない。
    // **シェルで Ctrl+C を押した後、次に起動した子が即座に止まる。**
    #[cfg(not(feature = "kill-keep-stale-interrupt-test"))]
    crate::input::clear_interrupt_request();
    // **どの深さの遠征スタックを使うかを控える（S11-5）。** 戻った後は深さが
    // 元へ戻っているので、そのときには引けない。
    let entered_at_depth = crate::arch::x86_64::excursion_depth();
    // **起動するプログラムの FP は既定値から始める（`ADR-0058` の Decision 4）。**
    // **前のプログラムが XMM へ残した値が、次のプログラムから読めてはならない。**
    //
    // 破壊テストでの確認: `fp-no-fresh-state` では戻さない。**前の値がそのまま見える。**
    //
    // **FS と GS の基底も、0 から始める**（2026-10-05。`arch` の `UserRegisters`）。**新しいプログラムは、前の
    // プログラム（子なら、親）のスレッドローカルの領域を指さずに始まる。**
    //
    // 破壊テストでの確認: `fs-base-spawn-no-fresh` では、FP だけを戻して基底を戻さない。**子が、親の基底のまま
    // 始まる。**
    #[cfg(all(
        not(feature = "fp-no-fresh-state"),
        not(feature = "fs-base-spawn-no-fresh")
    ))]
    // SAFETY: [`crate::arch::x86_64::UserRegisters::fresh`] の FP は `fxsave` の形に沿った並びで、
    // MXCSR も予約ビットを立てていない（`#GP` にならない）。基底は 0 で、正準な番地である。
    unsafe {
        crate::arch::x86_64::restore_user_registers(&crate::arch::x86_64::UserRegisters::fresh())
    };
    #[cfg(feature = "fs-base-spawn-no-fresh")]
    // SAFETY: 上と同じ（FP の部分だけを戻す）。
    unsafe {
        crate::arch::x86_64::restore_fp_state(crate::arch::x86_64::UserRegisters::fresh().fp())
    };
    // **カーネルスタックの高水位を、Ring 3 へ落ちる直前にも出す**（`ADR-0068` の (c)。
    // 運用者の決定。2026-09-23）。
    //
    // **`kernel_main` の `stack-water:` の行は `init` の前までしか測っていない**——
    // **`/bin/zash` を読み込む経路（この関数が `FileTable` を作り直す所）が、そこより
    // 4KiB ほど深い**（`ADR-0068` の「起動時のスタックの最深経路」）。
    //
    // **深さ 0 のときだけ出す。** **入れ子（深さ 1 以上）はこのカーネルスタックの上に
    // 居ない**——親の遠征スタックの上である。
    //
    // **Ring 3 へ落ちた後、そのプログラムのカーネル入場は遠征スタックに乗る**ので、
    // **この値は、そのプログラムが走っている間ずっとの値である。**
    if crate::arch::x86_64::excursion_depth() == 0 {
        report_stack_water_before_ring3(logger, process.name);
    }

    // **DF=1 の文脈から入った割り込みを数える起点（2026-09-24）。** **`spin` は `std` の後で
    // 空回りする**ので、止められるまでに来たタイマはどれも DF=1 の文脈から入る（下の判定行）。
    let irq_entries_from_df_before =
        crate::arch::x86_64::entries_from_direction_flag_set(crate::arch::x86_64::EntryPath::Irq);
    // **走る CPU の印を立てる**（2026-10-07。[`UserProcess::ran_on`]）。
    process.ran_on |= 1u64 << common::percpu::cpu_id();
    // SAFETY: entry と stack は今マップしたユーザーページで、`ud2` が必ずフォルト
    // する。main_entry_stack_top はメインのカーネルスタック上端。単一実行文脈である。
    unsafe {
        crate::arch::x86_64::run_excursion(
            main_entry_stack_top,
            process.entry,
            process.stack_top,
            crate::syscall::window_for_subtree(USER_PROGRAM_SUBTREE_INDEX),
        )
    };
    // **引き取る。** 遠征が例外による終了処理で戻っても `exit` で戻ってもここを通る
    // （`ring3::run_excursion` はこの 2 つの longjmp でしか戻らない）。
    // **前景を返す。** 取った者だけが返す（入れ子の子は取れていない）。
    if claimed_foreground {
        crate::input::release_foreground();
    }
    process.files = crate::vfs::swap_current_files(previous_files);
    process.heap = swap_current_heap(previous_heap);
    // **止めたときは、前景の持ち主と止めた相手を両方出す（S12 前の手当て、C）。**
    //
    // **この 2 つは同じではない。** 前景を取るのは遠征の最も外側
    // （シェル）で、**止めるのは最も内側（子）である。**
    // `claim_foreground` は入れ子では偽を返し、**親が持ったまま子はその前景を
    // 通して読む。** **どこにも書かれていなかったので、判定行に出す。**
    if crate::arch::x86_64::excursion_interrupted() {
        // **止めた打鍵そのものを捨てる。** 残すと、次にシェルが読んだときに
        // `^C` がもう 1 つ出る（実測。[`crate::input::discard_typed_input`]）。
        //
        // 破壊テスト (S12 前の手当て C, kill-keep-typed-input): 捨てない。
        // **止めた直後のプロンプトに `^C` が余分に出る。**
        #[cfg(not(feature = "kill-keep-typed-input-test"))]
        crate::input::discard_typed_input();
        logger.info(format_args!(
            "interrupt: stopped {} at excursion depth {entered_at_depth}; the foreground is held \
             at depth {} (the holder is the outermost excursion, the target is the innermost), \
             claimed here={claimed_foreground}",
            process.name,
            crate::input::foreground_depth()
        ));
        // **方向フラグの前提（2026-09-24）。** **`--shell-test` が止める `spin` が作る。**
        // **0 なら、IRQ の入口が DF を降ろすという主張は何も確かめていない**
        // （`crate::arch::x86_64::idt::check_direction_flag`）。**判定は `xtask` が行う。**
        let irq_entries_from_df = crate::arch::x86_64::entries_from_direction_flag_set(
            crate::arch::x86_64::EntryPath::Irq,
        ) - irq_entries_from_df_before;
        logger.info(format_args!(
            "direction flag: {} was interrupted from a context with DF=1 {irq_entries_from_df} \
             time(s) while it ran, and every handler ran with DF=0 (the stub clears it; a \
             handler that sees DF=1 halts)",
            process.name
        ));
    }
    // **ユーザースタックをどれだけ使ったかを出す（EV。ADR-0041）。**
    //
    // **1 ページしかないので、環境を積むと減る側である。** **増やすかどうかを
    // 決める材料が、これまで 1 つも無かった**——**遠征スタックには高水位が
    // 在るのに、こちらには無かった。**
    //
    // **測りかたは遠征スタックと同じである**——**マップするときに既知のバイトで埋め、
    // 戻ってから、毒値でない一番下のバイトを探す。** 使用量は上端からそこまでである。
    //
    // **限界も同じである**——**プログラムが毒値そのものを書いたら、使ったとは
    // 数えられない。** **下側から数えるので、間に毒値が挟まっても影響しない。**
    report_user_stack_high_water(logger, process);
    // **`futex` で待つ場面に入って、終わらせたプロセス**（2026-10-06。`ADR-0081`）。スレッドが 1 本しか無い間は、
    // 起こす者が居ないので、偽りの戻り値を返さずに終わらせる。**行き詰まりを隠さない。**
    if let Some(address) = crate::syscall::futex_deadlock_address() {
        report_futex_deadlock(logger, process.name, address);
        // **出したら消す**——記録はスロットごとで、親（`spawn` で待っていた側）が終わるときに、子のものを
        // もう 1 度出さないため。
        crate::syscall::clear_futex_deadlock_address();
    }
    // **`tkill` で自分へ送ったシグナルで終わらせたプロセス**（2026-10-06）。配送が無いので、名指しして終わらせる。
    if let Some(signal) = crate::syscall::signal_that_ended_the_process() {
        report_signal_that_ended_the_process(logger, process.name, signal);
        crate::syscall::clear_signal_that_ended_the_process();
    }

    // **この遠征で遠征スタックをどれだけ使ったかを出す（S11-5）。**
    //
    // **あふれは、下の見張りのページが、その瞬間に止める**（2026-10-06。以前は見張りのページが無く、最下部の
    // 256 バイトを、戻ってから見ていた）。**ここで出すのは、あふれる前の、余りの推移である。**
    let used = crate::arch::x86_64::excursion_stack_high_water(entered_at_depth);
    let capacity = crate::arch::x86_64::excursion_stack_capacity();
    logger.info(format_args!(
        "ring3: {} used {used} of {capacity} byte(s) of the depth-{entered_at_depth} \
         excursion stack ({}%), which has a guard page below it",
        process.name,
        used * 100 / capacity
    ));
    // **使用量が、止まる線（容量の 4 分の 3）を越えたら止まる**（S11-6。線は 2026-10-06 に半分から移した）。
    //
    // **あふれは、下の見張りのページが止める。ここで止まるのは、その手前である**——余りが減ったことを、次に何かを
    // 足す人が「前からそうだった」として扱う前に、判断を求める。線の意味と、移した経緯は、
    // `ring3` の `EXCURSION_STACK_BUDGET` の doc に在る。
    if !crate::arch::x86_64::excursion_stack_within_budget(entered_at_depth) {
        let budget = crate::arch::x86_64::excursion_stack_budget();
        logger.error(format_args!(
            "ring3: the depth-{entered_at_depth} excursion stack is more than three quarters used \
             ({used} of {capacity}; the line is {budget}). it has a guard page below it, so an \
             overflow would be caught, but little room is left - decide here: take the process \
             record off this stack, or raise the capacity with a measurement. halting"
        ));
        common::arch::x86_64::halt_forever();
    }

    // **表が動いたことの観測（S10-b）。** 開いたまま戻ったものが何本あるかを出す。
    // **`syscall-test` は最後に閉じるので 0 で戻る**——ここが 0 でなければ、
    // 開いた fd が漏れている。
    logger.info(format_args!(
        "vfs: {} opened {} file(s) in total and left Ring 3 with {} still open \
         (MAX_OPEN_FILES={}); it was handed {} byte(s) of input",
        process.name,
        process.files.opened_total(),
        process.files.open_count(),
        crate::vfs::MAX_OPEN_FILES,
        crate::input::delivered_count()
    ));
    // **本番のテーブルへ戻し、タスクの CR3 の欄も戻す（W1-b-2。W1-c-2 で一続きにした）。**
    // **この空間の持ち主はこの後で
    // 破棄されるので、ここで戻さないと死んだテーブルを指したまま残る。**
    //
    // **戻す値は深さから引く**——**深さ 0 なら 0（ユーザー空間を載せていない）、
    // 入れ子なら入口で読んだ `production`（親の空間）である。** **上の
    // `main_entry_stack_top` と同じ読み方で、控えの局所変数を持たない。**
    // **控えを持つ形にしたら、遠征スタックの高水位が 96 バイト増えた**（実測。
    // `tools/boot-log-compare.py` が検出した。**`dev` では局所変数がそのまま
    // フレームを広げ、このフレームは子が走っている間ずっと深さ 0 のスタックに載る**）。
    // **`ring3::run_excursion` は深さを戻してから返るので、ここで読む深さは入口と同じである。**
    // SAFETY: 本番のテーブルへ戻す。上位は同じなので連続して実行できる。
    unsafe {
        crate::task::switch_page_table_root_and_note(
            production,
            if crate::arch::x86_64::excursion_depth() == 0 {
                0
            } else {
                production.as_u64()
            },
        )
    };

    Ok(())
}

/// ファイルシステムからイメージを読み、子プロセスとして走らせ、**終わるまで待つ**（S11-5）。
///
/// # 同期である
///
/// **戻るのは子が終わった後である。** 親（呼び出し元の Ring 3）は、その間
/// 入れ子の遠征の下で止まっている。**この関数は同期のままである。**
/// **待たない形は別の関数にした**——**W1-c-4 の `start_detached`（`concurrent-test` の構成だけ）が、
/// 足した 1 本のタスクの上でこの関数を呼ぶ。** **以前ここは「非同期にするにはスケジューラが要り、
/// それはまだ無い」と書いていた**（`docs/roadmap.md` の S11）。
///
/// # 深さで断る
///
/// [`MAX_EXCURSION_DEPTH`] に達していたら [`SpawnError::TooDeep`] を返す。
/// **入る前に断る**——[`crate::arch::x86_64::run_excursion`] は上限を越えた深さで呼ばれると
/// 遠征スタックと回復点を index 0 へ丸めるので、**親のものを踏む。**
/// **その状態には判定行が無い**ので、**踏ませずに断る側で閉じる。**
///
/// # BKL は保持していない
///
/// **呼ぶ前に解いてある**（`crate::syscall::syscall_entry`。`ADR-0023` §1）。
/// **子は Ring 3 で走り、システムコールごとに自分で BKL を取る。**
/// 保持したまま入ると、子の最初のシステムコールが同じコアの再取得になる。
///
/// # 親の観測を壊さない
///
/// 子は [`crate::syscall::reset_counters`] を通り、自分の `write` と `exit` を
/// 記録する。**親の記録はここで控えて戻す**（`crate::syscall::Records` と
/// [`crate::arch::x86_64::ring3::FoldRecord`]）。
/// 起動できるイメージかを、起動する前に確かめる（`ADR-0063` の (b3)）。
///
/// **[`spawn`] の探索と同じ 4 つを見る**——**在る・ディレクトリでない・通常ファイル・大きさ。**
/// **切り離して起動する入口が「見つからなければ同期で `-ENOENT`」を返すために切り出した**
/// ——**子の中で探すと、`-ENOENT` が親へ届くのは子が終わった後になり、シェルの `PATH` の輪が
/// 回せない。** **子はもう一度探す**（[`spawn`] の中）。**2 度探す費用は、イメージの索引を読むだけである。**
pub fn probe_program(path: &[u8]) -> Result<(), SpawnError> {
    let fs = crate::vfs::root_filesystem().map_err(SpawnError::Lookup)?;
    let inode = fs.lookup(path).map_err(SpawnError::Lookup)?;
    if inode.is_directory() {
        return Err(SpawnError::IsDirectory);
    }
    if !inode.is_regular_file() {
        return Err(SpawnError::NotRegularFile);
    }
    if inode.size > MAX_EXECUTABLE_SIZE as u64 {
        return Err(SpawnError::TooLarge(inode.size));
    }
    Ok(())
}

/// 次の `spawn` へ渡す fd（`ADR-0063` の (b3)）。**スロットごとに 1 つ。**
///
/// **`spawn` の引数を変えない**（W1-c の決定）。**代わりに、起こす直前に側道へ置き、
/// 子の表を作るところ（[`load_user_program`]）が取る**——**`DETACHED_REQUEST` と同じ形である。**
struct InheritedEnds {
    /// 子の fd 0 に据えるパイプの読み端。
    stdin: Option<u8>,
    /// 子の fd 1 に据えるパイプの書き端。
    stdout: Option<u8>,
}

static INHERITED_ENDS: [common::critical::Locked<InheritedEnds>;
    crate::arch::x86_64::USER_TASK_SLOTS] = [
    common::critical::Locked::new(InheritedEnds {
        stdin: None,
        stdout: None,
    }),
    common::critical::Locked::new(InheritedEnds {
        stdin: None,
        stdout: None,
    }),
];

/// 予約した読み端のうち、まだ次の `spawn` に渡していないもの（`ADR-0063` の (b3)）。
/// **スロットごとに 1 つ**——**シェルが `a | b` の左を起動してから右を起動するまでの間、ここに在る。**
static PENDING_STDIN: [common::critical::Locked<Option<u8>>; crate::arch::x86_64::USER_TASK_SLOTS] = [
    common::critical::Locked::new(None),
    common::critical::Locked::new(None),
];

/// 次の入れ子の `spawn` の fd 0 に据える読み端を置く。
pub fn set_inherit_stdin(slot: usize, pipe: u8) {
    INHERITED_ENDS[slot].lock().stdin = Some(pipe);
}

/// 置いてあった読み端を取る（無ければ `None`）。
pub fn take_inherit_stdin(slot: usize) -> Option<u8> {
    INHERITED_ENDS[slot].lock().stdin.take()
}

fn set_inherit_stdout(slot: usize, pipe: u8) {
    INHERITED_ENDS[slot].lock().stdout = Some(pipe);
}

fn take_inherit_stdout(slot: usize) -> Option<u8> {
    INHERITED_ENDS[slot].lock().stdout.take()
}

/// 予約した読み端を「次の `spawn` へ」として控える。
pub fn set_pending_stdin(slot: usize, pipe: u8) {
    *PENDING_STDIN[slot].lock() = Some(pipe);
}

/// 控えてある予約を見る（取らない）。
pub fn peek_pending_stdin(slot: usize) -> Option<u8> {
    *PENDING_STDIN[slot].lock()
}

/// 控えてある予約を取る。
pub fn take_pending_stdin(slot: usize) -> Option<u8> {
    PENDING_STDIN[slot].lock().take()
}

pub fn spawn(
    path: &[u8],
    argv_bytes: &[u8],
    argv_count: usize,
    envp: Option<(&[u8], usize)>,
) -> Result<SpawnOutcome, SpawnError> {
    // **深さの上限。入る前に断る。**
    let depth = crate::arch::x86_64::excursion_depth();
    if depth >= MAX_EXCURSION_DEPTH {
        return Err(SpawnError::TooDeep);
    }
    // **緩衝の番号は深さそのものである**（[`MAX_SPAWN_IN_FLIGHT`] の doc）。
    // **上の判定が `depth < MAX_EXCURSION_DEPTH` を保証しているので、範囲内である。**
    //
    // **深さ 0 からも呼べる（S11-11）。** `init` がカーネルの直線上から
    // シェルを起動する。**S11-5 の時点では `dispatch` からしか来なかったので、
    // 深さ 0 を不具合として拒んでいた。** 呼び出し側が増えたので、その判定を外した。
    // **この `slot` は深さの番号である。** 遠征のスロット（W1-c-1）は
    // `crate::arch::x86_64::current_excursion_slot` で引く。
    let slot = depth;

    let mut serial = Serial::primary();
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);
    // **シェルの後の最初の打鍵の配送を、ここで 1 度だけ報せる**（HW-e-2。`ADR-0068`）——**シェルが Enter の
    // エコーを終えた後なので、行の途中に入らない。** **パスを引く前なので、無い名前を打った回でも出る。**
    crate::keyboard::report_first_delivery_once(&mut logger);
    // **処理の無い源を禁止したことも、ここで 1 度だけ報せる**（2026-09-28。`ADR-0072` の 4。9d-4b）。
    crate::interrupts::report_arrivals_without_handler_once(&mut logger);

    let fs = crate::vfs::root_filesystem().map_err(SpawnError::Lookup)?;
    let inode = fs.lookup(path).map_err(SpawnError::Lookup)?;
    if inode.is_directory() {
        return Err(SpawnError::IsDirectory);
    }
    if !inode.is_regular_file() {
        return Err(SpawnError::NotRegularFile);
    }
    if inode.size > MAX_EXECUTABLE_SIZE as u64 {
        return Err(SpawnError::TooLarge(inode.size));
    }
    let size = inode.size as usize;

    // **パスを控える。** `name` と `argv[0]` に `&'static str` が要る
    // （[`SPAWN_PATHS`]）。**入らない分は切る**——`copy_user_path` が
    // [`PATH_MAX`] で切っているので、ここへ来る時点で収まっている。
    let name_len = path.len().min(PATH_MAX);
    // SAFETY: `slot` は [`MAX_SPAWN_IN_FLIGHT`] の範囲内で、その深さで走っている
    // のはこの 1 本だけである（深さの判定が入れ子の重なりを禁じている）。
    // 単一コアの実行文脈で、割り込みハンドラはここへ来ない。
    let path_slot: &'static mut [u8; PATH_MAX] = unsafe {
        &mut (*core::ptr::addr_of_mut!(SPAWN_PATHS))[crate::arch::x86_64::current_excursion_slot()]
            [slot]
    };
    path_slot[..name_len].copy_from_slice(&path[..name_len]);
    let name_bytes: &'static [u8] = &path_slot[..name_len];
    // **UTF-8 でなければ名前を伏せる。** パスは Ring 3 から来るバイト列で、
    // **ext2 も UTF-8 を要求しない。** 判定行に出すためだけの値なので、
    // **読めないことを理由に起動を拒まない。**
    let name = core::str::from_utf8(name_bytes).unwrap_or("<not utf-8>");

    // **像は写さない。範囲を読む口を作って、載せる側へ渡す**（2026-10-05。`common::ext2::FileImage`）。
    //
    // **以前は、ブロックごとに静的な配列へ写してから載せていた**（上限は 32 KiB）。載せる側が要るのは、先頭の部分と、
    // 区画ごとのページの分だけなので、ファイルシステムのブロックから、写す先のフレームへ直に読む。
    let image = common::ext2::FileImage::new(&fs, &inode);
    // **読めない大きさのファイルは、空間を作る前に、名前のある失敗で断る。** ext2 の 2 段目の間接ブロックを使う
    // ファイル（4 KiB のブロックで約 4.05 MiB を越える）は、どの範囲も読めない（`Ext2::file_block`）。**黙って
    // 途中で切らない。** 最初の 1 バイトを読んで確かめる（空のファイルは、載せる側が「短すぎる」と断る）。
    if size > 0 {
        use common::image_source::ImageSource;
        let mut first = [0u8; 1];
        if let Err(error) = image.read_at(0, &mut first) {
            return Err(SpawnError::Read(error));
        }
    }

    // **親の遠征スタックの残りを測る（S11-5）。**
    //
    // **起動時の `load_user_program` はメインのカーネルスタック（ガードページ付き）の
    // 上で走るが、`spawn` からのそれは親の遠征スタックの上で走る。**
    // **遠征スタックは `.bss` の配列で、ガードページが無い**——溢れても止まらず、
    // 隣を静かに書く。**推測せずに測って出す。**
    // **深さ 0 の親は遠征スタックの上に居ない（S11-11 で直した）。**
    // **親は自分のタスクのカーネルスタック（ガードページ付き）の上に居る**——
    // `init` ならメインのカーネルスタック、切り離して起動した 1 本（W1-c-4）なら
    // そのタスクのスタックである。**そちらは測らない**——
    // **測る値打ちがあるのは、ガードの無い遠征スタックのほうである。**
    //
    // **行の文言は W1-c-4 の後に直した。** **以前は「the main kernel stack」と書いており、
    // 切り離して起動した 1 本から呼んだときに嘘になった**（運用者の指摘。2026-09-16）。
    let stack_probe = 0u8;
    let sp_now = &stack_probe as *const u8 as u64;
    if depth == 0 {
        logger.info(format_args!(
            "spawn: {name} is {size} byte(s) at inode {}; entering at depth {} (the parent \
             runs on its task's kernel stack, which has a guard page)",
            inode.number,
            depth + 1
        ));
    } else {
        let (excursion_bottom, excursion_top) = crate::arch::x86_64::excursion_stack_range();
        let stack_used = excursion_top.saturating_sub(sp_now);
        let stack_left = sp_now.saturating_sub(excursion_bottom);
        logger.info(format_args!(
            "spawn: {name} is {size} byte(s) at inode {}; entering at depth {} (the parent's \
             excursion stack {excursion_bottom:#x}..{excursion_top:#x} has {stack_used} byte(s) \
             used and {stack_left} left)",
            inode.number,
            depth + 1
        ));
    }

    // **子が起動した孫の隔離を控える（S11-11）。**
    //
    // **S11-5 で起動時の会計に同じ穴があり、そこは直した**——隔離のフレームは
    // 世代が退くまでアロケータへ戻らないので、**親から見ると消えたままである。**
    // **`spawn` 自身の会計にも同じ穴が残っていた。**
    // **シェルが `ls` と `cat` と `hello` を起動したところで出た**——
    // 実測で 35 枚消えて、シェル自身の隔離は 9 枚だった（9 + 9 + 9 + 8）。
    let (children_before, leaked_before) = spawn_accounting();
    let released_before_children = spawn_released_others();
    // **会計のウィンドウを開く（`ADR-0063` の (b1)）。** **他のウィンドウと交差したら、大域の差は主張しない。**
    let window = open_spawn_window();

    // **戻ってくるべき RSP0 を控える（S11-11 で直した）。**
    //
    // **S11-5 では「親の遠征スタックの上端」と突き合わせていた。** あのときは
    // `spawn` が `dispatch` からしか来ず、**親が必ず遠征の中にいた。**
    // **`init` が深さ 0 から呼ぶようになって、その前提が消えた**——
    // 深さ 0 の親はメインのカーネルスタックの上に居る。
    //
    // **控えて突き合わせる形なら、どちらの深さでも同じ 1 行で言える**
    // ——**「子が走る前と後で RSP0 が変わっていない」。**
    let entry_stack_before = crate::arch::x86_64::active_kernel_entry_stack_top();

    // **親の記録を控える。** 子は `reset_counters` を通る。
    let saved_records = crate::syscall::save_records();
    let saved_fold = crate::arch::x86_64::save_fold_record();

    // **会計のために借りて、すぐ返す**（`ADR-0030`）。**借りられなければ
    // 子も起動できない**ので、そのまま [`UserLoadError::AllocatorUnavailable`] へ落とす。
    let shared_before = crate::shm::frames_held();
    let allocator_waits_before = crate::syscall::allocator_waits();
    let free_before = match crate::frame_allocator::take() {
        Some(allocator) => {
            let count = allocator.free_frame_count();
            crate::frame_allocator::give_back(allocator);
            count
        }
        None => {
            crate::syscall::restore_records(saved_records);
            crate::arch::x86_64::restore_fold_record(saved_fold);
            return Err(SpawnError::Load(UserLoadError::AllocatorUnavailable));
        }
    };

    // **`argv` を控えて、`&'static [u8]` の並びへ切り分ける（S11-7）。**
    //
    // **切り分けは NUL で行う。** `copy_user_string_array` が要素ごとに NUL を付けて
    // 並べているので、**要素数だけ NUL があるはずである。** 無ければこちらの
    // 不具合なので、[`SpawnError::ArgvMalformed`] で止める。
    // SAFETY: `slot` は [`MAX_SPAWN_IN_FLIGHT`] の範囲内で、その深さで走っている
    // のはこの 1 本だけである（深さの判定が入れ子の重なりを禁じている）。
    // 単一コアの実行文脈で、割り込みハンドラはここへ来ない。
    let argv_slot: &'static mut [u8; MAX_ARGV_BYTES] = unsafe {
        &mut (*core::ptr::addr_of_mut!(SPAWN_ARGVS))[crate::arch::x86_64::current_excursion_slot()]
            [slot]
    };
    argv_slot[..argv_bytes.len()].copy_from_slice(argv_bytes);
    let stored: &'static [u8] = &argv_slot[..argv_bytes.len()];

    // 破壊テスト (S11-7, spawn-argv-drop-last): 最後の 1 本を落とす。
    // **終端の扱いを 1 つずらす形で、雑に見ると「ちゃんと切り分けている」ように
    // 見える。** 子が受け取る `argc` が 1 つ少なくなり、`spawn-test` の検算が
    // 食い違いを検出する。
    let argv_count = if cfg!(feature = "spawn-argv-drop-last") {
        argv_count.saturating_sub(1)
    } else {
        argv_count
    };

    let mut argv_slices: [&'static [u8]; MAX_ARGV] = [b""; MAX_ARGV];
    let mut at = 0usize;
    for slice in argv_slices.iter_mut().take(argv_count) {
        let Some(end) = stored[at..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|i| at + i)
        else {
            crate::syscall::restore_records(saved_records);
            crate::arch::x86_64::restore_fold_record(saved_fold);
            return Err(SpawnError::ArgvMalformed);
        };
        *slice = &stored[at..end];
        at = end + 1;
    }
    let argv: &[&[u8]] = &argv_slices[..argv_count];

    // **`envp` も同じ形で控えて切り分ける（f-2。`ADR-0053`）。**
    //
    // **`None` はカーネル側の呼び出しである**（`init` が `/bin/zash` を起動する形）。
    // **そのときは起動時の環境を積む**——`load_user_program` が表から採る。
    // **Ring 3 から来た `spawn` は必ず配列を渡す**（`copy_user_string_array` が
    // NULL を `-EFAULT` で断る。`ADR-0053` の Decision 2）。
    let mut envp_slices: [&'static [u8]; MAX_ENVP] = [b""; MAX_ENVP];
    let envp: Option<&[&[u8]]> = match envp {
        None => None,
        Some((envp_bytes, envp_count)) => {
            // **`argv` と同じく NUL 区切りで来る**——`copy_user_string_array` が
            // 要素ごとに NUL を付けて並べている。**要素数だけ NUL があるはずで、
            // 無ければこちらの不具合である。**
            // SAFETY: `slot` は [`MAX_SPAWN_IN_FLIGHT`] の範囲内で、その深さで走って
            // いるのはこの 1 本だけである（深さの判定が入れ子の重なりを禁じている）。
            // 単一コアの実行文脈で、割り込みハンドラはここへ来ない。
            let envp_slot: &'static mut [u8; MAX_ENVP_BYTES] = unsafe {
                &mut (*core::ptr::addr_of_mut!(SPAWN_ENVPS))
                    [crate::arch::x86_64::current_excursion_slot()][slot]
            };
            envp_slot[..envp_bytes.len()].copy_from_slice(envp_bytes);
            let env_stored: &'static [u8] = &envp_slot[..envp_bytes.len()];

            let mut env_at = 0usize;
            for slice in envp_slices.iter_mut().take(envp_count) {
                let Some(end) = env_stored[env_at..]
                    .iter()
                    .position(|byte| *byte == 0)
                    .map(|i| env_at + i)
                else {
                    crate::syscall::restore_records(saved_records);
                    crate::arch::x86_64::restore_fold_record(saved_fold);
                    return Err(SpawnError::ArgvMalformed);
                };
                *slice = &env_stored[env_at..end];
                env_at = end + 1;
            }
            Some(&envp_slices[..envp_count])
        }
    };

    // **親の FP の状態を控える（`ADR-0058` の Decision 2）。**
    //
    // **`spawn` は新しいタスクを作らない**——**同じタスクの上で遠征が入れ子に
    // なり、親はカーネルの中で子の終了を待つ。** **切り替えは 1 度も起きないので、
    // 切り替えの退避（[`crate::task`]）では守れない。** 子が XMM を使えば、
    // 親が Ring 3 に持っていた値はそのまま消える。
    //
    // 破壊テストでの確認: `fp-spawn-no-save` では控えない。**親が `spawn` を跨いで
    // 浮動小数点の値を保てなくなる。**
    //
    // **FS と GS の基底も、一緒に控える**（2026-10-05。`arch` の `UserRegisters`）。子は基底を 0 から始め、
    // 自分で入れ直しうる。
    #[cfg(not(feature = "fp-spawn-no-save"))]
    let parent_fp = {
        let mut area = crate::arch::x86_64::UserRegisters::fresh();
        // SAFETY: 単一の実行文脈で、この領域はこの関数の中にしかない。
        unsafe { crate::arch::x86_64::save_user_registers(&mut area) };
        area
    };

    let (outcome, held, leaked, taken) =
        load_user_program_from(&mut logger, &image, true, name, argv, envp);

    // **親の FP の状態を戻す。** **子が XMM に残したものを消す**ので、
    // 情報の漏れも同時に塞がる（決定 4 と同じ向きである）。
    //
    // 破壊テストでの確認: `fs-base-spawn-no-restore` では、FP だけを戻して基底を戻さない。**親が、子の基底
    // （子が入れていなければ 0）のまま走り続ける。**
    #[cfg(all(
        not(feature = "fp-spawn-no-save"),
        not(feature = "fs-base-spawn-no-restore")
    ))]
    // SAFETY: `parent_fp` は、直前に `save_user_registers` が書いたものである。
    unsafe {
        crate::arch::x86_64::restore_user_registers(&parent_fp)
    };
    #[cfg(all(
        not(feature = "fp-spawn-no-save"),
        feature = "fs-base-spawn-no-restore"
    ))]
    // SAFETY: `parent_fp` の FP の部分は、直前に `fxsave` が書いた 512 バイトである。
    unsafe {
        crate::arch::x86_64::restore_fp_state(parent_fp.fp())
    };

    // **子の終わり方をここで読む。** 戻す前に読まなければ、親のもので上書きされる。
    // **中断を先に見る（S12 前の手当て、C）。** **`exit` も例外による終了処理も通っていない**
    // ので、先に見なければ `Folded(0)` に化ける。
    let child = if crate::arch::x86_64::excursion_interrupted() {
        SpawnOutcome::Interrupted
    } else if crate::syscall::process_exited() {
        SpawnOutcome::Exited(crate::syscall::process_exit_status())
    } else {
        SpawnOutcome::Folded(crate::arch::x86_64::excursion_fault_number())
    };
    let syscalls = crate::syscall::invocation_count();
    let (unknown_numbers, last_unknown) = crate::syscall::unknown_numbers();
    let last_unknown = last_unknown.unwrap_or(0);

    // **子のカーネル入場が、子の遠征スタックの上で起きたことを見る（S11-5）。**
    //
    // **入れ子で最も静かに壊れる形がこれである**——RSP0 が親のスタックを指したまま
    // だと、子のシステムコールが**親のカーネルフレームを上書きする。**
    // **子は正しく走り終え、親が戻った先で壊れる**ので、原因から離れた場所で落ちる。
    // **実測で踏んだ**（S11-5。`docs/troubleshooting.md`）。
    // **戻ってきた RSP0 が、親の遠征スタックの上端であることを確かめる（S11-5）。**
    //
    // **ここが違うと、親が次にカーネルへ入るときのスタックが変わる。**
    // **すぐには壊れない**——親はそのまま Ring 3 へ返り、次のシステムコールで
    // 別のスタックに乗る。**壊れるのは、次に子を起動して親のフレームを踏んだ
    // ときである。** 原因から遠いので、ここで突き合わせる。
    let entry_stack_after = crate::arch::x86_64::active_kernel_entry_stack_top();
    if entry_stack_after != entry_stack_before {
        logger.error(format_args!(
            "spawn: RSP0 came back as {entry_stack_after:#x} but it was {entry_stack_before:#x} before the \
             child ran; the parent's next kernel entry would land on the wrong stack. halting"
        ));
        common::arch::x86_64::halt_forever();
    }

    let child_handler_sp = crate::syscall::handler_sp();
    let (child_bottom, child_top) = crate::arch::x86_64::excursion_stack_range_at(depth);
    let handler_on_child_stack = child_handler_sp >= child_bottom && child_handler_sp < child_top;

    let free_after = match crate::frame_allocator::take() {
        Some(allocator) => {
            let count = allocator.free_frame_count();
            crate::frame_allocator::give_back(allocator);
            count
        }
        None => free_before,
    };

    // **親の記録を戻す。**
    //
    // 破壊テスト (S11-5, spawn-keep-child-records): 戻さない。**子が送ったバイト列と
    // 終了状態が、親のものとして判定行に出る。** 親（`syscall-test`）の
    // `write` の突き合わせが食い違って検出する。
    #[cfg(not(feature = "spawn-keep-child-records"))]
    {
        crate::syscall::restore_records(saved_records);
        crate::arch::x86_64::restore_fold_record(saved_fold);
    }

    // **共有フレームを `consumed` から除く（`ADR-0065` の (A-3)）。** **ウィンドウの間にアロケータから
    // 取ったまま返っていない共有フレームは、`consumed` に入るが `AddressSpace::detach` が飛ばして
    // `quarantined` に入らない**——**その差を消す。** **(E) では 0**（プールはアロケータの外）。
    // **失うもの**——**`consumed == quarantined` の素の等式（共有分について）。**
    // **覆う判定**——**`shm` の created==released（フレームは参照数で返る。`shm:` の計測）。**
    let shared_after = crate::shm::frames_held();
    let shared_net = shared_after.saturating_sub(shared_before) as usize;
    // **その場で返した分と、途中で退いて返した分は、消えた数に入らない**（2026-10-07）。**隔離に残る分だけが「消えたまま」
    // である。** 前の破棄のフレームがこの窓の中で返っていれば（`released_others`）、その分を消えた数へ足し戻す。
    let destroy = last_destroy();
    let own_still = destroy.still_quarantined();
    let released_by_children = spawn_released_others().saturating_sub(released_before_children);
    let consumed = (free_before as i64 - free_after as i64 - shared_net as i64
        + destroy.released_others as i64
        + released_by_children as i64)
        .max(0) as usize;
    // **ウィンドウを閉じる。** **交差していたら、大域の差は相手の分を取り込んでいる**（`ADR-0063` の (b1)）。
    let crossed = close_spawn_window(window);
    // **孫のぶんを足す。** 子が更に起動していれば、そのぶんも消えている。
    let (children_after, leaked_after) = spawn_accounting();
    let quarantined = own_still + children_after.saturating_sub(children_before);
    let all_leaked = leaked + leaked_after.saturating_sub(leaked_before);
    // **親の会計へ回す（S11-5）。** 隔離に残るフレームは世代が退くまで
    // アロケータへ戻らないので、**親から見ると消えたままである。**
    SPAWN_QUARANTINED.fetch_add(own_still, core::sync::atomic::Ordering::SeqCst);
    SPAWN_RELEASED_OTHERS.fetch_add(
        destroy.released_others,
        core::sync::atomic::Ordering::SeqCst,
    );
    SPAWN_LEAKED.fetch_add(leaked, core::sync::atomic::Ordering::SeqCst);
    logger.info(format_args!(
        "spawn: {name} ended ({child:?}) after {syscalls} syscall(s) ({unknown_numbers} with a \
         number the kernel does not know; the last such number was {last_unknown}); its kernel \
         entries ran on RSP {child_handler_sp:#x} (inside its own excursion stack \
         {child_bottom:#x}..{child_top:#x} = {handler_on_child_stack}); the space was destroyed \
         ({consumed} frame(s) left the allocator; {} returned at once, {quarantined} still quarantined \
         ({own_still} its own + {} from what it spawned), match={} leaked={all_leaked}); the space \
         took {taken} frame(s) and the destroy collected {} (match={}); the global difference \
         was {} (checked {} time(s) so far)",
        destroy.returned,
        children_after.saturating_sub(children_before),
        consumed == quarantined,
        held + leaked,
        held + leaked == taken,
        if crossed {
            "not checked because another spawn window crossed this one"
        } else {
            "checked"
        },
        global_difference_checks()
    ));
    // **システムコールがアロケータを待った回数を出す**（2026-10-08。`crate::syscall` の `borrow_allocator`）。0 のときは
    // 出さない（ほかのタスクが借りている間にだけ増える）。諦めた回のシステムコールは、何も変えずに `-ENOMEM` を返している。
    let allocator_waits_after = crate::syscall::allocator_waits();
    if allocator_waits_after != allocator_waits_before {
        logger.warn(format_args!(
            "spawn: while {name} ran, system calls waited {} tick(s) for the frame allocator (another task held it) \
             and gave up {} time(s) without changing anything",
            allocator_waits_after.0 - allocator_waits_before.0,
            allocator_waits_after.1 - allocator_waits_before.1
        ));
    }

    // **ロードの失敗を 1 行で出す（ADR-0039）。**
    //
    // **以前は 1 行も出なかった。** 上の `ended ({child:?})` の行は
    // **ロードに失敗しても印字される**ので、走ったように読める——
    // **`/bin/zi` の切り分けが遠回りになった直接の原因である。**
    let entry = match outcome {
        Ok(entry) => entry,
        Err(error) => {
            logger.error(format_args!("spawn: {name} could not be loaded: {error:?}"));
            return Err(SpawnError::Load(error));
        }
    };
    let _ = entry;

    // **空間ごとの一致はいつも見る。** **大域の差は、交差しなかったときだけ見る**
    // （`ADR-0063` の (b1)。**交差したときは上の行が「確かめなかった」と書く**）。
    if held + leaked != taken {
        logger.error(format_args!(
            "spawn: {name} left the space short: the space took {taken} frame(s) but the destroy \
             collected {} ({held} quarantined + {leaked} leaked)",
            held + leaked
        ));
        return Err(SpawnError::DestroyAccounting {
            consumed: taken,
            quarantined: held + leaked,
            leaked: all_leaked,
        });
    }
    if !crossed && (consumed != quarantined || all_leaked != 0) {
        logger.error(format_args!(
            "spawn: {name} left the allocator short: {consumed} frame(s) consumed but \
             {quarantined} quarantined ({all_leaked} leaked)"
        ));
        return Err(SpawnError::DestroyAccounting {
            consumed,
            quarantined,
            leaked: all_leaked,
        });
    }

    Ok(child)
}

/// 切り離して走らせる 1 本の依頼（W1-c-4）。**パスと `argv` と要素数である。**
/// 切り離して起動する依頼（`ADR-0063` の (b3) で置き場つきに作り直した）。
///
/// **W1-c-4 では `&'static [u8]` の 3 つ組だった**（依頼するのが `init` で、イメージの中の文字列を
/// 渡せた）。**Ring 3 から来る `path` / `argv` / `envp` はシステムコールのスタックのコピーなので、
/// 静的な置き場が要る。** **大きさは `spawn_from_ring3` のコピーと同じである**（256 + 1024 + 1024）。
///
/// **`Locked` の中に置くので `.data` へ行く**（(b2) の実測と同じ機序。
/// `docs/coding-standards.md` の「コードが増える段では」の 3）。
struct DetachedRequest {
    path: [u8; crate::syscall::PATH_MAX],
    path_len: usize,
    argv: [u8; crate::syscall::MAX_ARGV_BYTES],
    argv_used: usize,
    argv_count: usize,
    envp: [u8; crate::syscall::MAX_ENVP_BYTES],
    envp_used: usize,
    envp_count: usize,
    has_envp: bool,
    /// 子の fd 1 に据えるパイプの書き端。
    stdout_pipe: Option<u8>,
}

impl DetachedRequest {
    const EMPTY: Self = Self {
        path: [0; crate::syscall::PATH_MAX],
        path_len: 0,
        argv: [0; crate::syscall::MAX_ARGV_BYTES],
        argv_used: 0,
        argv_count: 0,
        envp: [0; crate::syscall::MAX_ENVP_BYTES],
        envp_used: 0,
        envp_count: 0,
        has_envp: false,
        stdout_pipe: None,
    };
}

/// 足した 1 本のタスクへ渡す依頼（W1-c-4）。**[`start_detached`] が置き、そのタスクが取る。**
static DETACHED_REQUEST: common::critical::Locked<Option<DetachedRequest>> =
    common::critical::Locked::new(None);

/// プログラムを切り離して走らせる（W1-c-4。`ADR-0060`）。**終わるのを待たずに戻る。**
///
/// # [`spawn`] との違い
///
/// **`spawn` は親が待つ形で、変えていない。** **こちらは、足した 1 本のタスク（`crate::task` の
/// `RING3_TASK`）に依頼を渡して `Ready` にするだけである。** **そのタスクが、自分のスロット（1）の
/// 深さ 0 から `spawn` を呼ぶ**——**読み込み・遠征・破棄・会計は `spawn` の経路そのものである。**
///
/// # 前景は取らない
///
/// [`crate::input::claim_foreground`] がスロット 1 を断る。
///
/// # 既定の起動にも在る（`ADR-0063` の (a)。2026-09-18）
///
/// **W1-c-4 では `concurrent-test` の構成にだけ置いていた**（最初の利用者はその構成の `init`）。
/// **シェルの `|` が 2 本を同時に走らせるので、足した 1 本のスタックとガードページと一緒に
/// 既定の起動へ出した。** **既定の起動で呼ぶ者は、まだ居ない**——**(b)(c) でシェルが呼ぶ。**
pub fn start_detached(
    path: &[u8],
    argv_bytes: &[u8],
    argv_count: usize,
    envp: Option<(&[u8], usize)>,
    stdout_pipe: Option<u8>,
) -> Option<u64> {
    let mut request = DetachedRequest::EMPTY;
    let path_len = path.len().min(request.path.len());
    request.path[..path_len].copy_from_slice(&path[..path_len]);
    request.path_len = path_len;
    let argv_used = argv_bytes.len().min(request.argv.len());
    request.argv[..argv_used].copy_from_slice(&argv_bytes[..argv_used]);
    request.argv_used = argv_used;
    request.argv_count = argv_count;
    if let Some((envp_bytes, envp_count)) = envp {
        let envp_used = envp_bytes.len().min(request.envp.len());
        request.envp[..envp_used].copy_from_slice(&envp_bytes[..envp_used]);
        request.envp_used = envp_used;
        request.envp_count = envp_count;
        request.has_envp = true;
    }
    request.stdout_pipe = stdout_pipe;
    *DETACHED_REQUEST.lock() = Some(request);
    let handle = crate::task::start_ring3_task();
    if handle.is_none() {
        // **起動できなかったら依頼を片づける**——**次に起動する者が古い依頼を走らせないため。**
        *DETACHED_REQUEST.lock() = None;
    }
    handle
}

/// 切り離して起動した子の終わり方（`ADR-0063` の (b2)）。**ハンドルと一緒に置く。**
///
/// **`SpawnOutcome` をそのまま置かない**——**`SpawnError` は `Copy` ではない。**
/// **待つ側が要るのは「終わったこと」と「終わり方」なので、`SYS_SPAWN` と同じ形の
/// ビットへまとめる**（[`crate::abi::private::SPAWN_FOLDED_FLAG`]）。
static DETACHED_STATUS: common::critical::Locked<Option<(u64, u64)>> =
    common::critical::Locked::new(None);

/// 待つ入口が返す、子の終わり方（`ADR-0063` の (b2)）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    /// 終わり方のビット（`SYS_SPAWN` と同じ形）。
    Ended(u64),
    /// ハンドルが合わない（終わった後の二重待ちも、これである）。
    NoSuchChild,
}

/// 切り離して起動した子が終わるのを待ち、回収する（`ADR-0063` の (b2)）。
///
/// # 上限は置かない
///
/// **本番の待ちは上限で止めない**（`ADR-0061`）。**空回りし続ける子を待つと永久に待つ**
/// ——**(b2) の限界として `ADR-0063` に書いた。**
///
/// # 回収してから戻る
///
/// **`Finished` になったら終わり方を読み、タスクを `Uninitialized` へ戻す**
/// （`crate::task::reap_ring3_task`）。**戻せば次の `|` で起動できる。**
pub fn wait_for_ring3_task(handle: u64) -> ChildStatus {
    if !crate::task::handle_is_current(handle) {
        return ChildStatus::NoSuchChild;
    }
    loop {
        // **「終わったか」を見てから待ちの欄を据えるまでの間、割り込みを止める。**
        //
        // **`set_current_waiting` の doc が「割り込みを止めた文脈から呼ぶこと」と書いている。**
        // **`sys_read` と `sys_nanosleep` は `int 0x80`（割り込みゲート）の内側なので IF=0 だが、
        // ここは `init` のカーネル文脈で IF=1 である**——**ウィンドウが開く。**
        //
        // **逃すと永久に待つ**——**打鍵やティックと違って、子の終わりは 1 度しか起こさない。**
        // **見てから据えるまでにティックが食い込み、その先で子が終わって起こすと、まだ待って
        // いない親は見つからない。** **その後に親が「待っている」と書くと、二度と起きない。**
        // 破壊テスト (`ADR-0063` の (b2), wait-window-is-wide): ウィンドウを広げる。**ガードを取らず、
        // ティックが 2 つ入るまで空回りする**——**その間に子が終わると、起こしが取りこぼされる。**
        // **「機会が無い」破壊テストを、機会を作って落とす形である**（`docs/coding-standards.md` の
        // 「破壊テストが「機会が無い」になるなら、用意する前に機会を作れないかを見る」）。
        #[cfg(not(feature = "wait-window-is-wide"))]
        let guard = common::critical::InterruptGuard::enter();
        if crate::task::ring3_task_finished() {
            break;
        }
        #[cfg(feature = "wait-window-is-wide")]
        {
            let opened = crate::arch::x86_64::monotonic_ticks();
            while crate::arch::x86_64::monotonic_ticks().saturating_sub(opened) < 2 {
                core::hint::spin_loop();
            }
        }
        crate::task::set_current_waiting(crate::task::Wait::Child { handle });
        // **譲る前に割り込みを戻す。** **据えた後のウィンドウは無害である**——**親は既に「待っている」
        // ので、そこで起こされれば走行可能へ戻り、譲ってもすぐ選ばれる。**
        #[cfg(not(feature = "wait-window-is-wide"))]
        drop(guard);
        crate::task::yield_now();
        if !crate::task::handle_is_current(handle) {
            return ChildStatus::NoSuchChild;
        }
    }
    let status = match *DETACHED_STATUS.lock() {
        Some((stored, bits)) if stored == handle => bits,
        _ => u64::MAX,
    };
    if !crate::task::reap_ring3_task(handle) {
        return ChildStatus::NoSuchChild;
    }
    ChildStatus::Ended(status)
}

/// 足した 1 本のタスクが、渡された依頼を走らせる（W1-c-4）。**そのタスクの本体だけが呼ぶ。**
pub fn run_detached_request() {
    // **依頼は静的の置き場から借りる。値で取らない（`ADR-0063` の (b3)）。** **値で取ると
    // 約 2.4 KiB がこのタスクのカーネルスタックに 2 度乗り**（`take()` と `let Some(..)` で、
    // dev プロファイルはまとめない）**、高水位が半分の見張りを越えて止まった**（実測。
    // 33,864 / 65,536 バイト）。
    let request: *const DetachedRequest = match &*DETACHED_REQUEST.lock() {
        Some(request) => request as *const DetachedRequest,
        None => core::ptr::null(),
    };
    let mut serial = Serial::primary();
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);
    if request.is_null() {
        logger.error(format_args!(
            "detached: the ring3 task started without a request; halting"
        ));
        common::arch::x86_64::halt_forever();
    }
    // SAFETY: 置き場は `static` なのでアドレスは生き続ける。**書く者は `start_detached` だけで、
    // このタスクが走っている間は `start_ring3_task` が起動を断る**ので、読んでいる間に
    // 書き換えられることは無い。**ロックは上で外してある**（持ったまま `spawn` へ入ると、
    // `Locked` が割り込みを止めたままになる）。
    let request: &DetachedRequest = unsafe { &*request };
    let path = &request.path[..request.path_len];
    let argv_bytes = &request.argv[..request.argv_used];
    let argv_count = request.argv_count;
    let envp = request
        .has_envp
        .then_some((&request.envp[..request.envp_used], request.envp_count));
    let name = core::str::from_utf8(path).unwrap_or("<not utf-8>");
    let slot = crate::arch::x86_64::current_excursion_slot();
    logger.info(format_args!(
        "detached: starting {name} on ring3 slot {slot}"
    ));
    // **子の fd 1 に据える書き端を側道へ置く（`ADR-0063` の (b3)）。** **`spawn` の中で子の表を
    // 作るときに取られる。**
    if let Some(pipe) = request.stdout_pipe {
        set_inherit_stdout(slot, pipe);
    }
    let outcome = spawn(path, argv_bytes, argv_count, envp);
    // **取られなかったら（読み込みの前に失敗した）、書き端を返す**——**読み手が EOF を見に行ける。**
    if let Some(unused) = take_inherit_stdout(slot) {
        crate::pipe::close_write_end(unused);
    }
    // **終わり方をハンドルと一緒に置く（`ADR-0063` の (b2)）。** **待つ側がこれを読む。**
    let bits = match &outcome {
        Ok(ending) => crate::syscall::spawn_status(ending),
        Err(_) => u64::MAX,
    };
    *DETACHED_STATUS.lock() = Some((crate::task::current_ring3_task_handle(), bits));
    logger.info(format_args!(
        "detached: {name} ended ({outcome:?}) on ring3 slot {slot}"
    ));
}

#[cfg(test)]
mod tests {
    /// 既定に `HOME` が入っていること（f-1。`ADR-0052` の Decision 5）。
    ///
    /// **`~` の展開が `HOME` を引くので、既定に無いと、設定ファイルが
    /// 無いときだけ `~` が展開されなくなる。**
    #[test]
    fn the_default_environment_carries_home() {
        assert!(super::DEFAULT_ENVIRONMENT
            .iter()
            .any(|line| line.starts_with(b"HOME=")));
    }

    /// 位置を決めてリンクした像の配置（2026-10-06 に、スタックを 4 ページにして、下に見張りのページを置いた）。
    /// **像の番地と `mmap` の始まりは、今までの定数と同じ値である。** ヒープの上端は、見張りのページの下端である。
    #[test]
    fn the_executable_layout_has_a_four_page_stack_above_a_guard() {
        let layout = super::ProcessLayout::for_kind(common::elf::ElfKind::Executable);
        assert_eq!(layout, super::ProcessLayout::EXECUTABLE);
        assert_eq!(layout.stack_top, 0x0080_0000);
        assert_eq!(layout.stack_bytes, 4 * 4096);
        assert_eq!(layout.stack_bottom(), 0x007f_c000);
        assert_eq!(layout.stack_guard_page(), Some(0x007f_b000));
        assert_eq!(layout.heap_limit, 0x007f_b000);
        assert_eq!(layout.mmap_base, crate::syscall::MMAP_BASE);
        // 範囲では断らない（今までどおり、写す所が断る）。
        let policy = layout.load_policy(common::elf::ElfKind::Executable);
        assert_eq!(policy.position_independent_base, 0);
        assert_eq!(policy.window, (0, 1 << 47));
        assert_eq!(policy.max_span, u64::MAX);
    }

    /// 位置独立の像の配置。**像・ヒープ・`mmap`・見張りのページ・スタックが、この順に並び、重ならない。**
    #[test]
    fn the_position_independent_layout_keeps_its_regions_apart() {
        let layout = super::ProcessLayout::for_kind(common::elf::ElfKind::PositionIndependent);
        let policy = layout.load_policy(common::elf::ElfKind::PositionIndependent);
        assert_eq!(policy.position_independent_base, 0x40_0000);
        assert_eq!(policy.max_span, 32 * 1024 * 1024);
        // 像は、ずらす量からヒープの上端まで。上限の大きさの像が、そこに収まる。
        assert_eq!(policy.window, (0x40_0000, layout.heap_limit));
        assert!(policy.window.0 + policy.max_span <= layout.heap_limit);
        // ヒープの上端は、mmap の始まりより下。
        assert!(layout.heap_limit < layout.mmap_base);
        // mmap の終わりが見張りのページで、その 1 ページ上からスタック。
        let guard = layout
            .stack_guard_page()
            .expect("this layout has a guard page");
        assert_eq!(layout.mmap_limit, guard);
        assert_eq!(guard + 4096, layout.stack_bottom());
        assert_eq!(layout.stack_top - layout.stack_bottom(), 256 * 1024);
        assert_eq!(layout.stack_bytes % 4096, 0);
        // スタックの上端は、ユーザーの番地の上限より下で、ページの境界に在る。
        assert!(layout.stack_top <= 0x0000_7fff_ffff_f000);
        assert_eq!(layout.stack_top % 4096, 0);
    }

    /// **`mmap` の終わりは、どちらの配置でもユーザーの番地の上限を越えない**（2026-10-09）。上限と `mmap_limit` は、どちらも
    /// 範囲の終端で、その値を含まない。上端に置いた小さな表で、末尾が上限ちょうどの範囲は配られ、1 ページでも越える範囲は
    /// 配られないことを見る（大きな領域は取らない）。
    #[test]
    fn mmap_hands_out_ranges_up_to_the_user_address_limit_and_no_further() {
        use crate::arch::x86_64::USER_ADDRESS_LIMIT;
        use crate::mappings::{MapError, MappingKind, MemoryMap};
        const PAGE: u64 = 4096;
        let executable = super::ProcessLayout::for_kind(common::elf::ElfKind::Executable);
        let pie = super::ProcessLayout::for_kind(common::elf::ElfKind::PositionIndependent);
        assert_eq!(executable.mmap_limit, USER_ADDRESS_LIMIT);
        assert!(pie.mmap_limit < USER_ADDRESS_LIMIT);
        // 上限の 3 ページ下から配る表。2 ページ、1 ページで、末尾が上限ちょうどになる。
        let mut map = MemoryMap::INACTIVE;
        map.activate(executable.mmap_limit - 3 * PAGE, executable.mmap_limit);
        assert_eq!(
            map.reserve(2 * PAGE, MappingKind::Anonymous, true, true),
            Ok(USER_ADDRESS_LIMIT - 3 * PAGE)
        );
        assert_eq!(
            map.reserve(PAGE, MappingKind::Anonymous, true, true),
            Ok(USER_ADDRESS_LIMIT - PAGE)
        );
        // 上限を越える 1 ページは配らない。
        assert_eq!(
            map.reserve(PAGE, MappingKind::Anonymous, true, true),
            Err(MapError::NoRoom)
        );
        // 以前の値（`1 << 47`）なら、最後のページ（上限より上）を配っていた。
        let mut old = MemoryMap::INACTIVE;
        old.activate(USER_ADDRESS_LIMIT, 1 << 47);
        assert_eq!(
            old.reserve(PAGE, MappingKind::Anonymous, true, true),
            Ok(USER_ADDRESS_LIMIT)
        );
    }
}
