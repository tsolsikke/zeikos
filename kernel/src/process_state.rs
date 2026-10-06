//! プロセスごとの小さな状態（2026-10-06）——シグナルの登録と、マスク、代替スタック、`set_tid_address` の番地、
//! 実行ファイルの名前。
//!
//! # なぜ `UserProcess` に足さないのか
//!
//! **`UserProcess` は、載せる側の遠征スタックの上に在る**（`crate::userland`）。シグナルの表は 64 項 × 32 バイトで、
//! そこへ足すと、子が走っている間ずっと残る枠が 2 KiB 太る（止まる線は `ring3` の `EXCURSION_STACK_BUDGET`）。
//! **スロットと、遠征の深さごとの静的な置き場に置く**——`CURRENT_HEAP` や `CURRENT_FILES` と同じ考え方で、
//! あちらはスロットごとに 1 つを据え替えるが、こちらは据え替えに要る写しも遠征スタックに載せたくないので、深さで引く。
//!
//! # 配送はしない
//!
//! **登録と問い合わせだけである。** どのシグナルも、まだ起きない（ユーザーのページフォルトは、今までどおりプロセスの
//! 終了にする。`docs/deferred-decisions.md` の「シグナルを配送する」）。登録を受けるのは、Linux 向けの libc が起動の
//! 最初に `rt_sigaction`・`sigaltstack`・`rt_sigprocmask` を呼び、失敗すると先へ進まないからである。

use common::critical::Locked;

use crate::arch::x86_64::{MAX_EXCURSION_DEPTH, USER_TASK_SLOTS};

/// シグナルの数（Linux の `_NSIG`）。番号は 1 から 64 である。
pub const SIGNAL_COUNT: usize = 64;

/// `SIGKILL`。登録を変えられない。
pub const SIGKILL: u64 = 9;
/// `SIGSTOP`。登録を変えられない。
pub const SIGSTOP: u64 = 19;

/// `sa_flags` の `SA_RESTORER`（x86_64 の `asm/signal.h`）。**カーネルは、ハンドラから戻る道を自分では持たない**
/// ので、ハンドラを登録する求めは、これを立てていなければ断る（musl・glibc は必ず立てる）。
pub const SA_RESTORER: u64 = 0x0400_0000;
/// `SIG_DFL`（既定の振る舞い）。
pub const SIG_DFL: u64 = 0;
/// `SIG_IGN`（無視）。
pub const SIG_IGN: u64 = 1;

/// `rt_sigprocmask` の `how`。
pub const SIG_BLOCK: u64 = 0;
pub const SIG_UNBLOCK: u64 = 1;
pub const SIG_SETMASK: u64 = 2;

/// `sigaltstack` の `ss_flags`。
pub const SS_ONSTACK: u64 = 1;
pub const SS_DISABLE: u64 = 2;
/// `SS_AUTODISARM`（Linux 4.7 から。glibc が付けることが在る）。受けて、控えるだけである。
pub const SS_AUTODISARM: u64 = 1 << 31;
/// 代替スタックの最小の大きさ（`MINSIGSTKSZ`。x86_64）。
pub const MINSIGSTKSZ: u64 = 2048;

/// 実行ファイルの名前を控える長さの上限（`readlink("/proc/self/exe")` が返す）。
pub const EXEC_NAME_MAX: usize = 64;

/// 1 つのシグナルの登録（Linux の x86_64 の、カーネルが受ける `struct sigaction` の並び。32 バイト）。
///
/// 並びは `sa_handler`・`sa_flags`・`sa_restorer`・`sa_mask` である（`arch/x86/include/uapi/asm/signal.h` の
/// `struct sigaction`。glibc と musl は、ユーザー側の `struct sigaction` からこの並びへ詰め替えて渡す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl SigAction {
    /// 既定（`SIG_DFL`。フラグもマスクも 0）。
    pub const DEFAULT: Self = Self {
        handler: SIG_DFL,
        flags: 0,
        restorer: 0,
        mask: 0,
    };

    /// 32 バイトの並びから読む（純粋な論理）。
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        Self {
            handler: word(0),
            flags: word(8),
            restorer: word(16),
            mask: word(24),
        }
    }

    /// 32 バイトの並びへ書く（純粋な論理）。
    pub fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..8].copy_from_slice(&self.handler.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.flags.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.restorer.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.mask.to_le_bytes());
        bytes
    }
}

/// 代替スタック（`stack_t`。`ss_sp`・`ss_flags`（`i32` と詰め物）・`ss_size`。24 バイト）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltStack {
    pub sp: u64,
    pub flags: u64,
    pub size: u64,
}

impl AltStack {
    /// 無効（`SS_DISABLE`）。
    pub const DISABLED: Self = Self {
        sp: 0,
        flags: SS_DISABLE,
        size: 0,
    };

