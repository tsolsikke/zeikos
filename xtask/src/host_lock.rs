//! ホストのロック（2026-10-20）。**ZeikOS の QEMU と、同じ WSL で走る別のリポジトリ（Seinas）の重い作業を重ねない。**
//!
//! # 何を守るか
//!
//! **同じホストで重い作業（大きなビルドなど）が走ると、QEMU の時間が延び、ティックの位置と遅さの計測が揺れる。**
//! 起票済みの揺れる項目の 1 つは、落ちた回のホストの CPU 利用率が高かった。**検査のロック（`check_lock`）は
//! ZeikOS の中の並行だけを見る**（置き場は git の共通の置き場で、別のリポジトリからは見えない）。**こちらは
//! リポジトリをまたぐ約束である。** 約束の本体は `ADR-0089` にあり、Seinas の側はそれをコピーする。
//!
//! # 約束
//!
//! - **置き場は [`PATH`] に固定する**（同じ WSL のディストリの中なら、どのリポジトリからも同じ道である）。**無ければ
//!   作る。** `/tmp` は再起動で消えてよい——**誰も持っていない間に消えても、次に開いた者が作り直すだけである。**
//! - **ZeikOS は QEMU を使う間、共有で持つ**（`LOCK_SH`）。ZeikOS の中の並行の実行は互いに妨げない。
//! - **重い作業をする側は、排他で取れるまで待つ**（`LOCK_EX`。`flock(1)` で足りる）。
//! - **ZeikOS の新しい取得は、排他を待っている者が居れば先に譲る**——**Linux の flock は、排他が待っている間も新しい
//!   共有を通す**（実測。2026-10-20）。**譲らなければ、ZeikOS の実行が続く間、重い作業がいつまでも待つ。** 待っている者は
//!   `/proc/locks` の `->` の行で見える。**祖先がこのロックを持っているときは譲らずに取る**——譲ると、祖先が子の終わりを
//!   待ち、排他を待つ者が祖先を待ち、子が排他を待つ者を待つ、の輪になる。
//! - **待つ上限を持つ。** 普通の実行は [`WAIT_FOR_A_RUN`]、全検査は [`WAIT_FOR_THE_FULL_CHECK`]。**越えたら、検査のロックと
//!   同じ値（75）で断る**——**コミットの後の hook は 900 秒で切られる**ので、黙って待ち続けない。**待つ間は 1 分ごとに
//!   `host lock:` で始まる行を出す。**
//!
//! # 検査のロックとの関係
//!
//! **別のファイルである**——検査のロックはリポジトリの中の排他（全検査）を持ち、取れなければ待たずに断る。こちらは
//! リポジトリの外と分け合い、待つ。**取る順は、検査のロック → ホストのロック**（`check_lock::hold` が続けて取る）。
//! 検査のロックは誰も待たない（取れなければすぐ断る）ので、この順で輪はできない。重い作業の側は、ホストのロックしか取らない。
//!
//! # 持ち主が落ちたとき
//!
//! **flock はプロセスが終われば（SIGKILL を含む）カーネルが放す。** ファイルに中身は書かない（共有の持ち主が並ぶため）。
//! 持ち主は `/proc/locks` から読む。**ロックのファイルは開くたびに更新時刻を進める**——`/tmp` の掃除（30 日）が、持たれている
//! ファイルを消さないようにするため。消えると、次に開いた者が別のファイルを作り、排他が外れる。
//!
//! # 限界
//!
//! **ファイルを開いたのは xtask で、QEMU には渡さない**（Rust は `O_CLOEXEC` で開く）。xtask だけが止められて QEMU が残ると、
//! QEMU が走っている間もロックは放れている。検査のロックと同じ形である。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};

use crate::check_lock::{self, Mode, REFUSED_EXIT_CODE};
use crate::launch::HarnessFault;

/// ロックのファイル。**両方のリポジトリの約束である**——変えるときは Seinas の側も同時に変える。
pub const PATH: &str = "/tmp/zeikos-qemu.lock";

