//! 回ごとの置き場（案 B の ①。2026-09-29）。
//!
//! **1 回の QEMU の実行が読み書きするもの**——ESP・`disk0.img`・OVMF の変数の写し・シリアルのログ・`qemu-debug.log`・
//! 画面の写し・一時ファイル・monitor のソケット——**を、回ごとの置き場 `target/runs/<回の番号>/` に置く。**
//! 以前は `target/` の下の決まった名前に置いていたので、2 つの実行を同時に走らせると、互いの ESP やログを
//! 書き換えた。**置き場の名前を組み立てるのは、この型（[`RunDir`]）だけである**——基本の検査が、ほかの所に
//! 決まった名前が無いことを数える（`fixed_run_names`）。
//!
//! # 回の番号
//!
//! `target/runs/` の下にある番号の最大に 1 を足し、`mkdir` で取る。**`mkdir` は、取れるか取れないかが 1 度で
//! 決まる**ので、同時に走るほかのプロセスとは別の番号になる（取れなければ次の番号を試す）。
//!
//! # 使っている間はロックを持つ
//!
//! 置き場の `lock` を flock で持つ（プロセスが終われば、殺された場合も、カーネルが放す）。**古い置き場を
//! 片付けるときは、ロックが取れた置き場だけを触る**——走っている実行の置き場は消さない。
//!
//! # 残す量
//!
//! 新しい順に [`KEEP_WHOLE`] 個は丸ごと残す（以前の決まった名前と同じく、直前の実行の像とログを後から見られる）。
//! それより古いものは、ログ（`.log`）と説明（`run.txt`）だけを残し、像と一時ファイルを消す。[`KEEP_LOGS`] 個より
//! 古いものは丸ごと消す。**既定の像として示している置き場（[`RunDir::publish_as_default_image`] が示した置き場）は消さない。**
//! 1 回の像は約 12.7 MB（ESP 9.7 MB・`disk0.img` 2 MB・OVMF の変数 0.5 MB）、ログは約 0.5 MB である
//! （2026-09-29 に全検査の作業ツリーで量った）。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// 丸ごと残す置き場の数（新しい順）。
pub const KEEP_WHOLE: usize = 16;

/// ログだけでも残す置き場の数（新しい順）。これより古いものは丸ごと消す。
///
/// **2026-10-05 に、1,000 から 500 へ減らした**（SSD に置く量を減らすため）。全検査 1 回が取る置き場は約 430 個
/// なので、直近の全検査のログは、どれも後から読める。
pub const KEEP_LOGS: usize = 500;

/// 使い捨ての置き場（[`RunDir`] の doc の「使い捨ての置き場」）を置く tmpfs。
const SCRATCH_TMPFS: &str = "/dev/shm";

/// tmpfs の空きが、これより少なければ、その回の使い捨ての置き場は SSD に置く（バイト）。
///
/// **1 回の実行が tmpfs に置く量は、多くて約 150 MiB である**（装置の像 32 MiB、取り出した像 32 MiB、起動媒体の像
/// 66 MiB、ESP 約 10 MiB）。**同時に走る QEMU は 4 つまで**（`launch::VCPU_BUDGET`）で、項目が終わるまで残す分を
/// 入れても、全検査の間に使うのは 2 GiB に届かない見込みである。**1 GiB を切ったら、ほかの者が tmpfs を使って
/// いる**——メモリを食い合わないように、SSD へ戻す。
const SCRATCH_FLOOR_BYTES: u64 = 1 << 30;

/// 検査が tmpfs に置いてよい量の上限（バイト）。**置いてある量に 1 回の分を足して、これを越えるなら、その回は
/// SSD に置く**（2026-10-06）。tmpfs はメモリなので、片付けが追いつかない形になったときに、黙って食い続けない。
/// 数えるのは、利用者ごとの親（`/dev/shm/zeikos-runs-<利用者>`）の下の全部で、メインの木と全検査の木の両方が入る。
const SCRATCH_BUDGET_BYTES: u64 = 2 << 30;

/// 1 回の実行が tmpfs に置く量の見積もり（バイト。[`SCRATCH_FLOOR_BYTES`] の doc の内訳を、切り上げた値）。
const SCRATCH_PER_RUN_BYTES: u64 = 160 << 20;

