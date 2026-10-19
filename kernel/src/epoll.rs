//! `epoll`（2026-10-19。`ADR-0088`）——複数の fd のどれかの準備ができるまで待つ。Seinas のイベントループ（calloop と
//! polling）と wayland-server が使う。
//!
//! # 項目が指すもの
//!
//! **項目は「オブジェクト」（[`Source`]。種類と表の番号）と「登録した fd の番号」の組で持つ。** 準備ができたかは、fd の
//! 番号を引き直さず、オブジェクトを直に問い合わせる——閉じた後に同じ番号で別のものが開いても、それを見張らない。
//! **オブジェクトを手放すとき（`crate::vfs::File::release_end`）に、そのオブジェクトを指す項目を全部の epoll から外す**
//! （[`forget`]）。参照を数える種類（eventfd・timerfd・epoll）は最後のコピーを閉じたときで、Linux の「最後の参照が閉じたら
//! 黙って外れる」と同じである。ソケットとパイプの端は参照を数えていないので、コピーのどれかを閉じたときに外れる
//! （`ADR-0064` の 2026-10-19 の Addendum の既知の不具合の範囲）。
//!
//! # 入れ子
//!
//! epoll を epoll に足せる。**自分自身は `-EINVAL`、輪になる足し方と、鎖が [`MAX_CHAIN`] 個を越える足し方は `-ELOOP`**
//! （Linux と同じ。WSL で、鎖は 5 個まで、6 個目を作る足しが `ELOOP` であることを測った）。準備の判定は入れ子を辿るが、
//! 鎖が 5 個を越えない形でしか足せないので、辿りも 5 段で止まる。判定の関数も深さを数え、越えたら「準備なし」で止める。
//!
//! # トリガー
//!
//! レベルトリガーと `EPOLLONESHOT` だけである。**`EPOLLET` は断る**（`crate::syscall` が名前のある行を出して `-EINVAL`）。
//! ONESHOT の項目は、一度返したら止まり、`EPOLL_CTL_MOD` で付け直すまで返さない。**`EPOLLERR` と `EPOLLHUP` は、求められて
//! いなくても返す**（`events` が 0 の項目も、この 2 つは返る）。
//!
//! # 表
//!
//! epoll は 8 個、1 つに項目 16 個。参照を数える（`F_DUPFD` のコピーは同じ epoll を指す。wayland-backend の epoll を
//! calloop の epoll へ入れる形）。**BKL の内側で触る。** 表の操作は [`Table`]（純粋な論理。ホストで試す）にあり、リーフの
//! オブジェクトの準備は呼び手が渡す関数で問い合わせる。

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

/// 同時に作れる epoll の数。
pub const MAX_EPOLLS: usize = 8;
/// 1 つの epoll に足せる項目の数。
pub const MAX_ENTRIES: usize = 16;
/// 入れ子の鎖に並べられる epoll の数（Linux の `EP_MAX_NESTS` の 4 段と、一番外の 1 つ）。
pub const MAX_CHAIN: usize = 5;

/// `epoll_event` の `events` のビット（Linux の値）。
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLPRI: u32 = 0x002;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDHUP: u32 = 0x2000;
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLWAKEUP: u32 = 1 << 29;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;

/// 項目が指すオブジェクト（種類と表の番号）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// listen しているソケット（`crate::socket` の listener の番号）。
    Listener { listener: u8 },
    /// 繋がったソケットの端（接続の番号と側。側は `crate::socket::Side` の値）。
    Stream { conn: u8, side: u8 },
    /// パイプの読み端。
    PipeRead { pipe: u8 },
    /// パイプの書き端。
    PipeWrite { pipe: u8 },
    /// eventfd。
    EventFd { id: u8 },
    /// timerfd。
    TimerFd { id: u8 },
    /// 入れ子の epoll。
    Epoll { id: u8 },
    /// 入力の生イベント。**`open` は入力の fd を開いた回の番号である**（`crate::input::open_handle`）。キーボードは 1 つ
    /// だが、開いた回ごとに分けるので、ほかの者が入力の fd を閉じても、この項目は外れない。
    Input { open: u8 },
}