    /// 24 バイトの並びから読む（純粋な論理）。`ss_flags` は 32 ビットで、上位の 4 バイトは詰め物である。
    pub fn from_bytes(bytes: &[u8; 24]) -> Self {
        Self {
            sp: u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes")),
            flags: u64::from(u32::from_le_bytes(
                bytes[8..12].try_into().expect("4 bytes"),
            )),
            size: u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")),
        }
    }

    /// 24 バイトの並びへ書く（純粋な論理）。
    pub fn to_bytes(self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&self.sp.to_le_bytes());
        bytes[8..12].copy_from_slice(&(self.flags as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&self.size.to_le_bytes());
        bytes
    }
}

/// 断る理由（呼ぶ側が errno に写す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    /// 値が受け付けられない（`-EINVAL`）。
    Invalid,
    /// 代替スタックが小さすぎる（`-ENOMEM`）。
    TooSmall,
}

/// 1 つのプロセスの状態。
pub struct ProcessState {
    actions: [SigAction; SIGNAL_COUNT],
    blocked: u64,
    alt_stack: AltStack,
    tid_address: u64,
    exec_name: [u8; EXEC_NAME_MAX],
    exec_name_len: u8,
}

impl ProcessState {
    /// プロセスが始まる前の形（登録は全部 `SIG_DFL`、マスクは空、代替スタックは無効）。
    pub const FRESH: Self = Self {
        actions: [SigAction::DEFAULT; SIGNAL_COUNT],
        blocked: 0,
        alt_stack: AltStack::DISABLED,
        tid_address: 0,
        exec_name: [0; EXEC_NAME_MAX],
        exec_name_len: 0,
    };

    /// `signal`（1 から 64）の登録を読む（2026-10-06。`tkill` が `SIG_IGN` かを見る）。範囲の外は既定。
    pub fn action(&self, signal: u64) -> SigAction {
        match signal {
            1..=64 => self.actions[(signal - 1) as usize],
            _ => SigAction::DEFAULT,
        }
    }

    /// `rt_sigaction`（純粋な論理）。`new` が `Some` なら登録を替え、どちらでも前の登録を返す。
    ///
    /// - 番号は 1 から 64。`SIGKILL` と `SIGSTOP` は、問い合わせはできるが替えられない（Linux と同じ）。
    /// - ハンドラ（`SIG_DFL`・`SIG_IGN` 以外）を登録する求めは、`SA_RESTORER` が立っていなければ断る。
    pub fn set_action(
        &mut self,
        signal: u64,
        new: Option<SigAction>,
    ) -> Result<SigAction, StateError> {
        if signal == 0 || signal > SIGNAL_COUNT as u64 {
            return Err(StateError::Invalid);
        }
        let index = (signal - 1) as usize;
        let old = self.actions[index];
        if let Some(new) = new {
            if signal == SIGKILL || signal == SIGSTOP {
                return Err(StateError::Invalid);
            }
            let is_handler = new.handler != SIG_DFL && new.handler != SIG_IGN;
            if is_handler && new.flags & SA_RESTORER == 0 {
                return Err(StateError::Invalid);
            }
            self.actions[index] = new;
        }
        Ok(old)
    }

    /// `rt_sigprocmask`（純粋な論理）。`set` が `Some` ならマスクを変え、どちらでも前のマスクを返す。
    /// **`SIGKILL` と `SIGSTOP` は塞げない**（Linux と同じに、黙って外す）。
    pub fn change_mask(&mut self, how: u64, set: Option<u64>) -> Result<u64, StateError> {
        let old = self.blocked;
        if let Some(set) = set {
            let unblockable = (1u64 << (SIGKILL - 1)) | (1u64 << (SIGSTOP - 1));
            self.blocked = match how {
                SIG_BLOCK => old | set,
                SIG_UNBLOCK => old & !set,
                SIG_SETMASK => set,
                _ => return Err(StateError::Invalid),
            } & !unblockable;
        }
        // `set` が無ければ `how` は見ない（Linux と同じ）。
        Ok(old)
    }

    /// `sigaltstack`（純粋な論理）。`new` が `Some` なら代替スタックを替え、どちらでも前の値を返す。
    ///
    /// 新しい値は、`SS_DISABLE` なら無効にし、そうでなければ `ss_flags` が 0 か `SS_AUTODISARM` で、大きさが
    /// `MINSIGSTKSZ` 以上であること。
    pub fn set_alt_stack(&mut self, new: Option<AltStack>) -> Result<AltStack, StateError> {
        let old = self.alt_stack;
        if let Some(new) = new {
            if new.flags & SS_DISABLE != 0 {
                self.alt_stack = AltStack::DISABLED;
            } else {
                if new.flags & !SS_AUTODISARM != 0 {
                    return Err(StateError::Invalid);
                }
                if new.size < MINSIGSTKSZ {
                    return Err(StateError::TooSmall);
                }
                self.alt_stack = AltStack {
                    sp: new.sp,
                    flags: new.flags,
                    size: new.size,
                };
            }
        }
        Ok(old)
    }