/// `run.txt` に書く、使い捨ての置き場を SSD に置いた回の印（行の頭）。**数える側（[`fallbacks_since`]）が読む。**
const FALLBACK_MARK: &str = "scratch-fallback: ";

/// 番号を取り合って負けたときに、次の番号を試す回数の上限（上限の無い繰り返しを書かない）。
const ALLOCATION_ATTEMPTS: u64 = 1000;

/// 1 回の QEMU の実行の置き場。**作ってから落とすまで、置き場のロックを持つ。**
///
/// # 使い捨ての置き場（2026-10-05）
///
/// **装置の像・ESP・起動媒体の像・QEMU から取り出した RAM の像は、tmpfs に置く**（`/dev/shm` の下。[`RunDir::scratch`]）。
/// どれも、その回の間だけ要るファイルで、QEMU がいちばん多く書く先である（保存のたびに、装置の像の全体を書く）。
/// SSD に置いていたときは、全検査 1 回で 100 GiB ほどを書いていた（実測。`docs/verification-coverage.md` の
/// 「全検査が SSD に書く量」）。**ログ（シリアルと `-D` の記録）と `run.txt` は、今までどおり SSD の置き場に置く。**
///
/// **使い捨ての置き場は、値を落とすときに消す。** その前に、**後で要るものだけを SSD の置き場へ写す**（0 だけの
/// ブロックは書かない）。
///
/// - **項目の中で作った置き場**（全検査と、検査の項目）——項目が終わるまで残し（同じ項目の次の起動が、前の起動の
///   装置の像を読む）、**項目が失敗したときだけ**、装置の像と取り出した像を写す（[`finish_item`]）。
/// - **項目の外で作った置き場**（手で打つ起動）——落とすときに、装置の像と取り出した像を写す。次の起動が前の
///   装置の像を持ち越す流れ（`--keep-disk`・`--manual`）が、今までどおり動く。
/// - **既定の像として示した置き場**（[`RunDir::publish_as_default_image`]）——ESP も写す（道具が読む）。
///
/// **tmpfs が使えないか、空きが少ないか、検査が置いている量が上限（[`SCRATCH_BUDGET_BYTES`]）を越えそうなときは、
/// 使い捨ての置き場も SSD の置き場と同じ所になる**（今までの形）。**その回は、理由を `run.txt` と端末に出す。**
/// 全検査のまとめと見張りが、その回の数を出す（[`fallbacks_since`]）。
pub struct RunDir {
    number: u64,
    path: PathBuf,
    /// 使い捨ての置き場。**tmpfs に置けなかった回は、`path` と同じである。**
    scratch: PathBuf,
    /// 既定の像として示したか（落とすときに、ESP も写す）。
    published: std::cell::Cell<bool>,
    _lock: Option<File>,
}

thread_local! {
    /// いまの項目の中で落とした置き場の、使い捨ての置き場と SSD の置き場（[`begin_item`] から [`finish_item`] まで）。
    /// **項目の外では `None`。**
    static ITEM_SCRATCH: std::cell::RefCell<Option<Vec<(PathBuf, PathBuf, bool)>>> =
        const { std::cell::RefCell::new(None) };
}

/// 項目が始まった。**この糸で、これから落とす置き場の使い捨ての置き場を、項目の終わりまで残す。**
pub fn begin_item() {
    // 前の項目の分が残っていれば、通ったものとして片付ける（終わりを告げずに次が始まった場合の守り）。
    finish_item(false);
    ITEM_SCRATCH.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
}

/// 項目が終わった。**失敗していれば、装置の像と取り出した像を SSD の置き場へ写してから、使い捨ての置き場を消す。**
/// 通っていれば、写さずに消す。
pub fn finish_item(failed: bool) {
    let pending = ITEM_SCRATCH.with(|slot| slot.borrow_mut().take());
    for (scratch, path, published) in pending.unwrap_or_default() {
        retire_scratch(&scratch, &path, failed, published);
    }
}

