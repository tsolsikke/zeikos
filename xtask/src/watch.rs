//! 走っている全検査の進み具合を、1 回呼ぶごとに短くまとめて出す（2026-10-05。`cargo xtask full --watch`）。
//!
//! # なぜ在るのか
//!
//! **全検査は 55 分から 90 分かかる。** 途中で異常が出ても、終わるまで気づかないと、その回がまるごと無駄になる
//! （像を 32 MiB にした回は、10 項目の時間切れに、86 分たって終わってから気づいた）。**走っている間に、外から
//! 読むだけで様子を見られるようにする。**
//!
//! # 何を読むか
//!
//! **読むだけである。** 全検査の側には触らない（錠も取らない）。
//!
//! - **全検査のログ**（メインの木の `target/full-check/logs/<時刻>-<木>.log`）——項目の始まりの行（`=== xtask check:`）、
//!   判定の行（`--- …: OK` / `FAILED`）、所要の行（`(info) item time:`）。**並べて走らせる項目は、終わったときに
//!   塊で書かれる**（`crate::batch`）ので、ログから分かるのは「終わった項目」である。
//! - **項目の始まりの記録**（同じ名前の `-samples.tsv` の `item` の行。`crate::sampling`）——始まった時刻が載る。
//!   **始まっていて、まだ所要の行が無い項目が、いま走っている項目である。**
//! - **前回の緑の全検査のログ**——項目ごとの所要と、実時間。終わるまでの見込みと、「前回の 2 倍」の印に使う。
//! - **動いている QEMU**（`/proc` の命令行）と、その回の置き場の `run.txt`（`crate::launch` が、起動のたびに
//!   上限の秒数を書き足す）——上限の 8 割を越えた実行に印を付ける。
//! - **ディスク**——全検査の木とメインの `target/` の大きさ（`du`）、空き。
//!
//! # 状態を持つのは 2 つのファイルだけ
//!
//! - `target/full-check/watch-state.tsv`——前に呼んだときに、ログを何行目まで読んだか。**「前回呼んだ後に新しく出た
//!   失敗」を出すためである。**
//! - `target/full-check/watch-history.tsv`——呼ぶたびの、終わった数と、終わる見込みの時刻。**見込みがどれだけ
//!   当たったかを、後で比べるためである。**
//!
//! # 判断はしない
//!
//! **止めるかどうかは、読んだ者が決める**（`CLAUDE.md` の「全検査の見張り」）。この道具は、決まりに当たる形を
//! 見つけたら `advice:` の行で知らせるだけである。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// 「前回の 2 倍」の印を付けるときの、前回の所要の下限（秒）。**短い項目は、少しの揺れで 2 倍になる。**
const SLOW_FLOOR_SECONDS: f64 = 20.0;
/// 上限の何割を越えた QEMU に印を付けるか。
const LIMIT_FRACTION: f64 = 0.8;
/// 置き場の大きさの警告の印（`crate::BUILD_DIR_WARN_BYTES` と同じ 100 GiB）の、何割で「近い」とするか。
const SIZE_NEAR_FRACTION: f64 = 0.9;
/// 同じ形の失敗が、終わった項目の末尾でこの数だけ続いたら、止める形として知らせる。
const FAILURES_IN_A_ROW: usize = 3;
/// いま走っている項目のうち、名前を出す数の上限。
const MAX_RUNNING_SHOWN: usize = 4;
/// 1 回の呼び出しで出す、新しい異常の行の上限。
const MAX_NEW_LINES: usize = 6;

/// ログから読んだ、終わった項目 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub name: String,
    pub seconds: f64,
    pub failed: bool,
    /// 項目の塊の中の、目に留めるべき行（失敗の判定、時間切れ、`[ERROR]`、panic、ビルドの失敗）。
    pub notable: Vec<String>,
    /// 所要の行が在った行番号（0 始まり）。**「前回呼んだ後」を決めるのに使う。**
    pub line: usize,
}