/// `epoll_ctl` の失敗。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CtlError {
    /// epoll が無い（`-EBADF`）。
    NoEpoll,
    /// 自分自身を足そうとした（`-EINVAL`）。
    SelfAdd,
    /// 同じオブジェクトと fd の組が既に在る（`-EEXIST`）。
    Exists,
    /// 組が無い（`-ENOENT`）。
    NotFound,
    /// 輪になるか、鎖が長すぎる（`-ELOOP`）。
    Loop,
    /// 項目が尽きた（`-ENOSPC`）。
    NoSpace,
}

/// 1 つの項目。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    source: Source,
    fd: u32,
    events: u32,
    data: u64,
    /// ONESHOT で一度返した。
    disabled: bool,
}

/// 1 つの epoll。`refs` が 0 なら空いている。
#[derive(Clone, Copy)]
struct Instance {
    refs: u32,
    entries: [Option<Entry>; MAX_ENTRIES],
}

impl Instance {
    const EMPTY: Self = Self {
        refs: 0,
        entries: [None; MAX_ENTRIES],
    };
}

/// 返す 1 件（`epoll_event` の `events` と `data`、それにどの種類の項目だったか）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ready {
    pub events: u32,
    pub data: u64,
    pub kind: Option<SourceKind>,
}

/// 返した項目の種類（待ちが何で終わったかを数えるため）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    Socket,
    Pipe,
    EventFd,
    TimerFd,
    Epoll,
    Input,
}

impl Source {
    /// 種類。
    pub fn kind(self) -> SourceKind {
        match self {
            Self::Listener { .. } | Self::Stream { .. } => SourceKind::Socket,
            Self::PipeRead { .. } | Self::PipeWrite { .. } => SourceKind::Pipe,
            Self::EventFd { .. } => SourceKind::EventFd,
            Self::TimerFd { .. } => SourceKind::TimerFd,
            Self::Epoll { .. } => SourceKind::Epoll,
            Self::Input { .. } => SourceKind::Input,
        }
    }
}

/// 待つときに要るもの（[`Table::wait_hints`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WaitHints {
    /// 辿れる timerfd の締切の最小。
    pub timer_deadline: Option<u64>,
    /// 入力が辿れる。
    pub input: bool,
}

/// epoll の表（純粋な論理）。
pub struct Table {
    instances: [Instance; MAX_EPOLLS],
}

impl Table {
    /// 空の表。
    pub const fn new() -> Self {
        Self {
            instances: [Instance::EMPTY; MAX_EPOLLS],
        }
    }

    fn live(&self, id: u8) -> Option<&Instance> {
        self.instances
            .get(id as usize)
            .filter(|instance| instance.refs != 0)
    }

    fn live_mut(&mut self, id: u8) -> Option<&mut Instance> {
        self.instances
            .get_mut(id as usize)
            .filter(|instance| instance.refs != 0)
    }

    /// 作る。**空きが無ければ `None`。** 参照は 1 から始まる。
    pub fn create(&mut self) -> Option<u8> {
        let index = self
            .instances
            .iter()
            .position(|instance| instance.refs == 0)?;
        self.instances[index] = Instance {
            refs: 1,
            entries: [None; MAX_ENTRIES],
        };
        Some(index as u8)
    }

    /// 参照を 1 つ足す（`F_DUPFD` のコピー）。
    pub fn attach(&mut self, id: u8) {
        if let Some(instance) = self.live_mut(id) {
            instance.refs += 1;
        }
    }

    /// 参照を 1 つ減らす。**0 になったら空け、この epoll を指す項目を全部外して `true` を返す。**
    pub fn detach(&mut self, id: u8) -> bool {
        let Some(instance) = self.live_mut(id) else {
            return false;
        };
        instance.refs -= 1;
        if instance.refs != 0 {
            return false;
        }
        *instance = Instance::EMPTY;
        self.forget(Source::Epoll { id });
        true
    }