/// 使い捨ての置き場を片付ける。`keep_images` なら、装置の像と取り出した像を SSD の置き場へ写す。`keep_esp` なら
/// ESP も写す。**写しは、0 だけのブロックを書かない。** 失敗は無視する（片付けは検査の結果に効かない）。
fn retire_scratch(scratch: &Path, path: &Path, keep_images: bool, keep_esp: bool) {
    if scratch == path {
        return;
    }
    if keep_images || keep_esp {
        for name in [DISK_IMAGE, EXTRACTED_IMAGE] {
            let from = scratch.join(name);
            if from.is_file() {
                let _ = copy_sparse(&from, &path.join(name));
            }
        }
    }
    if keep_esp {
        copy_tree(&scratch.join(ESP), &path.join(ESP));
    }
    let _ = fs::remove_dir_all(scratch);
}

/// ディレクトリを丸ごと写す（既定の像として示した置き場の ESP。小さいので、そのまま写す）。
fn copy_tree(from: &Path, to: &Path) {
    let Ok(entries) = fs::read_dir(from) else {
        return;
    };
    let _ = fs::create_dir_all(to);
    for entry in entries.filter_map(|entry| entry.ok()) {
        let target = to.join(entry.file_name());
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            copy_tree(&entry.path(), &target);
        } else {
            let _ = fs::copy(entry.path(), &target);
        }
    }
}

/// ファイルを、0 だけの 4 KiB のブロックを書かずに写す。**長さは変えない。**
fn copy_sparse(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};

    const BLOCK: usize = 4096;
    let bytes = fs::read(from)?;
    let mut file = File::create(to)?;
    file.set_len(bytes.len() as u64)?;
    for (index, block) in bytes.chunks(BLOCK).enumerate() {
        if block.iter().all(|byte| *byte == 0) {
            continue;
        }
        file.seek(SeekFrom::Start((index * BLOCK) as u64))?;
        file.write_all(block)?;
    }
    Ok(())
}

/// 置き場の中の、決まった名前。
const DISK_IMAGE: &str = "disk0.img";
const EXTRACTED_IMAGE: &str = "fs-extract.img";
const ESP: &str = "esp";

/// この回の使い捨ての置き場を決める（純粋な論理ではない。tmpfs の様子を見る）。**置けなければ、理由を返す。**
///
/// **道は `/dev/shm/zeikos-runs-<利用者>/<置き場の親の道から作った名前>/<番号>` である**——作業ツリーごと
/// （メインの木と、全検査の木）に分かれ、同じ番号でもぶつからない。
fn scratch_for(runs: &Path, number: u64) -> Result<PathBuf, String> {
    let tmpfs = Path::new(SCRATCH_TMPFS);
    let present = tmpfs.is_dir();
    let free = present
        .then(|| crate::launch::available_bytes(tmpfs))
        .flatten();
    let used = directory_blocks_bytes(&scratch_root());
    match scratch_refusal(present, free, used) {
        Some(reason) => Err(reason),
        None => Ok(scratch_base(runs).join(number.to_string())),
    }
}

/// tmpfs に置かない理由（純粋な論理）。**置いてよければ `None`。** `free` は tmpfs の空き、`used` は検査が
/// いま tmpfs に置いている量である。
fn scratch_refusal(present: bool, free: Option<u64>, used: u64) -> Option<String> {
    const MIB: u64 = 1 << 20;
    if !present {
        return Some(format!("{SCRATCH_TMPFS} is not there"));
    }
    let Some(free) = free else {
        return Some(format!(
            "the free space of {SCRATCH_TMPFS} could not be read"
        ));
    };
    if free < SCRATCH_FLOOR_BYTES {
        return Some(format!(
            "{SCRATCH_TMPFS} has {} MiB free, below the floor of {} MiB",
            free / MIB,
            SCRATCH_FLOOR_BYTES / MIB
        ));
    }
    if used.saturating_add(SCRATCH_PER_RUN_BYTES) > SCRATCH_BUDGET_BYTES {
        return Some(format!(
            "the checks already hold {} MiB on {SCRATCH_TMPFS}; one more run ({} MiB) would pass the budget of {} MiB",
            used / MIB,
            SCRATCH_PER_RUN_BYTES / MIB,
            SCRATCH_BUDGET_BYTES / MIB
        ));
    }
    None
}