/// ログ 1 本から読んだもの（純粋な論理。[`parse_log`]）。
#[derive(Debug, Default, PartialEq)]
pub struct LogView {
    pub finished: Vec<Finished>,
    /// 全検査が終わっていれば、その結果の行（`xtask check: all …` か `Error: xtask check: …`）。
    ///
    /// **通った行だけでは、終わったと言えない。** 全検査は、最初に基本の検査を 1 度走らせ、その終わりにも
    /// `xtask check: all 62 check(s) passed` が出る。**全検査の終わりの結果の行は、まとめの `(info) wall clock:` の
    /// 行より後に出る**（通った回にも、落ちた回にも）。**落ちた行（`Error: xtask check:`）は、どこに出ても終わりである**
    /// ——最初の基本の検査が落ちると、全検査はそこで終わる。
    pub ended: Option<String>,
    /// 実時間（分。`(info) wall clock:` の行）。
    pub wall_minutes: Option<f64>,
    /// ビルドが失敗した行が在るか（`could not compile`）。
    pub build_failed: bool,
    pub lines: usize,
}

/// 行が、目に留めるべき種類か。
fn is_notable(line: &str) -> bool {
    (line.starts_with("--- ") && line.contains(": FAILED"))
        || line.contains("[ERROR]")
        || line.contains("panicked")
        || line.contains("reached its failure deadline")
        || line.contains("ended at the time limit with no declaration")
        || line.contains("harness fault")
        || line.contains("could not compile")
}

/// 全検査のログを読む（純粋な論理）。
///
/// **項目の塊は、`=== xtask check: <名前>` から、同じ名前の `(info) item time:` までである。** 塊の中に
/// `--- …: FAILED` の行が 1 つでも在れば、その項目は失敗である。
pub fn parse_log(text: &str) -> LogView {
    let mut view = LogView::default();
    let mut failed = false;
    let mut notable: Vec<String> = Vec::new();
    // まとめの行（`(info) wall clock:`）を見たか。**見る前の「通った」の行は、最初の基本の検査のものである**
    // （`ended` の doc）。
    let mut summary_seen = false;
    for (index, line) in text.lines().enumerate() {
        view.lines = index + 1;
        if line.starts_with("=== xtask check: ") {
            failed = false;
            notable.clear();
            continue;
        }
        if line.contains("could not compile") {
            view.build_failed = true;
        }
        if let Some(rest) = line.strip_prefix("(info) item time: ") {
            // `12.3s [family] for <名前>`
            let seconds = rest
                .split('s')
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok());
            let name = rest.split_once("] for ").map(|(_, name)| name.to_string());
            if let (Some(seconds), Some(name)) = (seconds, name) {
                view.finished.push(Finished {
                    name,
                    seconds,
                    failed,
                    notable: std::mem::take(&mut notable),
                    line: index,
                });
            }
            failed = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("(info) wall clock: ") {
            view.wall_minutes = rest
                .split(" min")
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok());
            summary_seen = true;
        }
        if line.starts_with("Error: xtask check:")
            || (summary_seen && line.starts_with("xtask check: all "))
        {
            view.ended = Some(line.to_string());
        }
        if is_notable(line) {
            if line.starts_with("--- ") && line.contains(": FAILED") {
                failed = true;
            }
            notable.push(line.to_string());
        }
    }
    view
}

/// 項目の始まりの記録（`-samples.tsv` の `item` の行）を読む。**返すのは、時刻（ミリ秒）と名前である。**
pub fn parse_item_starts(text: &str) -> Vec<(u128, String)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let at = fields.next()?.parse::<u128>().ok()?;
            if fields.next()? != "item" {
                return None;
            }
            Some((at, fields.next()?.to_string()))
        })
        .collect()
}

/// 始まっていて、まだ終わっていない項目（純粋な論理）。**同じ名前が 2 度走る**（基本の検査は、全検査の中で
/// もう 1 度走る）ので、名前ごとに、終わった数だけ先頭から消す。
pub fn running_items(starts: &[(u128, String)], finished: &[Finished]) -> Vec<(u128, String)> {
    let mut done: HashMap<&str, usize> = HashMap::new();
    for item in finished {
        *done.entry(item.name.as_str()).or_default() += 1;
    }
    let mut running = Vec::new();
    for (at, name) in starts {
        match done.get_mut(name.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => running.push((*at, name.clone())),
        }
    }
    running
}

/// 前回の全検査から、項目ごとの所要を名前で引けるようにする（同じ名前は、出た順に並べる）。
fn times_by_name(finished: &[Finished]) -> HashMap<&str, Vec<f64>> {
    let mut times: HashMap<&str, Vec<f64>> = HashMap::new();
    for item in finished {
        times
            .entry(item.name.as_str())
            .or_default()
            .push(item.seconds);
    }
    times
}