    /// そのオブジェクトを指す項目を、全部の epoll から外す。
    pub fn forget(&mut self, source: Source) {
        for instance in self
            .instances
            .iter_mut()
            .filter(|instance| instance.refs != 0)
        {
            for slot in instance.entries.iter_mut() {
                if slot.is_some_and(|entry| entry.source == source) {
                    *slot = None;
                }
            }
        }
    }

    /// `from` から入れ子を辿って `to` に届くか（輪の判定）。深さは [`MAX_CHAIN`] で打ち切る。
    fn reaches(&self, from: u8, to: u8, depth: usize) -> bool {
        if from == to {
            return true;
        }
        if depth >= MAX_CHAIN {
            return false;
        }
        let Some(instance) = self.live(from) else {
            return false;
        };
        instance
            .entries
            .iter()
            .flatten()
            .any(|entry| match entry.source {
                Source::Epoll { id } => self.reaches(id, to, depth + 1),
                _ => false,
            })
    }

    /// `id` から下へ辿れる鎖の長さ（`id` を含む epoll の数）。深さは打ち切る。
    fn chain_below(&self, id: u8, depth: usize) -> usize {
        if depth > MAX_CHAIN {
            return depth;
        }
        let Some(instance) = self.live(id) else {
            return 1;
        };
        instance
            .entries
            .iter()
            .flatten()
            .filter_map(|entry| match entry.source {
                Source::Epoll { id } => Some(1 + self.chain_below(id, depth + 1)),
                _ => None,
            })
            .max()
            .unwrap_or(1)
    }

    /// `id` を含む epoll を上へ辿った鎖の長さ（`id` を含む epoll の数）。深さは打ち切る。
    fn chain_above(&self, id: u8, depth: usize) -> usize {
        if depth > MAX_CHAIN {
            return depth;
        }
        (0..MAX_EPOLLS as u8)
            .filter(|parent| {
                self.live(*parent).is_some_and(|instance| {
                    instance
                        .entries
                        .iter()
                        .flatten()
                        .any(|entry| entry.source == Source::Epoll { id })
                })
            })
            .map(|parent| 1 + self.chain_above(parent, depth + 1))
            .max()
            .unwrap_or(1)
    }