/// ディレクトリの下のファイルが、実際に占めている量（バイト。ブロックの数から。穴は数えない）。無ければ 0。
fn directory_blocks_bytes(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_blocks_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.blocks() * 512),
            _ => 0,
        })
        .sum()
}

/// `run.txt` が、指定の時刻より後に始まり、使い捨ての置き場を SSD に置いた回のものか（純粋な論理）。
fn is_fallback_since(run_txt: &str, since_unix_ms: u128) -> bool {
    let started = run_txt.lines().find_map(|line| {
        line.strip_prefix("started (unix ms): ")?
            .trim()
            .parse::<u128>()
            .ok()
    });
    started.is_some_and(|started| started >= since_unix_ms)
        && run_txt.lines().any(|line| line.starts_with(FALLBACK_MARK))
}

/// 指定の時刻より後に始まった回のうち、使い捨ての置き場を SSD に置いた回の数（作業ツリーの置き場を数える）。
///
/// **`run.txt` から数える**——検査の項目が別のプロセスで走っても、後から呼ぶ見張りからでも、同じ数になる。
pub fn fallbacks_since(workspace_root: &Path, since_unix_ms: u128) -> usize {
    numbered_entries(&runs_dir(workspace_root))
        .into_iter()
        .filter(|(_, path)| {
            fs::read_to_string(path.join("run.txt"))
                .is_ok_and(|text| is_fallback_since(&text, since_unix_ms))
        })
        .count()
}

/// 検査が tmpfs に置くものの、利用者ごとの親。
fn scratch_root() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    Path::new(SCRATCH_TMPFS).join(format!("zeikos-runs-{user}"))
}

/// 置き場の親（`…/target/runs`）に対応する、tmpfs の側の親。
fn scratch_base(runs: &Path) -> PathBuf {
    scratch_root().join(scratch_name(runs))
}