/// 終わるまでの見込み（秒。純粋な論理）。
///
/// **前回の全検査の、まだ終わっていない項目の所要を足し、前回の「実時間 ÷ 項目の所要の合計」を掛ける**
/// （項目は並べて走るので、実時間は合計より短い）。前回に無い項目は数えない。前回の記録が無ければ `None`。
pub fn remaining_seconds(previous: &LogView, finished: &[Finished]) -> Option<f64> {
    let total: f64 = previous.finished.iter().map(|item| item.seconds).sum();
    if previous.finished.is_empty() || total <= 0.0 {
        return None;
    }
    let ratio = previous
        .wall_minutes
        .map_or(1.0, |minutes| (minutes * 60.0 / total).min(1.0));
    let mut left = times_by_name(&previous.finished);
    for item in finished {
        if let Some(times) = left.get_mut(item.name.as_str()) {
            if !times.is_empty() {
                times.remove(0);
            }
        }
    }
    let remaining: f64 = left.values().flatten().sum();
    Some(remaining * ratio)
}

/// いま走っている項目が、前回の 2 倍を越えているか（純粋な論理）。**前回の所要が短い項目には印を付けない。**
pub fn is_slow(elapsed_seconds: f64, previous_seconds: Option<f64>) -> bool {
    match previous_seconds {
        Some(previous) => elapsed_seconds > 2.0 * previous.max(SLOW_FLOOR_SECONDS),
        None => false,
    }
}

/// 終わった項目の末尾で、失敗が続いている数（純粋な論理）。
pub fn failures_at_the_tail(finished: &[Finished]) -> usize {
    finished.iter().rev().take_while(|item| item.failed).count()
}

/// `run.txt` に書き足した、QEMU の起動の行（`crate::launch` の `note_launch`）を読む。**最後の行を返す。**
/// 形は `qemu: limit <秒>s started (unix ms): <時刻>`。
pub fn last_launch(run_txt: &str) -> Option<(f64, u128)> {
    run_txt.lines().rev().find_map(|line| {
        let rest = line.strip_prefix("qemu: limit ")?;
        let (limit, started) = rest.split_once("s started (unix ms): ")?;
        Some((limit.trim().parse().ok()?, started.trim().parse().ok()?))
    })
}

/// `run.txt` の `what:` の行。
fn run_what(run_txt: &str) -> String {
    run_txt
        .lines()
        .find_map(|line| line.strip_prefix("what: "))
        .unwrap_or("?")
        .to_string()
}

/// 動いている QEMU の、回の置き場（`…/target/runs/<番号>`）。`/proc` の命令行から拾う。
fn running_qemu_run_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return dirs;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let line = String::from_utf8_lossy(&raw).replace('\0', " ");
        if !line.contains("qemu-system") {
            continue;
        }
        // 命令行の中の `…/runs/<番号>/…` を 1 つ拾う。
        let dir = line.split([' ', ',', '=', ':']).find_map(|token| {
            let at = token.find("/runs/")?;
            let after = &token[at + "/runs/".len()..];
            let number: String = after.chars().take_while(char::is_ascii_digit).collect();
            if number.is_empty() {
                return None;
            }
            Some(PathBuf::from(format!("{}/runs/{number}", &token[..at])))
        });
        dirs.push(dir.unwrap_or_default());
    }
    dirs
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