    /// 足す（`EPOLL_CTL_ADD`）。
    pub fn add(
        &mut self,
        id: u8,
        source: Source,
        fd: u32,
        events: u32,
        data: u64,
    ) -> Result<(), CtlError> {
        if self.live(id).is_none() {
            return Err(CtlError::NoEpoll);
        }
        if source == (Source::Epoll { id }) {
            return Err(CtlError::SelfAdd);
        }
        if let Source::Epoll { id: inner } = source {
            // **輪**——足すものから辿って自分に届くなら、足すと輪になる。
            if self.reaches(inner, id, 0) {
                return Err(CtlError::Loop);
            }
            // **鎖の長さ**——自分を含む上への鎖と、足すものを含む下への鎖をつないだ長さ。
            if self.chain_above(id, 0) + self.chain_below(inner, 0) > MAX_CHAIN {
                return Err(CtlError::Loop);
            }
        }
        let instance = self.live_mut(id).ok_or(CtlError::NoEpoll)?;
        if instance
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.source == source && entry.fd == fd)
        {
            return Err(CtlError::Exists);
        }
        let slot = instance
            .entries
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(CtlError::NoSpace)?;
        *slot = Some(Entry {
            source,
            fd,
            events,
            data,
            disabled: false,
        });
        Ok(())
    }

    /// 付け直す（`EPOLL_CTL_MOD`）。**ONESHOT で止まった項目も、ここで動き出す。**
    pub fn modify(
        &mut self,
        id: u8,
        source: Source,
        fd: u32,
        events: u32,
        data: u64,
    ) -> Result<(), CtlError> {
        let instance = self.live_mut(id).ok_or(CtlError::NoEpoll)?;
        let entry = instance
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.source == source && entry.fd == fd)
            .ok_or(CtlError::NotFound)?;
        entry.events = events;
        entry.data = data;
        entry.disabled = false;
        Ok(())
    }

    /// 外す（`EPOLL_CTL_DEL`）。
    pub fn delete(&mut self, id: u8, source: Source, fd: u32) -> Result<(), CtlError> {
        let instance = self.live_mut(id).ok_or(CtlError::NoEpoll)?;
        let slot = instance
            .entries
            .iter_mut()
            .find(|slot| slot.is_some_and(|entry| entry.source == source && entry.fd == fd))
            .ok_or(CtlError::NotFound)?;
        *slot = None;
        Ok(())
    }

    /// その項目が返すビット。`leaf` はリーフのオブジェクトの準備のビットを返す。入れ子の epoll は、中に返すものが在れば
    /// `EPOLLIN`。**求められたビットと、`EPOLLERR`・`EPOLLHUP` だけを返す。**
    fn entry_bits(&self, entry: &Entry, depth: usize, leaf: &dyn Fn(Source) -> u32) -> u32 {
        let ready = match entry.source {
            Source::Epoll { id } => {
                if self.any_ready(id, depth + 1, leaf) {
                    EPOLLIN
                } else {
                    0
                }
            }
            source => leaf(source),
        };
        ready & (entry.events | EPOLLERR | EPOLLHUP)
    }

    /// その epoll に、返すものが 1 つでも在るか（入れ子の判定）。**深さが [`MAX_CHAIN`] を越えたら「無い」で止める。**
    fn any_ready(&self, id: u8, depth: usize, leaf: &dyn Fn(Source) -> u32) -> bool {
        if depth >= MAX_CHAIN {
            return false;
        }
        let Some(instance) = self.live(id) else {
            return false;
        };
        instance
            .entries
            .iter()
            .flatten()
            .filter(|entry| !entry.disabled)
            .any(|entry| self.entry_bits(entry, depth, leaf) != 0)
    }

    /// 返すものを `out` へ集め、数を返す（多くても `out.len()` 件）。**`commit` が真なら、ONESHOT の返した項目を止める**
    /// ——偽なら何も変えない（書き先を確かめる前に数えるため）。epoll が無ければ `None`。
    pub fn collect(
        &mut self,
        id: u8,
        out: &mut [Ready],
        commit: bool,
        leaf: &dyn Fn(Source) -> u32,
    ) -> Option<usize> {
        let snapshot = self.live(id)?.entries;
        let mut count = 0;
        for (index, entry) in snapshot.iter().enumerate() {
            if count == out.len() {
                break;
            }
            let Some(entry) = entry.filter(|entry| !entry.disabled) else {
                continue;
            };
            let bits = self.entry_bits(&entry, 0, leaf);
            if bits == 0 {
                continue;
            }
            out[count] = Ready {
                events: bits,
                data: entry.data,
                kind: Some(entry.source.kind()),
            };
            count += 1;
            if commit && entry.events & EPOLLONESHOT != 0 {
                if let Some(Some(stored)) = self
                    .live_mut(id)
                    .and_then(|instance| instance.entries.get_mut(index))
                {
                    stored.disabled = true;
                }
            }
        }
        Some(count)
    }

    /// 待つときに要るもの——辿れる timerfd の締切の最小と、入力が辿れるか。`deadline_of` は timerfd の締切を返す。
    /// **`EPOLLIN` を求めている項目だけを辿る**——timerfd・入力・入れ子の epoll は `EPOLLIN` しか返さないので、求めて
    /// いない項目の締切で起きても返すものが無い（`events` が 0 の timerfd の過ぎた締切で、ティックごとに空回りする）。
    pub fn wait_hints(&self, id: u8, deadline_of: &dyn Fn(u8) -> Option<u64>) -> WaitHints {
        let mut hints = WaitHints::default();
        self.gather_hints(id, 0, deadline_of, &mut hints);
        hints
    }

    fn gather_hints(
        &self,
        id: u8,
        depth: usize,
        deadline_of: &dyn Fn(u8) -> Option<u64>,
        hints: &mut WaitHints,
    ) {
        if depth >= MAX_CHAIN {
            return;
        }
        let Some(instance) = self.live(id) else {
            return;
        };
        for entry in instance
            .entries
            .iter()
            .flatten()
            .filter(|entry| !entry.disabled && entry.events & EPOLLIN != 0)
        {
            match entry.source {
                Source::TimerFd { id } => {
                    if let Some(deadline) = deadline_of(id) {
                        hints.timer_deadline =
                            Some(hints.timer_deadline.map_or(deadline, |d| d.min(deadline)));
                    }
                }
                Source::Input { .. } => hints.input = true,
                Source::Epoll { id } => self.gather_hints(id, depth + 1, deadline_of, hints),
                _ => {}
            }
        }
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

static EPOLLS: Locked<Table> = Locked::new(Table::new());

/// 待った回数と、待ちが終わったときに返した項目の種類ごとの回数（計測。`epoll-waits:` の判定行に出す）。
static WAITS: AtomicU64 = AtomicU64::new(0);
static ENDED_BY_SOCKET: AtomicU64 = AtomicU64::new(0);
static ENDED_BY_PIPE: AtomicU64 = AtomicU64::new(0);
static ENDED_BY_EVENTFD: AtomicU64 = AtomicU64::new(0);
static ENDED_BY_TIMERFD: AtomicU64 = AtomicU64::new(0);
static ENDED_BY_EPOLL: AtomicU64 = AtomicU64::new(0);
static ENDED_BY_INPUT: AtomicU64 = AtomicU64::new(0);

/// 待った回数。
pub fn waits() -> u64 {
    WAITS.load(Ordering::Relaxed)
}

/// 待ちが終わって返した項目の種類ごとの回数（ソケット・パイプ・eventfd・timerfd・入れ子の epoll・入力）。
pub fn ended_by() -> [u64; 6] {
    [
        ENDED_BY_SOCKET.load(Ordering::Relaxed),
        ENDED_BY_PIPE.load(Ordering::Relaxed),
        ENDED_BY_EVENTFD.load(Ordering::Relaxed),
        ENDED_BY_TIMERFD.load(Ordering::Relaxed),
        ENDED_BY_EPOLL.load(Ordering::Relaxed),
        ENDED_BY_INPUT.load(Ordering::Relaxed),
    ]
}

/// 待つことを数える。**待つのは呼び出し側である**（`crate::syscall`）。
pub fn note_wait() {
    WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 待った後に返した項目を、種類ごとに数える。
pub fn note_ended_by(ready: &[Ready]) {
    for item in ready {
        let counter = match item.kind {
            Some(SourceKind::Socket) => &ENDED_BY_SOCKET,
            Some(SourceKind::Pipe) => &ENDED_BY_PIPE,
            Some(SourceKind::EventFd) => &ENDED_BY_EVENTFD,
            Some(SourceKind::TimerFd) => &ENDED_BY_TIMERFD,
            Some(SourceKind::Epoll) => &ENDED_BY_EPOLL,
            Some(SourceKind::Input) => &ENDED_BY_INPUT,
            None => continue,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// 作る。
pub fn create() -> Option<u8> {
    EPOLLS.lock().create()
}

/// 参照を 1 つ足す。
pub fn attach(id: u8) {
    EPOLLS.lock().attach(id);
}

/// 参照を 1 つ減らす（最後なら空け、この epoll を指す項目を外す）。
pub fn detach(id: u8) {
    if EPOLLS.lock().detach(id) {
        notify();
    }
}

/// そのオブジェクトを指す項目を、全部の epoll から外す（オブジェクトを手放すとき）。
pub fn forget(source: Source) {
    EPOLLS.lock().forget(source);
}

/// `EPOLL_CTL_ADD`。足せたら、epoll で待っている者を起こす（足したものが既に準備できているかもしれない）。
pub fn add(id: u8, source: Source, fd: u32, events: u32, data: u64) -> Result<(), CtlError> {
    EPOLLS.lock().add(id, source, fd, events, data)?;
    notify();
    Ok(())
}

/// `EPOLL_CTL_MOD`。付け直したら起こす。
pub fn modify(id: u8, source: Source, fd: u32, events: u32, data: u64) -> Result<(), CtlError> {
    EPOLLS.lock().modify(id, source, fd, events, data)?;
    notify();
    Ok(())
}

/// `EPOLL_CTL_DEL`。
pub fn delete(id: u8, source: Source, fd: u32) -> Result<(), CtlError> {
    EPOLLS.lock().delete(id, source, fd)
}

/// 返すものを集める（[`Table::collect`]）。
pub fn collect(
    id: u8,
    out: &mut [Ready],
    commit: bool,
    leaf: &dyn Fn(Source) -> u32,
) -> Option<usize> {
    EPOLLS.lock().collect(id, out, commit, leaf)
}

/// 待つときに要るもの（[`Table::wait_hints`]）。
pub fn wait_hints(id: u8, deadline_of: &dyn Fn(u8) -> Option<u64>) -> WaitHints {
    EPOLLS.lock().wait_hints(id, deadline_of)
}

/// epoll で待っている者を全部起こす（空振りを許す。起きた側が問い合わせ直す。`ADR-0061`）。**状態を変える側が、システム
/// コールの文脈（BKL の内側）で呼ぶ**——ソケット・パイプ・eventfd・timerfd の変化と、epoll の項目の足し・付け直し。
/// 割り込みの道からは呼ばない（入力は、待ちの集合に `Keyboard` を入れて、既存の起こしで起きる）。
pub fn notify() {
    crate::task::wake_tasks_waiting_on(crate::task::Wait::Epoll);
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTFD: Source = Source::EventFd { id: 0 };

    fn table_with(count: usize) -> (Table, [u8; MAX_EPOLLS]) {
        let mut table = Table::new();
        let mut ids = [0u8; MAX_EPOLLS];
        for slot in ids.iter_mut().take(count) {
            *slot = table.create().expect("room");
        }
        (table, ids)
    }

    fn nothing_ready(_: Source) -> u32 {
        0
    }

    fn eventfd_readable(source: Source) -> u32 {
        if source == EVENTFD {
            EPOLLIN | EPOLLOUT
        } else {
            0
        }
    }

    #[test]
    fn a_level_triggered_entry_is_returned_every_time_it_is_ready() {
        let (mut table, ids) = table_with(1);
        table.add(ids[0], EVENTFD, 3, EPOLLIN, 77).unwrap();
        let mut out = [Ready::default(); 4];
        for _ in 0..3 {
            assert_eq!(
                table.collect(ids[0], &mut out, true, &eventfd_readable),
                Some(1)
            );
            assert_eq!(
                out[0].events, EPOLLIN,
                "only the requested bit (and ERR/HUP)"
            );
            assert_eq!(out[0].data, 77);
        }
        assert_eq!(
            table.collect(ids[0], &mut out, true, &nothing_ready),
            Some(0)
        );
    }

    #[test]
    fn a_oneshot_entry_is_returned_once_until_it_is_modified() {
        let (mut table, ids) = table_with(1);
        table
            .add(ids[0], EVENTFD, 3, EPOLLIN | EPOLLONESHOT, 1)
            .unwrap();
        let mut out = [Ready::default(); 4];
        assert_eq!(
            table.collect(ids[0], &mut out, false, &eventfd_readable),
            Some(1)
        );
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(1)
        );
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(0)
        );
        table
            .modify(ids[0], EVENTFD, 3, EPOLLIN | EPOLLONESHOT, 2)
            .unwrap();
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(1)
        );
        assert_eq!(out[0].data, 2);
    }

    #[test]
    fn err_and_hup_are_returned_even_when_not_requested() {
        let (mut table, ids) = table_with(1);
        table.add(ids[0], EVENTFD, 3, 0, 5).unwrap();
        let mut out = [Ready::default(); 4];
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(0)
        );
        let hangs_up = |_: Source| EPOLLIN | EPOLLHUP | EPOLLERR;
        assert_eq!(table.collect(ids[0], &mut out, true, &hangs_up), Some(1));
        assert_eq!(out[0].events, EPOLLHUP | EPOLLERR);
    }

    #[test]
    fn adding_twice_deleting_and_modifying_missing_entries_are_refused() {
        let (mut table, ids) = table_with(1);
        table.add(ids[0], EVENTFD, 3, EPOLLIN, 0).unwrap();
        assert_eq!(
            table.add(ids[0], EVENTFD, 3, EPOLLIN, 0),
            Err(CtlError::Exists)
        );
        assert_eq!(
            table.add(ids[0], EVENTFD, 4, EPOLLIN, 0),
            Ok(()),
            "another fd of the same object"
        );
        assert_eq!(table.delete(ids[0], EVENTFD, 3), Ok(()));
        assert_eq!(table.delete(ids[0], EVENTFD, 3), Err(CtlError::NotFound));
        assert_eq!(
            table.modify(ids[0], EVENTFD, 3, EPOLLIN, 0),
            Err(CtlError::NotFound)
        );
    }

    #[test]
    fn forgetting_an_object_removes_it_from_every_epoll() {
        let (mut table, ids) = table_with(2);
        table.add(ids[0], EVENTFD, 3, EPOLLIN, 0).unwrap();
        table.add(ids[1], EVENTFD, 9, EPOLLIN, 0).unwrap();
        table.forget(EVENTFD);
        let mut out = [Ready::default(); 4];
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(0)
        );
        assert_eq!(
            table.collect(ids[1], &mut out, true, &eventfd_readable),
            Some(0)
        );
    }

    #[test]
    fn adding_itself_is_invalid_and_a_ring_is_a_loop() {
        let (mut table, ids) = table_with(2);
        let (a, b) = (ids[0], ids[1]);
        assert_eq!(
            table.add(a, Source::Epoll { id: a }, 1, EPOLLIN, 0),
            Err(CtlError::SelfAdd)
        );
        assert_eq!(table.add(a, Source::Epoll { id: b }, 2, EPOLLIN, 0), Ok(()));
        assert_eq!(
            table.add(b, Source::Epoll { id: a }, 3, EPOLLIN, 0),
            Err(CtlError::Loop)
        );
    }

    #[test]
    fn a_chain_of_five_epolls_is_allowed_and_a_sixth_is_a_loop_from_either_end() {
        // 上から足す（WSL の実測と同じ形）。
        let (mut table, ids) = table_with(6);
        for i in 0..4 {
            assert_eq!(
                table.add(
                    ids[i],
                    Source::Epoll { id: ids[i + 1] },
                    10 + i as u32,
                    EPOLLIN,
                    0
                ),
                Ok(())
            );
        }
        assert_eq!(
            table.add(ids[4], Source::Epoll { id: ids[5] }, 20, EPOLLIN, 0),
            Err(CtlError::Loop)
        );
        // 下から足す。
        let (mut table, ids) = table_with(6);
        for i in (1..5).rev() {
            assert_eq!(
                table.add(
                    ids[i],
                    Source::Epoll { id: ids[i + 1] },
                    10 + i as u32,
                    EPOLLIN,
                    0
                ),
                Ok(())
            );
        }
        assert_eq!(
            table.add(ids[0], Source::Epoll { id: ids[1] }, 30, EPOLLIN, 0),
            Err(CtlError::Loop)
        );
    }

    #[test]
    fn a_nested_epoll_is_ready_when_something_inside_is_ready() {
        let (mut table, ids) = table_with(5);
        for i in 0..4 {
            table
                .add(
                    ids[i],
                    Source::Epoll { id: ids[i + 1] },
                    10,
                    EPOLLIN,
                    i as u64,
                )
                .unwrap();
        }
        table.add(ids[4], EVENTFD, 3, EPOLLIN, 99).unwrap();
        let mut out = [Ready::default(); 4];
        assert_eq!(
            table.collect(ids[0], &mut out, true, &nothing_ready),
            Some(0)
        );
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(1)
        );
        assert_eq!(out[0].events, EPOLLIN);
        assert_eq!(out[0].kind, Some(SourceKind::Epoll));
    }

    #[test]
    fn freeing_an_epoll_removes_the_entries_that_point_at_it() {
        let (mut table, ids) = table_with(2);
        table
            .add(ids[0], Source::Epoll { id: ids[1] }, 4, EPOLLIN, 0)
            .unwrap();
        table.attach(ids[1]);
        assert!(!table.detach(ids[1]), "one reference is left");
        assert!(table.detach(ids[1]));
        let reused = table.create().unwrap();
        assert_eq!(reused, ids[1]);
        table.add(reused, EVENTFD, 3, EPOLLIN, 0).unwrap();
        let mut out = [Ready::default(); 4];
        assert_eq!(
            table.collect(ids[0], &mut out, true, &eventfd_readable),
            Some(0),
            "the outer epoll no longer watches the freed one, even though its number came back"
        );
    }

    #[test]
    fn wait_hints_find_the_earliest_timer_and_input_through_nesting() {
        let (mut table, ids) = table_with(2);
        table
            .add(ids[0], Source::TimerFd { id: 1 }, 3, EPOLLIN, 0)
            .unwrap();
        table
            .add(ids[0], Source::Epoll { id: ids[1] }, 4, EPOLLIN, 0)
            .unwrap();
        table
            .add(ids[1], Source::TimerFd { id: 2 }, 5, EPOLLIN, 0)
            .unwrap();
        table
            .add(ids[1], Source::Input { open: 0 }, 6, EPOLLIN, 0)
            .unwrap();
        let deadline_of = |id: u8| match id {
            1 => Some(50),
            2 => Some(30),
            _ => None,
        };
        assert_eq!(
            table.wait_hints(ids[0], &deadline_of),
            WaitHints {
                timer_deadline: Some(30),
                input: true
            }
        );
    }

    #[test]
    fn a_timerfd_entry_that_does_not_ask_for_epollin_puts_no_deadline_in_the_wait() {
        let (mut table, ids) = table_with(1);
        table
            .add(ids[0], Source::TimerFd { id: 1 }, 3, 0, 0)
            .unwrap();
        table
            .add(ids[0], Source::Input { open: 0 }, 4, 0, 0)
            .unwrap();
        let deadline_of = |_: u8| Some(10);
        assert_eq!(table.wait_hints(ids[0], &deadline_of), WaitHints::default());
        table
            .modify(ids[0], Source::TimerFd { id: 1 }, 3, EPOLLIN, 0)
            .unwrap();
        assert_eq!(
            table.wait_hints(ids[0], &deadline_of).timer_deadline,
            Some(10)
        );
    }

    #[test]
    fn closing_another_open_of_the_input_keeps_this_input_entry() {
        let mut opens = crate::input::OpenCounts::new();
        let mine = opens.open().unwrap();
        let (mut table, ids) = table_with(1);
        table
            .add(ids[0], Source::Input { open: mine }, 5, EPOLLIN, 42)
            .unwrap();
        // 別の者が入力の fd を開いて閉じる（`crate::input::detach_handle` と同じ順）。
        let other = opens.open().unwrap();
        if opens.detach(other) {
            table.forget(Source::Input { open: other });
        }
        let keys_waiting = |source: Source| {
            if matches!(source, Source::Input { .. }) {
                EPOLLIN
            } else {
                0
            }
        };
        let mut out = [Ready::default(); 4];
        assert_eq!(
            table.collect(ids[0], &mut out, true, &keys_waiting),
            Some(1)
        );
        assert_eq!(out[0].data, 42);
        // 自分の最後のコピーを閉じたら外れる。
        if opens.detach(mine) {
            table.forget(Source::Input { open: mine });
        }
        assert_eq!(
            table.collect(ids[0], &mut out, true, &keys_waiting),
            Some(0)
        );
    }
}