/// 置き場の親の道から、tmpfs の側のディレクトリの名前を作る（純粋な論理）。英数字のほかは `_` にする。
fn scratch_name(runs: &Path) -> String {
    runs.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

impl RunDir {
    /// 新しい置き場を取る。`what` は人が読む名前（`run.txt` と、取ったときの 1 行に出す）。
    ///
    /// **取る前に、古い置き場を片付ける**（モジュールの doc の「残す量」）。
    pub fn create(workspace_root: &Path, what: &str) -> Result<RunDir> {
        let runs = runs_dir(workspace_root);
        fs::create_dir_all(&runs)
            .with_context(|| format!("failed to create {}", runs.display()))?;
        prune(&runs);
        let mut number = highest_number(&runs).map_or(1, |n| n + 1);
        for _ in 0..ALLOCATION_ATTEMPTS {
            let path = runs.join(number.to_string());
            match fs::create_dir(&path) {
                Ok(()) => {
                    let lock = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(path.join("lock"))
                        .with_context(|| {
                            format!("failed to create the lock in {}", path.display())
                        })?;
                    lock.lock()
                        .with_context(|| format!("failed to lock {}", path.display()))?;
                    let started = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    let mut note = File::create(path.join("run.txt")).with_context(|| {
                        format!("failed to write run.txt in {}", path.display())
                    })?;
                    writeln!(note, "what: {what}")?;
                    writeln!(note, "pid: {}", std::process::id())?;
                    writeln!(note, "started (unix ms): {started}")?;
                    // **使い捨ての置き場を取る**（tmpfs。取れなければ、SSD の置き場と同じ所にする）。
                    // **SSD に置く回は、黙って置かない**（2026-10-06）——理由を `run.txt` と端末に出す。
                    let placed = scratch_for(&runs, number).and_then(|scratch| {
                        let _ = fs::remove_dir_all(&scratch);
                        fs::create_dir_all(&scratch)
                            .map(|()| scratch)
                            .map_err(|error| format!("the directory could not be made ({error})"))
                    });
                    let scratch = match placed {
                        Ok(scratch) => scratch,
                        Err(reason) => {
                            writeln!(note, "{FALLBACK_MARK}{reason}")?;
                            println!(
                                "(warn) run {number}: scratch files go to the SSD this time: {reason}"
                            );
                            path.clone()
                        }
                    };
                    writeln!(note, "scratch: {}", scratch.display())?;
                    println!(
                        "(info) run {number}: {} ({what}{})",
                        path.display(),
                        if scratch == path {
                            "; scratch files on the same disk"
                        } else {
                            "; scratch files on tmpfs"
                        }
                    );
                    return Ok(RunDir {
                        number,
                        path,
                        scratch,
                        published: std::cell::Cell::new(false),
                        _lock: Some(lock),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()))
                }
            }
        }
        bail!(
            "could not take a run number under {} after {ALLOCATION_ATTEMPTS} attempt(s)",
            runs.display()
        )
    }

    /// 置き場だけを表す値（ロックも番号の取り合いも無い。**ホストのテストで引数の形を見るためだけに使う**）。
    #[cfg(test)]
    pub fn for_test(path: &Path, number: u64) -> RunDir {
        RunDir {
            number,
            path: path.to_path_buf(),
            scratch: path.to_path_buf(),
            published: std::cell::Cell::new(false),
            _lock: None,
        }
    }

    /// 置き場そのもの。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// ESP のディレクトリ（QEMU には `fat:rw:` で渡す）。**使い捨ての置き場に在る。**
    pub fn esp(&self) -> PathBuf {
        self.scratch.join(ESP)
    }

    /// virtio-blk のディスクのイメージ。**使い捨ての置き場に在る。**
    pub fn disk_image(&self) -> PathBuf {
        self.scratch.join(DISK_IMAGE)
    }

    /// QEMU から取り出した RAM の像。**使い捨ての置き場に在る。**
    pub fn extracted_image(&self) -> PathBuf {
        self.scratch.join(EXTRACTED_IMAGE)
    }

    /// その回だけの大きなファイル（起動媒体の像など）。**使い捨ての置き場に在り、SSD へは写さない。**
    pub fn scratch_file(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// OVMF の変数の写し（毎回テンプレートから作り直す）。
    pub fn ovmf_vars(&self) -> PathBuf {
        self.path.join("OVMF_VARS_4M.fd")
    }

    /// シリアルのログ。
    pub fn serial_log(&self) -> PathBuf {
        self.path.join("serial.log")
    }

    /// QEMU の `-d` の記録。
    pub fn debug_log(&self) -> PathBuf {
        self.path.join("qemu-debug.log")
    }

    /// そのほかのファイル（画面の写し・取り出したイメージなど）。**名前は置き場の中だけで意味を持つ。**
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// monitor のソケット。**名前に回の番号を入れる**——同じプロセスの中で並べても、ぶつからない。
    /// 道の長さの上限の確かめは、呼ぶ側が今までどおり行う（`ensure_socket_path_fits`）。
    pub fn monitor_socket(&self, kind: &str) -> PathBuf {
        PathBuf::from(format!(
            "/tmp/zeikos-xtask-{kind}-{}-{}.sock",
            std::process::id(),
            self.number
        ))
    }

    /// 道具（`tools/stack-deepest.py` など）が使う既定の像として、この置き場を示す（`target/runs/default-image`
    /// を、この置き場への印にする）。
    pub fn publish_as_default_image(&self) -> Result<()> {
        let Some(runs) = self.path.parent() else {
            bail!("the run directory {} has no parent", self.path.display());
        };
        let link = runs.join(DEFAULT_IMAGE);
        let staged = runs.join(format!("{DEFAULT_IMAGE}.{}", self.number));
        let _ = fs::remove_file(&staged);
        std::os::unix::fs::symlink(self.number.to_string(), &staged)
            .with_context(|| format!("failed to make {}", staged.display()))?;
        // **置き換えは rename で 1 度に行う**——読む側が、印の無い瞬間を見ない。
        fs::rename(&staged, &link)
            .with_context(|| format!("failed to replace {}", link.display()))?;
        // **落とすときに、ESP も SSD の置き場へ写す**（道具は、SSD の置き場を読む）。
        self.published.set(true);
        Ok(())
    }

    /// 直前の回のディスクのイメージ（`--keep-disk`・`--manual` で持ち越すもの）。**この置き場より前で、ディスクの
    /// イメージを持つ最も新しい置き場のもの**である。無ければ `None`。
    pub fn previous_disk_image(&self) -> Option<PathBuf> {
        let runs = self.path.parent()?;
        let mut numbers: Vec<u64> = numbered_entries(runs)
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| *n < self.number)
            .collect();
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        // **使い捨ての置き場に残っていれば、そちらを先に見る**（同じ項目の中の、前の起動）。無ければ、SSD の置き場
        // （落とすときに写したもの）を見る。
        let base = scratch_base(runs);
        numbers
            .into_iter()
            .flat_map(|n| {
                [
                    base.join(n.to_string()).join(DISK_IMAGE),
                    runs.join(n.to_string()).join(DISK_IMAGE),
                ]
            })
            .find(|path| path.is_file())
    }
}

impl Drop for RunDir {
    /// 使い捨ての置き場を片付ける（型の doc）。**項目の中なら、項目の終わりまで残す。**
    fn drop(&mut self) {
        if self.scratch == self.path {
            return;
        }
        // **既定の像として示した回の ESP と装置の像は、ここで写す**（項目の終わりを待たない）——同じ項目の中で、
        // すぐ後に走る道具（`tools/stack-deepest.py`）が、SSD の置き場の ELF と `disk0.img` を読む。項目の終わりまで
        // 待つと、道具が読む時点でまだ無い（全検査で 2 度落ちた。1 度目は ELF、2 度目は装置の像）。
        if self.published.get() {
            copy_tree(&self.scratch.join(ESP), &self.path.join(ESP));
            let image = self.scratch.join(DISK_IMAGE);
            if image.is_file() {
                let _ = copy_sparse(&image, &self.path.join(DISK_IMAGE));
            }
        }
        let deferred = ITEM_SCRATCH.with(|slot| match slot.borrow_mut().as_mut() {
            Some(pending) => {
                pending.push((self.scratch.clone(), self.path.clone(), false));
                true
            }
            None => false,
        });
        if !deferred {
            retire_scratch(&self.scratch, &self.path, true, false);
        }
    }
}

/// 既定の像の印の名前。
const DEFAULT_IMAGE: &str = "default-image";

/// 回の置き場の親。
fn runs_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target").join("runs")
}