/// 普通の実行（`run`・`run-set` の子・`check --commit` など）が待つ上限。**コミットの後の hook の上限（900 秒）から、
/// `--commit` の所要（約 2 分）と余裕を引いた。**
pub const WAIT_FOR_A_RUN: Duration = Duration::from_secs(10 * 60);

/// 全検査が待つ上限。**全検査の親の上限（225 分）の内側に収める。**
pub const WAIT_FOR_THE_FULL_CHECK: Duration = Duration::from_secs(60 * 60);

/// 待つ間に行を出す間隔。
const NOTE_EVERY: Duration = Duration::from_secs(60);

/// 取り直す間隔。
const RETRY_EVERY: Duration = Duration::from_millis(500);

/// 持っているロック。**ファイルを開いたまま、プロセスが終わるまで持つ。**
static HELD: Mutex<Option<File>> = Mutex::new(None);

/// `/proc/locks` の 1 行の読み（`(待っているか, 排他か, pid, (装置の major, minor, inode))`）。
pub type LocksLine = (bool, bool, u32, (u32, u32, u64));

/// `/proc/locks` の 1 行（純粋な論理）。
///
/// 形は `7: FLOCK  ADVISORY  READ 2517134 08:30:1478185 0 EOF` と、待っている行の
/// `7: -> FLOCK  ADVISORY  WRITE 2517137 08:30:1478185 0 EOF`（実測。2026-10-20）。
pub fn parse_locks_line(line: &str) -> Option<LocksLine> {
    let mut fields = line.split_whitespace().peekable();
    fields.next()?;
    let waiting = fields.peek() == Some(&"->");
    if waiting {
        fields.next();
    }
    if fields.next()? != "FLOCK" {
        return None;
    }
    fields.next()?;
    let exclusive = match fields.next()? {
        "WRITE" => true,
        "READ" => false,
        _ => return None,
    };
    let pid = fields.next()?.parse().ok()?;
    let mut device = fields.next()?.split(':');
    let major = u32::from_str_radix(device.next()?, 16).ok()?;
    let minor = u32::from_str_radix(device.next()?, 16).ok()?;
    let inode = device.next()?.parse().ok()?;
    Some((waiting, exclusive, pid, (major, minor, inode)))
}

/// あるファイルの flock の様子（純粋な論理）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// 持っている pid と、排他か。
    pub holders: Vec<(u32, bool)>,
    /// 排他を待っている pid。
    pub waiting_exclusive: Vec<u32>,
}

/// `/proc/locks` の中身から、あるファイルの様子を拾う（純粋な論理）。
pub fn seen_in(locks: &str, file: (u32, u32, u64)) -> Seen {
    let mut seen = Seen::default();
    for (waiting, exclusive, pid, identity) in locks.lines().filter_map(parse_locks_line) {
        if identity != file {
            continue;
        }
        if waiting {
            if exclusive {
                seen.waiting_exclusive.push(pid);
            }
        } else {
            seen.holders.push((pid, exclusive));
        }
    }
    seen
}

/// 今すぐ取りに行ってよいか（純粋な論理）。**祖先が持っていれば取る。排他の持ち主か、排他を待つ者が居れば待つ。**
/// 待つなら、その理由になった pid と、排他で持っているのか（偽なら待っている）を返す。
pub fn must_wait(seen: &Seen, ancestors: &[u32]) -> Option<(u32, bool)> {
    if seen.holders.iter().any(|(pid, _)| ancestors.contains(pid)) {
        return None;
    }
    if let Some((pid, _)) = seen.holders.iter().find(|(_, exclusive)| *exclusive) {
        return Some((*pid, true));
    }
    seen.waiting_exclusive.first().map(|pid| (*pid, false))
}

fn identity(file: &File) -> Result<(u32, u32, u64)> {
    let meta = file
        .metadata()
        .with_context(|| format!("could not read the metadata of {PATH}"))?;
    let (major, minor) = check_lock::device_numbers(meta.dev());
    Ok((major, minor, meta.ino()))
}