    /// `set_tid_address` の番地を控える。**書き込みはしない**（スレッドが終わるときに 0 を書いて `futex` で起こす
    /// 仕組みは、スレッドが無いので要らない）。
    pub fn set_tid_address(&mut self, address: u64) {
        self.tid_address = address;
    }

    /// 控えた `set_tid_address` の番地（判定の行に出す）。
    pub fn tid_address(&self) -> u64 {
        self.tid_address
    }

    /// 実行ファイルの名前を控える（長ければ切る）。
    pub fn set_exec_name(&mut self, name: &[u8]) {
        let len = name.len().min(EXEC_NAME_MAX);
        self.exec_name[..len].copy_from_slice(&name[..len]);
        self.exec_name_len = len as u8;
    }

    /// 控えた実行ファイルの名前。
    pub fn exec_name(&self) -> &[u8] {
        &self.exec_name[..usize::from(self.exec_name_len)]
    }
}

/// スロットと、遠征の深さごとの置き場。**深さの添字は、そのプロセスが走る遠征の深さから 1 を引いたものである**
/// （載せる側が据えるときは、今の深さそのもの。`crate::arch::x86_64::excursion_depth`）。
static STATES: [[Locked<ProcessState>; MAX_EXCURSION_DEPTH]; USER_TASK_SLOTS] =
    [const { [const { Locked::new(ProcessState::FRESH) }; MAX_EXCURSION_DEPTH] }; USER_TASK_SLOTS];

/// 添字の範囲の外（深さの取り違え）。止める。
#[inline(never)]
#[cold]
fn report_depth_out_of_range(depth: usize) -> ! {
    panic!(
        "process-state: the excursion depth index {depth} is out of range (MAX_EXCURSION_DEPTH = \
         {MAX_EXCURSION_DEPTH}); the process state would be read from the wrong slot"
    );
}

/// これから走らせるプロセスの状態を初めの形に戻し、実行ファイルの名前を控える。**載せる側が、Ring 3 へ落ちる前に呼ぶ**
/// （`crate::userland`）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - 今のスロットの、今の遠征の深さの欄を書く。ほかは何も変えない。
pub fn reset_for_next_process(exec_name: &[u8]) {
    let depth = crate::arch::x86_64::excursion_depth();
    if depth >= MAX_EXCURSION_DEPTH {
        report_depth_out_of_range(depth);
    }
    let mut state = STATES[crate::arch::x86_64::current_excursion_slot()][depth].lock();
    *state = ProcessState::FRESH;
    state.set_exec_name(exec_name);
}