/// 置き場の番号と道（数字の名前のディレクトリだけ）。
fn numbered_entries(runs: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = fs::read_dir(runs) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let number = entry.file_name().to_str()?.parse::<u64>().ok()?;
            Some((number, entry.path()))
        })
        .collect()
}

fn highest_number(runs: &Path) -> Option<u64> {
    numbered_entries(runs).into_iter().map(|(n, _)| n).max()
}

/// 片付けの分け方（純粋な論理）。新しい順に並べた番号から、丸ごと残すもの・ログだけ残すもの・消すものを返す。
fn prune_plan(mut numbers: Vec<u64>, keep: Option<u64>) -> (Vec<u64>, Vec<u64>) {
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    let mut logs_only = Vec::new();
    let mut remove = Vec::new();
    for (index, number) in numbers.into_iter().enumerate() {
        if Some(number) == keep {
            continue;
        }
        if index >= KEEP_LOGS {
            remove.push(number);
        } else if index >= KEEP_WHOLE {
            logs_only.push(number);
        }
    }
    (logs_only, remove)
}

/// 使い捨ての置き場（tmpfs）のうち、これより長く触られていないものは、置き去りと見なして消す。
///
/// **検査を途中で止めると、使い捨ての置き場が tmpfs に残る**（落とす処理が走らない）。tmpfs はメモリなので、
/// 残したままにしない。**全検査は 1 時間ほどで、上限は 195 分である。** 6 時間触られていない置き場を使っている者は
/// 居ない。
const SCRATCH_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// 置き去りの使い捨ての置き場を消す（[`SCRATCH_MAX_AGE`]）。失敗は無視する。
fn prune_scratch(runs: &Path) {
    let Ok(entries) = fs::read_dir(scratch_base(runs)) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > SCRATCH_MAX_AGE);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// 古い置き場を片付ける。**ロックの取れない置き場（走っている実行のもの）は触らない。** 失敗は無視する