/// `/proc/locks` を **1 回の `read` で**読む。**小さな `read` を重ねると、間で外れたロックの分だけ行がずれ、ほかの行が
/// 抜ける**（seq_file は続きを行の番号で探す。2026-10-20 に、4 本並んだ `run-set` の 1 本が、取った直後の自分の行を
/// 読み落とした）。1 回の `read` なら、カーネルが一覧を一度に書く。
fn read_locks() -> Result<String> {
    let mut file = File::open("/proc/locks").context("could not open /proc/locks")?;
    let mut buffer = vec![0u8; 1 << 20];
    let length = file
        .read(&mut buffer)
        .context("could not read /proc/locks")?;
    buffer.truncate(length);
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn seen(file: &File) -> Result<Seen> {
    Ok(seen_in(&read_locks()?, identity(file)?))
}

fn command_of(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .split('\0')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|_| "(gone)".to_string())
}

fn reason(pid: u32, holding: bool) -> String {
    if holding {
        format!("held exclusively by pid {pid} ({})", command_of(pid))
    } else {
        format!(
            "an exclusive request from pid {pid} ({}) is waiting, and ZeikOS lets it go first",
            command_of(pid)
        )
    }
}

/// 共有で取る。**取れれば `None`、上限まで待っても取れなければ断りの文を返す。** 既に持っていれば何もしない。
pub fn hold_shared(what: &str, limit: Duration) -> Result<Option<String>> {
    let mut held = HELD
        .lock()
        .map_err(|_| anyhow!("the host lock's state was poisoned"))?;
    if held.is_some() {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PATH)
        .with_context(|| format!("could not open the host lock {PATH}"))?;
    // **/tmp の掃除に持たれているファイルを消させない**（モジュールの doc の「持ち主が落ちたとき」）。
    let _ = file.set_modified(SystemTime::now());
    let ancestors = check_lock::ancestors();
    let started = Instant::now();
    let mut noted: Option<Instant> = None;
    loop {
        let wait = must_wait(&seen(&file)?, &ancestors);
        if wait.is_none() {
            match file.try_lock_shared() {
                Ok(()) => {
                    let me = std::process::id();
                    if !seen(&file)?.holders.contains(&(me, false)) {
                        return Err(anyhow::Error::new(HarnessFault(format!(
                            "the host lock {PATH} was taken but /proc/locks does not show it; the lock \
                             cannot tell the other side who holds it"
                        ))));
                    }
                    if noted.is_some() {
                        println!(
                            "host lock: took {PATH} for `{what}` after waiting {:.0} s",
                            started.elapsed().as_secs_f64()
                        );
                    }
                    *held = Some(file);
                    return Ok(None);
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(anyhow::Error::new(HarnessFault(format!(
                        "could not try the host lock {PATH}: {error}"
                    ))))
                }
            }
        }
        let why = match wait.or_else(|| {
            seen(&file)
                .ok()
                .and_then(|seen| must_wait(&seen, &ancestors))
        }) {
            Some((pid, holding)) => reason(pid, holding),
            None => "another process changed it a moment ago".to_string(),
        };
        let elapsed = started.elapsed();
        if elapsed >= limit {
            return Ok(Some(format!(
                "xtask: the host lock {PATH} was not free within {} min, so `{what}` was not run (exit \
                 {REFUSED_EXIT_CODE}).\n  last seen: {why}\n  Heavy work outside ZeikOS holds this lock \
                 exclusively; run this again after it ends.",
                limit.as_secs() / 60
            )));
        }
        if noted.is_none_or(|at| at.elapsed() >= NOTE_EVERY) {
            println!(
                "host lock: `{what}` waits for {PATH} ({:.0} of {} min so far): {why}",
                elapsed.as_secs_f64() / 60.0,
                limit.as_secs() / 60
            );
            noted = Some(Instant::now());
        }
        std::thread::sleep(RETRY_EVERY);
    }
}