/// 今走っているプロセス（システムコールを打った側）の状態へ触る。**遠征の中から呼ぶ**（`crate::syscall`）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - 今のスロットの、今の遠征の深さから 1 を引いた欄へ触る。遠征の外（深さ 0）から呼ぶと止まる。
pub fn with_current<R>(body: impl FnOnce(&mut ProcessState) -> R) -> R {
    let depth = crate::arch::x86_64::excursion_depth();
    let Some(index) = depth
        .checked_sub(1)
        .filter(|index| *index < MAX_EXCURSION_DEPTH)
    else {
        report_depth_out_of_range(depth);
    };
    body(&mut STATES[crate::arch::x86_64::current_excursion_slot()][index].lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登録の 32 バイトの並びは、読んで書くと元に戻る。
    #[test]
    fn a_sigaction_survives_the_round_trip_through_its_bytes() {
        let action = SigAction {
            handler: 0x40_1000,
            flags: SA_RESTORER | 0x1000_0000,
            restorer: 0x40_2000,
            mask: 0x8000_0200,
        };
        assert_eq!(SigAction::from_bytes(&action.to_bytes()), action);
        let bytes = action.to_bytes();
        assert_eq!(&bytes[0..8], &0x40_1000u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0x8000_0200u64.to_le_bytes());
    }

    /// `rt_sigaction`: 番号の範囲、`SIGKILL`・`SIGSTOP`、`SA_RESTORER` の無いハンドラを断り、前の登録を返す。
    #[test]
    fn sigaction_refuses_what_linux_refuses_and_returns_the_old_action() {
        let mut state = ProcessState::FRESH;
        let ignore = SigAction {
            handler: SIG_IGN,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        assert_eq!(state.set_action(13, Some(ignore)), Ok(SigAction::DEFAULT));
        assert_eq!(state.set_action(13, None), Ok(ignore));
        assert_eq!(state.set_action(0, None), Err(StateError::Invalid));
        assert_eq!(state.set_action(65, None), Err(StateError::Invalid));
        assert_eq!(
            state.set_action(SIGKILL, Some(ignore)),
            Err(StateError::Invalid)
        );
        assert_eq!(
            state.set_action(SIGSTOP, Some(ignore)),
            Err(StateError::Invalid)
        );
        // 問い合わせはできる。
        assert_eq!(state.set_action(SIGKILL, None), Ok(SigAction::DEFAULT));
        let without_restorer = SigAction {
            handler: 0x40_1000,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        assert_eq!(
            state.set_action(11, Some(without_restorer)),
            Err(StateError::Invalid)
        );
        let with_restorer = SigAction {
            flags: SA_RESTORER,
            restorer: 0x40_2000,
            ..without_restorer
        };
        assert_eq!(
            state.set_action(11, Some(with_restorer)),
            Ok(SigAction::DEFAULT)
        );
        assert_eq!(state.set_action(11, None), Ok(with_restorer));
    }

    /// `rt_sigprocmask`: 3 つの `how`、知らない `how`、塞げない 2 つ。
    #[test]
    fn the_mask_changes_by_how_and_never_blocks_kill_or_stop() {
        let mut state = ProcessState::FRESH;
        assert_eq!(state.change_mask(SIG_BLOCK, Some(0b1100)), Ok(0));
        assert_eq!(state.change_mask(SIG_BLOCK, Some(0b0001)), Ok(0b1100));
        assert_eq!(state.change_mask(SIG_UNBLOCK, Some(0b0100)), Ok(0b1101));
        assert_eq!(state.change_mask(SIG_SETMASK, Some(0b0010)), Ok(0b1001));
        assert_eq!(state.change_mask(7, Some(1)), Err(StateError::Invalid));
        assert_eq!(state.change_mask(7, None), Ok(0b0010));
        let kill_and_stop = (1u64 << (SIGKILL - 1)) | (1u64 << (SIGSTOP - 1));
        assert_eq!(
            state.change_mask(SIG_SETMASK, Some(kill_and_stop | 1)),
            Ok(0b0010)
        );
        assert_eq!(state.change_mask(SIG_SETMASK, None), Ok(1));
    }

    /// `sigaltstack`: 据えて読み戻す、小さすぎる、知らないフラグ、無効にする。並びの往復も。
    #[test]
    fn the_alternate_stack_is_kept_checked_and_read_back() {
        let mut state = ProcessState::FRESH;
        assert_eq!(state.set_alt_stack(None), Ok(AltStack::DISABLED));
        let stack = AltStack {
            sp: 0x7000_0000,
            flags: 0,
            size: 8192,
        };
        assert_eq!(state.set_alt_stack(Some(stack)), Ok(AltStack::DISABLED));
        assert_eq!(state.set_alt_stack(None), Ok(stack));
        assert_eq!(
            state.set_alt_stack(Some(AltStack { size: 100, ..stack })),
            Err(StateError::TooSmall)
        );
        assert_eq!(
            state.set_alt_stack(Some(AltStack { flags: 4, ..stack })),
            Err(StateError::Invalid)
        );
        assert_eq!(
            state.set_alt_stack(Some(AltStack {
                flags: SS_AUTODISARM,
                ..stack
            })),
            Ok(stack)
        );
        assert_eq!(
            state.set_alt_stack(Some(AltStack {
                flags: SS_DISABLE,
                size: 0,
                sp: 0
            })),
            Ok(AltStack {
                flags: SS_AUTODISARM,
                ..stack
            })
        );
        assert_eq!(state.set_alt_stack(None), Ok(AltStack::DISABLED));
        assert_eq!(AltStack::from_bytes(&stack.to_bytes()), stack);
        // `ss_flags` の上位 4 バイトは詰め物で、読まない。
        let mut bytes = stack.to_bytes();
        bytes[12..16].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert_eq!(AltStack::from_bytes(&bytes), stack);
    }

    /// 実行ファイルの名前は、上限で切って控える。
    #[test]
    fn the_exec_name_is_kept_up_to_the_limit() {
        let mut state = ProcessState::FRESH;
        state.set_exec_name(b"/bin/pie-hello");
        assert_eq!(state.exec_name(), b"/bin/pie-hello");
        let long = [b'x'; EXEC_NAME_MAX + 10];
        state.set_exec_name(&long);
        assert_eq!(state.exec_name().len(), EXEC_NAME_MAX);
        state.set_tid_address(0x1234);
        assert_eq!(state.tid_address(), 0x1234);
    }
}