/// （片付けは検査の結果に効かない）。
fn prune(runs: &Path) {
    prune_scratch(runs);
    let keep = fs::read_link(runs.join(DEFAULT_IMAGE))
        .ok()
        .and_then(|target| target.to_str()?.parse::<u64>().ok());
    let numbers: Vec<u64> = numbered_entries(runs).into_iter().map(|(n, _)| n).collect();
    let (logs_only, remove) = prune_plan(numbers, keep);
    for number in remove {
        let dir = runs.join(number.to_string());
        if let Some(_held) = lock_if_idle(&dir) {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    for number in logs_only {
        let dir = runs.join(number.to_string());
        let Some(_held) = lock_if_idle(&dir) else {
            continue;
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "lock" || name == "run.txt" || name.ends_with(".log") {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// 置き場のロックが取れれば取って返す（使っている置き場なら `None`）。
fn lock_if_idle(dir: &Path) -> Option<File> {
    let file = OpenOptions::new().write(true).open(dir.join("lock")).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tmpfs の側の名前は、置き場の親の道ごとに違う（メインの木と全検査の木が、同じ番号でぶつからない）。
    #[test]
    fn the_scratch_name_differs_per_runs_directory() {
        let main = scratch_name(Path::new("/home/u/zeikos/target/runs"));
        let full = scratch_name(Path::new("/home/u/zeikos-full-check/target/runs"));
        assert_ne!(main, full);
        assert!(main.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    /// tmpfs に置かない理由。**無い・空きが読めない・空きが下限を割る・置いてある量が上限を越えそう、の 4 つで
    /// 断り、それ以外は置く。** 上限は、1 回の分を足して、ちょうどまでは置く。
    #[test]
    fn scratch_goes_to_the_ssd_only_for_a_named_reason() {
        let roomy = Some(8 * SCRATCH_FLOOR_BYTES);
        assert_eq!(scratch_refusal(true, roomy, 0), None);
        assert!(scratch_refusal(false, roomy, 0)
            .unwrap()
            .contains("not there"));
        assert!(scratch_refusal(true, None, 0)
            .unwrap()
            .contains("could not be read"));
        assert!(scratch_refusal(true, Some(SCRATCH_FLOOR_BYTES - 1), 0)
            .unwrap()
            .contains("below the floor"));
        assert_eq!(scratch_refusal(true, Some(SCRATCH_FLOOR_BYTES), 0), None);
        let edge = SCRATCH_BUDGET_BYTES - SCRATCH_PER_RUN_BYTES;
        assert_eq!(scratch_refusal(true, roomy, edge), None);
        assert!(scratch_refusal(true, roomy, edge + 1)
            .unwrap()
            .contains("would pass the budget"));
        assert!(scratch_refusal(true, roomy, u64::MAX).is_some());
    }

    /// SSD に置いた回を数える側。**印の行が在り、指定の時刻より後に始まった回だけを数える。**
    #[test]
    fn a_fallback_is_counted_from_its_run_note() {
        let fallback = format!(
            "what: x\npid: 1\nstarted (unix ms): 2000\n{FALLBACK_MARK}no room\nscratch: /w/target/runs/3\n"
        );
        let on_tmpfs = "what: x\npid: 1\nstarted (unix ms): 2000\nscratch: /dev/shm/z/3\n";
        assert!(is_fallback_since(&fallback, 2000));
        assert!(is_fallback_since(&fallback, 1999));
        assert!(!is_fallback_since(&fallback, 2001));
        assert!(!is_fallback_since(on_tmpfs, 0));
        assert!(!is_fallback_since("what: x\n", 0));
    }

    /// 占めている量は、下のディレクトリのファイルまで足す。無いディレクトリは 0 である。
    #[test]
    fn the_held_amount_adds_up_the_files_below() {
        let root = std::env::temp_dir().join(format!("zeikos-held-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert_eq!(directory_blocks_bytes(&root), 0);
        fs::create_dir_all(root.join("a").join("7")).unwrap();
        fs::write(
            root.join("a").join("7").join("disk0.img"),
            vec![1u8; 64 * 1024],
        )
        .unwrap();
        fs::write(root.join("top"), vec![1u8; 4096]).unwrap();
        let held = directory_blocks_bytes(&root);
        assert!(held >= 64 * 1024 + 4096, "{held}");
        let _ = fs::remove_dir_all(&root);
    }

    /// 使い捨ての置き場を片付けるとき、**残すと決めた回だけ、装置の像と取り出した像を SSD の置き場へ写す。**
    /// 写しは長さと中身が同じである。どちらの場合も、使い捨ての置き場は消える。
    #[test]
    fn retiring_scratch_copies_the_images_only_when_asked() {
        let root = std::env::temp_dir().join(format!("zeikos-scratch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (keep, name) in [(true, "kept"), (false, "dropped")] {
            let scratch = root.join(name).join("scratch");
            let path = root.join(name).join("run");
            fs::create_dir_all(scratch.join(ESP)).unwrap();
            fs::create_dir_all(&path).unwrap();
            let mut image = vec![0u8; 3 * 4096 + 17];
            image[4096 + 5] = 7;
            fs::write(scratch.join(DISK_IMAGE), &image).unwrap();
            fs::write(scratch.join(EXTRACTED_IMAGE), b"x").unwrap();
            fs::write(scratch.join(ESP).join("kernel.elf"), b"elf").unwrap();
            retire_scratch(&scratch, &path, keep, false);
            assert!(!scratch.exists(), "{name}");
            assert_eq!(path.join(DISK_IMAGE).is_file(), keep, "{name}");
            assert_eq!(path.join(EXTRACTED_IMAGE).is_file(), keep, "{name}");
            assert!(
                !path.join(ESP).exists(),
                "{name}: the ESP is copied only for a published run"
            );
            if keep {
                assert_eq!(fs::read(path.join(DISK_IMAGE)).unwrap(), image);
            }
        }
        // 既定の像として示した回は、ESP も写す。
        let scratch = root.join("published").join("scratch");
        let path = root.join("published").join("run");
        fs::create_dir_all(scratch.join(ESP).join("zeikos")).unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(scratch.join(ESP).join("zeikos").join("kernel.elf"), b"elf").unwrap();
        retire_scratch(&scratch, &path, false, true);
        assert_eq!(
            fs::read(path.join(ESP).join("zeikos").join("kernel.elf")).unwrap(),
            b"elf"
        );
        // 使い捨ての置き場が SSD の置き場と同じ所なら、何も消さない。
        retire_scratch(&path, &path, false, false);
        assert!(path.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// **新しい順に KEEP_WHOLE 個は触らず、KEEP_LOGS 個までは像だけ消し、それより古いものは丸ごと消す。**
    /// **既定の像として示している置き場は、古くても触らない。**
    #[test]
    fn the_newest_runs_are_kept_whole_and_the_oldest_are_removed() {
        let numbers: Vec<u64> = (1..=(KEEP_LOGS as u64 + 5)).collect();
        let (logs_only, remove) = prune_plan(numbers, Some(2));
        assert_eq!(remove, vec![5, 4, 3, 1]);
        assert_eq!(logs_only.len(), KEEP_LOGS - KEEP_WHOLE);
        let newest_logs_only = KEEP_LOGS as u64 + 5 - KEEP_WHOLE as u64;
        assert_eq!(logs_only.first(), Some(&newest_logs_only));
        let (logs_only, remove) = prune_plan((1..=3).collect(), None);
        assert!(logs_only.is_empty() && remove.is_empty());
    }

    /// **置き場の中の名前と、ソケットの名前に回の番号が入ること。**
    #[test]
    fn the_names_in_a_run_directory_come_from_the_run() {
        let run = RunDir::for_test(Path::new("/w/target/runs/7"), 7);
        assert_eq!(run.esp(), Path::new("/w/target/runs/7/esp"));
        assert_eq!(run.disk_image(), Path::new("/w/target/runs/7/disk0.img"));
        assert_eq!(run.serial_log(), Path::new("/w/target/runs/7/serial.log"));
        assert!(run
            .monitor_socket("shell")
            .to_string_lossy()
            .ends_with(&format!("-{}-7.sock", std::process::id())));
    }
}