/// 今の様子（`cargo xtask full --status` が出す）。**ロックは取らない。**
pub fn status_lines() -> Vec<String> {
    let Ok(file) = OpenOptions::new().read(true).open(PATH) else {
        return vec![format!("host lock: free ({PATH} does not exist yet)")];
    };
    let Ok(seen) = seen(&file) else {
        return vec![format!("host lock: could not read /proc/locks for {PATH}")];
    };
    let mut lines = Vec::new();
    if seen.holders.is_empty() {
        lines.push(format!("host lock: free ({PATH})"));
    }
    for (pid, exclusive) in &seen.holders {
        lines.push(format!(
            "host lock: held by pid {pid} ({}) {}",
            if *exclusive { "exclusive" } else { "shared" },
            command_of(*pid)
        ));
    }
    for pid in &seen.waiting_exclusive {
        lines.push(format!(
            "host lock: pid {pid} waits for it exclusively {}",
            command_of(*pid)
        ));
    }
    lines
}

/// 検査のロックの取り方から、待つ上限を選ぶ（純粋な論理）。
pub fn limit_for(mode: Mode) -> Duration {
    match mode {
        Mode::Exclusive => WAIT_FOR_THE_FULL_CHECK,
        Mode::Shared => WAIT_FOR_A_RUN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCKS: &str = "\
1: POSIX  ADVISORY  WRITE 900 00:1a:55 0 EOF
7: FLOCK  ADVISORY  READ 2517134 08:30:1478185 0 EOF
7: -> FLOCK  ADVISORY  WRITE 2517137 08:30:1478185 0 EOF
8: FLOCK  ADVISORY  WRITE 4000 08:30:99 0 EOF
";

    #[test]
    fn the_waiting_line_is_read_as_waiting() {
        assert_eq!(
            parse_locks_line("7: -> FLOCK  ADVISORY  WRITE 2517137 08:30:1478185 0 EOF"),
            Some((true, true, 2517137, (8, 0x30, 1478185)))
        );
        assert_eq!(
            parse_locks_line("7: FLOCK  ADVISORY  READ 2517134 08:30:1478185 0 EOF"),
            Some((false, false, 2517134, (8, 0x30, 1478185)))
        );
        assert_eq!(
            parse_locks_line("1: POSIX  ADVISORY  WRITE 900 00:1a:55 0 EOF"),
            None
        );
    }

    #[test]
    fn only_the_lines_of_the_file_count() {
        assert_eq!(
            seen_in(LOCKS, (8, 0x30, 1478185)),
            Seen {
                holders: vec![(2517134, false)],
                waiting_exclusive: vec![2517137],
            }
        );
        assert_eq!(
            seen_in(LOCKS, (8, 0x30, 99)),
            Seen {
                holders: vec![(4000, true)],
                waiting_exclusive: vec![],
            }
        );
    }

    #[test]
    fn a_new_shared_taker_lets_a_waiting_exclusive_go_first() {
        let seen = seen_in(LOCKS, (8, 0x30, 1478185));
        assert_eq!(must_wait(&seen, &[1]), Some((2517137, false)));
        assert_eq!(must_wait(&Seen::default(), &[1]), None);
    }

    #[test]
    fn an_exclusive_holder_makes_it_wait() {
        let seen = seen_in(LOCKS, (8, 0x30, 99));
        assert_eq!(must_wait(&seen, &[1]), Some((4000, true)));
    }

    #[test]
    fn a_child_of_a_holder_does_not_wait_for_the_waiting_exclusive() {
        // 祖先が共有で持っている子が譲ると、祖先・排他を待つ者・子の輪になる。
        let seen = seen_in(LOCKS, (8, 0x30, 1478185));
        assert_eq!(must_wait(&seen, &[77, 2517134, 1]), None);
    }

    #[test]
    fn the_full_check_waits_longer_than_a_run() {
        assert!(limit_for(Mode::Exclusive) > limit_for(Mode::Shared));
        // コミットの後の hook（900 秒）の内側に、`--commit` の所要と余裕を残す。
        assert!(limit_for(Mode::Shared) <= Duration::from_secs(900 - 240));
    }
}