fn minutes(seconds: f64) -> String {
    format!("{:.1} min", seconds / 60.0)
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

/// 壁の時計の時刻（`HH:MM`）。`date` に訊く（読めなければ空）。
fn clock(unix_seconds: u64) -> String {
    std::process::Command::new("date")
        .arg("-d")
        .arg(format!("@{unix_seconds}"))
        .arg("+%H:%M")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

/// ログの置き場の中の、`.log` を新しい順に並べる（名前が時刻で始まるので、名前の順が時刻の順である）。
fn logs_newest_first(logs: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(logs)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.reverse();
    found
}

/// `cargo xtask full --watch`。
pub fn command(root: &Path) -> Result<()> {
    let main = crate::check_lock::main_tree(root)?;
    let state_dir = main.join("target").join("full-check");
    let logs = logs_newest_first(&state_dir.join("logs"));
    let Some(current_path) = logs.first() else {
        println!("watch: no full check log under {}", state_dir.display());
        return Ok(());
    };
    let current_text = fs::read_to_string(current_path)
        .or_else(|_| fs::read(current_path).map(|raw| String::from_utf8_lossy(&raw).into_owned()))
        .with_context(|| format!("failed to read {}", current_path.display()))?;
    let current = parse_log(&current_text);

    // 前回の緑の全検査（いまのログより古いもののうち、最初に見つかる緑）。
    let previous = logs.iter().skip(1).find_map(|path| {
        let text = fs::read(path)
            .map(|raw| String::from_utf8_lossy(&raw).into_owned())
            .ok()?;
        let view = parse_log(&text);
        view.ended
            .as_deref()
            .is_some_and(|line| line.starts_with("xtask check: all "))
            .then_some(view)
    });

    let stem = current_path.with_extension("");
    let samples_path = PathBuf::from(format!("{}-samples.tsv", stem.display()));
    let samples_text = fs::read(&samples_path)
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .unwrap_or_default();
    let starts = parse_item_starts(&samples_text);
    let now = now_ms();
    let began = samples_text
        .lines()
        .filter_map(|line| line.split('\t').next()?.parse::<u128>().ok())
        .next();
    // **終わった全検査では、最後の記録までを経過とする**（いまの時刻から数えると、終わった後も伸び続ける）。
    let last_record = samples_text
        .lines()
        .rev()
        .find_map(|line| line.split('\t').next()?.parse::<u128>().ok());
    let until = match (&current.ended, last_record) {
        (Some(_), Some(last)) => last,
        _ => now,
    };
    let elapsed = began.map(|began| until.saturating_sub(began) as f64 / 1000.0);

    // --- 1 行目: 数と時間 ---
    let failed: Vec<&Finished> = current.finished.iter().filter(|item| item.failed).collect();
    let total = previous.as_ref().map(|view| view.finished.len());
    let remaining = previous
        .as_ref()
        .and_then(|previous| remaining_seconds(previous, &current.finished));
    let log_name = current_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut first = format!(
        "watch: {} of {} item(s) finished (pass {}, fail {})",
        current.finished.len(),
        total.map_or("?".to_string(), |total| format!("about {total}")),
        current.finished.len() - failed.len(),
        failed.len()
    );
    if let Some(elapsed) = elapsed {
        first.push_str(&format!(", {} elapsed", minutes(elapsed)));
    }
    let mut eta_unix = 0u64;
    match (&current.ended, remaining) {
        (Some(ended), _) => first.push_str(&format!("; ENDED: {ended}")),
        (None, Some(remaining)) => {
            eta_unix = (now / 1000) as u64 + remaining as u64;
            first.push_str(&format!(
                ", about {} left (ends around {})",
                minutes(remaining),
                clock(eta_unix)
            ));
        }
        (None, None) => first.push_str(", no earlier green run to estimate from"),
    }
    println!("{first} [{log_name}]");

    // --- 2 行目: いま走っている項目 ---
    let previous_times = previous
        .as_ref()
        .map(|view| times_by_name(&view.finished))
        .unwrap_or_default();
    if current.ended.is_none() {
        let running = running_items(&starts, &current.finished);
        // **印の付いた項目を先に、あとは長く走っている順に、[`MAX_RUNNING_SHOWN`] 個まで出す。**
        let mut shown: Vec<(bool, f64, String)> = running
            .iter()
            .map(|(at, name)| {
                let seconds = now.saturating_sub(*at) as f64 / 1000.0;
                let before = previous_times
                    .get(name.as_str())
                    .and_then(|times| times.last().copied());
                let slow = is_slow(seconds, before);
                (
                    slow,
                    seconds,
                    format!(
                        "{name} ({seconds:.0}s; last time {}){}",
                        before.map_or("-".to_string(), |before| format!("{before:.0}s")),
                        if slow { " !SLOW" } else { "" }
                    ),
                )
            })
            .collect();
        shown.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
        let slow_count = shown.iter().filter(|item| item.0).count();
        let parts: Vec<String> = shown
            .iter()
            .take(MAX_RUNNING_SHOWN)
            .map(|item| item.2.clone())
            .collect();
        println!(
            "watch: running {} ({slow_count} over twice their last time): {}",
            running.len(),
            if parts.is_empty() {
                "nothing has started and not finished".to_string()
            } else {
                parts.join(" | ")
            }
        );
    }

    // --- 3 行目: 動いている QEMU と、上限に近い実行 ---
    let qemu_dirs = running_qemu_run_dirs();
    let mut near = Vec::new();
    for dir in &qemu_dirs {
        let Ok(text) = fs::read_to_string(dir.join("run.txt")) else {
            continue;
        };
        if let Some((limit, started)) = last_launch(&text) {
            let seconds = now.saturating_sub(started) as f64 / 1000.0;
            if limit > 0.0 && seconds >= LIMIT_FRACTION * limit {
                near.push(format!(
                    "{} ({seconds:.0}s of its {limit:.0}s limit) !NEAR-LIMIT",
                    run_what(&text)
                ));
            }
        }
    }
    println!(
        "watch: QEMU running {}{}",
        qemu_dirs.len(),
        if near.is_empty() {
            String::new()
        } else {
            format!("; {}", near.join(" | "))
        }
    );

    // --- 4 行目から: 前回呼んだ後に新しく出た異常 ---
    let state_path = state_dir.join("watch-state.tsv");
    let seen = fs::read_to_string(&state_path)
        .ok()
        .and_then(|text| {
            let (name, lines) = text.trim().split_once('\t')?;
            (name == log_name).then(|| lines.parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut new_lines = Vec::new();
    for item in current.finished.iter().filter(|item| item.line >= seen) {
        // **失敗した項目の行だけを出す。** 破壊テストは、通る項目の中でも `[ERROR]` や panic を出す（狙いどおりである）。
        if item.failed {
            for line in &item.notable {
                new_lines.push(format!("{}: {}", item.name, line));
            }
        } else {
            for line in item
                .notable
                .iter()
                .filter(|line| line.contains("ended at the time limit with no declaration"))
            {
                new_lines.push(format!("{} (passed): {}", item.name, line));
            }
        }
    }
    if new_lines.is_empty() {
        println!("watch: nothing new failed since the last call");
    } else {
        println!(
            "watch: NEW since the last call: {} line(s) from failed or timed-out item(s)",
            new_lines.len()
        );
        for line in new_lines.iter().take(MAX_NEW_LINES) {
            let short: String = line.chars().take(300).collect();
            println!("watch:   {short}");
        }
        if new_lines.len() > MAX_NEW_LINES {
            println!(
                "watch:   … and {} more (read {})",
                new_lines.len() - MAX_NEW_LINES,
                current_path.display()
            );
        }
    }
    let _ = fs::write(&state_path, format!("{log_name}\t{}\n", current.lines));

    // --- ディスク ---
    let worktree = crate::full_check::worktree_path(&main);
    let worktree_bytes = crate::directory_bytes(&worktree);
    let main_bytes = crate::directory_bytes(&main.join("target"));
    let free = crate::launch::available_bytes(&main);
    let near_mark = |bytes: Option<u64>| {
        if bytes.is_some_and(|bytes| {
            bytes as f64 > SIZE_NEAR_FRACTION * crate::BUILD_DIR_WARN_BYTES as f64
        }) {
            " !NEAR-100GiB"
        } else {
            ""
        }
    };
    let show =
        |bytes: Option<u64>| bytes.map_or("?".to_string(), |bytes| format!("{:.1}", gib(bytes)));
    let low_disk = free.is_some_and(|free| free < 2 * crate::launch::DISK_FLOOR_BYTES);
    // **全検査が始まってから装置へ書いた量**（`/proc/diskstats` の差。始まりの値は、全検査の入口が控える）と、
    // **VHD の載ったドライブの空き**（入口の空きの確かめが見るのと同じ値。WSL の中の空きとは別である）。
    let written = fs::read_to_string(crate::full_check::current_run_path(&main))
        .ok()
        .and_then(|text| {
            let (name, start) = text.trim().split_once('\t')?;
            if name != log_name {
                return None;
            }
            let start = start.parse::<u64>().ok()?;
            let now = crate::full_check::sectors_written(&main)?;
            Some(now.saturating_sub(start) * 512)
        });
    let (_, host_free, _) = crate::full_check::free_spaces(&main);
    // **使い捨てのファイルを SSD に置いた回の数**（全検査の木の置き場の `run.txt` から。全検査が始まった後の回だけ）。
    let fallbacks = began.map(|began| crate::run_dir::fallbacks_since(&worktree, began));
    println!(
        "watch: disk: written since the start {} GiB; scratch on the SSD in {} run(s){}; full-check tree {} GiB{}, \
         main target {} GiB{}; free on the drive holding the WSL disk {} GiB, inside WSL {} GiB{}",
        show(written),
        fallbacks.map_or("?".to_string(), |count| count.to_string()),
        if fallbacks.is_some_and(|count| count > 0) {
            " !SSD-SCRATCH"
        } else {
            ""
        },
        show(worktree_bytes),
        near_mark(worktree_bytes),
        show(main_bytes),
        near_mark(main_bytes),
        show(host_free),
        show(free),
        if low_disk { " !LOW" } else { "" }
    );

    // --- 決まりに当たる形 ---
    let tail = failures_at_the_tail(&current.finished);
    if current.ended.is_none() {
        if current.build_failed {
            println!(
                "advice: STOP — a build failed (could not compile); later items cannot be trusted"
            );
        } else if tail >= FAILURES_IN_A_ROW {
            println!(
                "advice: STOP — the last {tail} finished item(s) all failed; check whether they fail the same way"
            );
        } else if low_disk {
            println!("advice: STOP — the disk is close to the floor QEMU needs");
        } else if !failed.is_empty() {
            println!(
                "advice: keep running and investigate by reading only — {} item(s) failed so far",
                failed.len()
            );
        }
    }

    // --- 見込みの記録 ---
    let history = state_dir.join("watch-history.tsv");
    let mut text = fs::read_to_string(&history).unwrap_or_else(|_| {
        "# unix\tlog\tfinished\tfailed\teta_unix (0 = none or ended)\n".to_string()
    });
    text.push_str(&format!(
        "{}\t{log_name}\t{}\t{}\t{}\n",
        now / 1000,
        current.finished.len(),
        failed.len(),
        if current.ended.is_some() { 0 } else { eta_unix }
    ));
    let _ = fs::write(&history, text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
=== xtask check: build kernel (none)
--- build kernel (none): OK
(info) item time: 0.4s [base] for build kernel (none)
=== xtask check: acpi-test bad-checksum
acpi-test bad-checksum: (info) the run reached its failure deadline (20.1s)
--- acpi-test bad-checksum: FAILED [timeout] (acpi-test bad-checksum: FAIL)
(info) item wait: 1 run(s) ended at the time limit with no declaration that this is normal, for acpi-test bad-checksum
(info) item time: 20.3s [interrupts] for acpi-test bad-checksum
=== xtask check: panic-test
panic-test: serial contains \"[ERROR] panic\" = OK
--- panic-test: OK
(info) item time: 12.5s [boot] for panic-test
=== xtask check: build kernel (none)
--- build kernel (none): OK
(info) item time: 0.1s [base] for build kernel (none)
(info) wall clock: 0.5 min (the limit is 195 min)
xtask check: all 4 check(s) passed
";

    /// 終わった項目と、所要と、失敗と、終わりの行を読む。**通った項目の中の `[ERROR]` は、失敗にしない。**
    #[test]
    fn the_log_gives_finished_items_with_their_verdicts() {
        let view = parse_log(LOG);
        let names: Vec<&str> = view
            .finished
            .iter()
            .map(|item| item.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "build kernel (none)",
                "acpi-test bad-checksum",
                "panic-test",
                "build kernel (none)"
            ]
        );
        assert_eq!(view.finished[1].seconds, 20.3);
        assert!(view.finished[1].failed);
        assert_eq!(view.finished[1].notable.len(), 3);
        assert!(!view.finished[2].failed);
        assert_eq!(view.finished[2].notable.len(), 1);
        assert!(
            !view.finished[3].failed,
            "a failure does not leak into the next item"
        );
        assert_eq!(view.wall_minutes, Some(0.5));
        assert_eq!(
            view.ended.as_deref(),
            Some("xtask check: all 4 check(s) passed")
        );
        assert!(!view.build_failed);
    }

    /// **最初の基本の検査の「通った」の行は、全検査の終わりではない。** まとめの行の後の結果の行が、終わりである。
    /// **落ちた行は、どこに出ても終わりである。**
    #[test]
    fn the_base_checks_own_end_line_is_not_the_end_of_the_full_check() {
        let text = "(info) item time: 0.4s [base] for build kernel (none)\n\
                    xtask check: all 62 check(s) passed\n\
                    === xtask check: panic-test\n";
        assert_eq!(parse_log(text).ended, None);
        let failed = "xtask check: all 62 check(s) passed\n\
                      (info) wall clock: 86.1 min (the limit is 195 min)\n\
                      Error: xtask check: 10 of 479 check(s) failed: a, b\n";
        assert_eq!(
            parse_log(failed).ended.as_deref(),
            Some("Error: xtask check: 10 of 479 check(s) failed: a, b")
        );
        let base_failed = "Error: xtask check: 1 of 62 check(s) failed: fmt --check\n";
        assert!(parse_log(base_failed).ended.is_some());
    }

    /// 走っている途中のログは、終わりの行を持たない。ビルドの失敗は、行から分かる。
    #[test]
    fn an_unfinished_log_has_no_end_and_a_build_failure_is_seen() {
        let text = "=== xtask check: build kernel (none)\nerror: could not compile `kernel`\n";
        let view = parse_log(text);
        assert_eq!(view.ended, None);
        assert!(view.build_failed);
        assert!(view.finished.is_empty());
    }

    /// 始まっていて終わっていない項目だけが残る。**同じ名前が 2 度走るときは、終わった数だけ消す。**
    #[test]
    fn running_items_are_the_started_ones_without_a_time_line() {
        let starts = parse_item_starts(
            "1000\tsample\t\t0.9\n1000\titem\tbuild kernel (none)\n2000\titem\tpanic-test\n\
             3000\titem\tbuild kernel (none)\n4000\titem\tzi-test a\n",
        );
        assert_eq!(starts.len(), 4);
        let view = parse_log(
            "(info) item time: 0.4s [base] for build kernel (none)\n\
             (info) item time: 12.5s [boot] for panic-test\n",
        );
        let running = running_items(&starts, &view.finished);
        assert_eq!(
            running,
            [
                (3000, "build kernel (none)".to_string()),
                (4000, "zi-test a".to_string())
            ]
        );
    }

    /// 見込みは、前回の「まだ終わっていない項目」の所要に、前回の実時間の割合を掛けたものである。
    #[test]
    fn the_estimate_scales_what_is_left_by_last_runs_wall_clock() {
        let previous = parse_log(LOG);
        // 前回: 合計 33.3 秒、実時間 30 秒 → 割合 0.9009…
        let nothing: Vec<Finished> = Vec::new();
        let all = remaining_seconds(&previous, &nothing).unwrap();
        assert!((all - 30.0).abs() < 0.01, "{all}");
        let now = parse_log(
            "(info) item time: 0.5s [base] for build kernel (none)\n\
             (info) item time: 25.0s [interrupts] for acpi-test bad-checksum\n",
        );
        // 残り: panic-test 12.5 + 2 度目の build 0.1 = 12.6 秒 × 割合。
        let left = remaining_seconds(&previous, &now.finished).unwrap();
        assert!((left - 12.6 * 30.0 / 33.3).abs() < 0.01, "{left}");
        assert_eq!(remaining_seconds(&LogView::default(), &nothing), None);
    }

    /// 「前回の 2 倍」の印。**短い項目には付けない**（下限の 2 倍までは付かない）。
    #[test]
    fn slow_means_twice_the_last_time_with_a_floor() {
        assert!(is_slow(101.0, Some(50.0)));
        assert!(!is_slow(99.0, Some(50.0)));
        assert!(!is_slow(30.0, Some(2.0)));
        assert!(is_slow(41.0, Some(2.0)));
        assert!(!is_slow(1000.0, None));
    }

    /// 末尾で続いている失敗の数。
    #[test]
    fn failures_in_a_row_are_counted_from_the_end() {
        let view = parse_log(LOG);
        assert_eq!(failures_at_the_tail(&view.finished), 0);
        assert_eq!(failures_at_the_tail(&view.finished[..2]), 1);
    }

    /// `run.txt` の、QEMU の起動の行。**最後の行を読む**（1 つの回が、QEMU を何度か起こすことがある）。
    #[test]
    fn the_last_launch_line_gives_the_limit_and_the_start() {
        let text =
            "what: zi-test\npid: 1\nstarted (unix ms): 5\nqemu: limit 60s started (unix ms): 100\n\
                    qemu: limit 90.5s started (unix ms): 200\n";
        assert_eq!(last_launch(text), Some((90.5, 200)));
        assert_eq!(last_launch("what: x\n"), None);
        assert_eq!(run_what(text), "zi-test");
    }
}
