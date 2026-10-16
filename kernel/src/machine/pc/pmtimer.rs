//! ACPI の Power Management タイマ（HW-c。`ADR-0068`）。
//!
//! **unsafe を含む。** ポートを 1 本読む。
//!
//! # 何のために在るのか
//!
//! **Local APIC タイマを較正する基準にする。** 初めは PIT が刻まない機械のためだった（**PIT のティックが
//! 1 本も来ないと、較正は諦め、タイマは PIT のまま進み、ティックを待つ所で黙る**。実測。`pit=off`。
//! `docs/hardware-inventory.md`）。**2026-10-11 からは、FADT が示せば既定の基準である**——PIT のティックは、KVM が
//! 溜めてまとめて入れたり、捨てたりするので、数えると実時間からずれる（`ADR-0068` の Addendum）。
//! **PM タイマは割り込みを要らず、ポートを読むだけである。**
//!
//! # 何をしないか
//!
//! - **時計として使わない。** **較正のウィンドウを測るためだけに読む**（`crate::machine::pc::apic`）。
//! - **書かない。** **PM タイマは読み出し専用のカウンタである。**
//! - **在りかは決めない。** **ポートと幅は FADT が示す**（`crate::machine::pc::acpi`）——**既定値を焼き込まない。**
//!
//! # 幅は 24 か 32 である
//!
//! **FADT の `Flags` の TMR_VAL_EXT が示す。** **24 ビットなら約 4.7 秒で一周する**
//! （3.579545MHz）。**差を取るときは幅で包み込む**（[`PmTimer::elapsed`]）——**忘れると、
//! 一周したウィンドウで巨大な差が出て、較正が過大になる。**

use common::arch::x86_64::port::inl;

/// PM タイマの周波数（ACPI が定める値）。**機械に依らない。**
///
/// 破壊テスト (HW-c, pm-timer-double-frequency): **2 倍にする。** **較正が 2 倍に出て、タイマは半分の
/// 速さで走る。** **カーネル内の比は自己無矛盾のままなので、実時間と突き合わせて初めて見える**
/// （`lapic-timer-test` の速さの判定）。
pub const HZ: u64 = if cfg!(feature = "pm-timer-double-frequency") {
    2 * 3_579_545
} else {
    3_579_545
};

/// いちばん狭い幅（ビット）。**24 ビットの PM タイマは約 4.7 秒で一周する。**
///
/// **較正のウィンドウがこの一周より十分短いことを、`crate::machine::pc::apic` が const assert で守る。**
pub const NARROWEST_WIDTH_BITS: u32 = 24;

/// PM タイマの所在。**FADT から作る**（`crate::machine::pc::acpi`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmTimer {
    port: u16,
    bits: u8,
}

impl PmTimer {
    /// FADT が示したポートと幅で作る。
    ///
    /// # Safety
    ///
    /// `port` が、FADT が PM タイマの在りかとして名乗ったポートであること。**[`Self::read`] は安全な関数で、この
    /// ポートを読む**——ほかのポートを渡すと、安全な関数から任意のポートを読めてしまう（2026-09-30 に、作れるのを
    /// `machine` の中だけにした）。
    pub(in crate::machine) const unsafe fn new(port: u16, bits: u8) -> Self {
        Self { port, bits }
    }

    /// 読むポート（判定行に出す）。
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// 数える幅（24 か 32）。
    pub const fn bits(&self) -> u8 {
        self.bits
    }

    /// いまの値を読む。
    ///
    /// **副作用は無い**（読み出し専用のカウンタである）。**幅が 24 ビットなら上位は 0 である**
    /// （仕様。**こちらで切らない**——**切ると「上位にゴミを返す機械」を隠す。** 差は
    /// [`Self::elapsed`] が幅で包み込む）。
    pub fn read(&self) -> u32 {
        // SAFETY: 作る所の契約（`new` の # Safety）で、FADT が PM タイマの在りかとして名乗ったポートを読むだけである。
        // **書かない。** 読み出しはカウンタの値を返すだけで、装置の状態を変えない。
        unsafe { inl(self.port) }
    }

    /// 2 回の読みの間に進んだティック（純粋ロジック）。**幅で包み込む。**
    pub const fn elapsed(&self, before: u32, after: u32) -> u32 {
        elapsed_with_width(before, after, self.bits)
    }

    /// 3 回続けて読み、並びが崩れていなければ真ん中の値を返す（2026-10-11）。戻り値の 2 つ目は、読み直した回数である。
    ///
    /// **Linux の `acpi_pm_read_verified`（`drivers/clocksource/acpi_pm.c`）と同じ形である。** カウンタを一度に
    /// 読めないチップセット（ICH4・PIIX4 など）では、読みがまれに前後する。3 つの並びが崩れていたら読み直す。
    /// **Linux は上限なしで回すが、ここでは [`VERIFIED_READ_ATTEMPTS`] 回で諦めて `None` を返す**（上限のない待ちを
    /// 書かない）。
    pub fn read_verified(&self) -> Option<(u32, u32)> {
        let mask = width_mask(self.bits);
        for retries in 0..VERIFIED_READ_ATTEMPTS {
            let first = self.read() & mask;
            let middle = self.read() & mask;
            let last = self.read() & mask;
            if reads_are_ordered(first, middle, last) {
                return Some((middle, retries));
            }
        }
        None
    }

    /// 較正の基準に使う前に、値が前へ進むことを確かめる（2026-10-11）。
    ///
    /// **Linux の `init_acpi_pm_clocksource` の確かめと同じ考え方である。** 最初に読んだ値から、変わるまで読む。
    /// 増えたか、一周して小さな値（[`WRAP_THRESHOLD`] 未満）になったなら、その回は通る。それを
    /// [`MONOTONICITY_CHECKS`] 回くり返す。**Linux は回の間に 100µs × 回 の待ちを挟むが、ここでは挟まない**
    /// （まだ時間を測る手段が無いので）。
    pub fn check_consistency(&self) -> Result<(), ConsistencyFailure> {
        let mask = width_mask(self.bits);
        for _ in 0..MONOTONICITY_CHECKS {
            let before = self.read() & mask;
            let mut changed = false;
            for _ in 0..READ_CHECKS {
                let after = self.read() & mask;
                match classify_change(before, after) {
                    Change::Same => continue,
                    Change::Forward | Change::Wrapped => {
                        changed = true;
                        break;
                    }
                    Change::Backward => {
                        return Err(ConsistencyFailure::WentBackward { before, after })
                    }
                }
            }
            if !changed {
                return Err(ConsistencyFailure::DidNotAdvance);
            }
        }
        Ok(())
    }
}

/// [`PmTimer::read_verified`] が読み直す上限。**Linux には無い。** ふつうは 1 回目で通るので、16 回で足りる。
pub const VERIFIED_READ_ATTEMPTS: u32 = 16;

/// [`PmTimer::check_consistency`] の回数（Linux の `ACPI_PM_MONOTONICITY_CHECKS` と同じ値）。
pub const MONOTONICITY_CHECKS: u32 = 10;

/// [`PmTimer::check_consistency`] が 1 回の中で読む上限（Linux の `ACPI_PM_READ_CHECKS` と同じ値）。
pub const READ_CHECKS: u32 = 10_000;

/// 値が減ったときに「一周した」とみなす上限（Linux の `init_acpi_pm_clocksource` と同じ `0xFFF`）。
pub const WRAP_THRESHOLD: u32 = 0xFFF;

/// [`PmTimer::check_consistency`] が通らなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsistencyFailure {
    /// [`READ_CHECKS`] 回読んでも値が変わらなかった。
    DidNotAdvance,
    /// 値が、一周とは言えない形で減った。
    WentBackward { before: u32, after: u32 },
}

impl core::fmt::Display for ConsistencyFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DidNotAdvance => write!(f, "the value did not change in {READ_CHECKS} read(s)"),
            Self::WentBackward { before, after } => {
                write!(f, "the value went back from {before:#x} to {after:#x}")
            }
        }
    }
}

/// 2 回の読みの関係（純粋ロジック）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Same,
    Forward,
    /// 減ったが、小さな値なので一周したとみなす。
    Wrapped,
    Backward,
}

/// 2 回の読みを比べる（純粋ロジック。幅で切った値を渡す）。**Linux の `init_acpi_pm_clocksource` と同じ判定である。**
pub const fn classify_change(before: u32, after: u32) -> Change {
    if after == before {
        Change::Same
    } else if after > before {
        Change::Forward
    } else if after < WRAP_THRESHOLD {
        Change::Wrapped
    } else {
        Change::Backward
    }
}

/// 3 回続けた読みの並びが崩れていないか（純粋ロジック。幅で切った値を渡す）。
///
/// **Linux の `acpi_pm_read_verified` のくり返しの条件を、裏返したものである**——`(v1 > v2 && v1 < v3) ||
/// (v2 > v3 && v2 < v1) || (v3 > v1 && v3 < v2)` のどれかが成り立てば崩れている。一周をまたいだ並び
/// （例: `0xff_fffe, 0x1, 0x3`）は崩れていないと判定される。
pub const fn reads_are_ordered(first: u32, middle: u32, last: u32) -> bool {
    !((first > middle && first < last)
        || (middle > last && middle < first)
        || (last > first && last < middle))
}

/// 幅のぶんだけを残すマスク（純粋ロジック）。
const fn width_mask(bits: u8) -> u32 {
    if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    }
}

/// 幅を明示して差を取る（純粋ロジック。テストのために分けてある）。
pub const fn elapsed_with_width(before: u32, after: u32, bits: u8) -> u32 {
    after.wrapping_sub(before) & width_mask(bits)
}

/// Local APIC タイマの周波数（Hz）を、減った数と PM タイマのティックから求める（純粋ロジック）。
///
/// **PIT 基準の式と同じ形である**——**ウィンドウの実時間で割る**（`crate::machine::pc::apic` の較正）。
/// **ティックが 0 なら 0 を返す**（割らない）。
pub const fn lapic_hz_from_ticks(lapic_counts: u64, pm_ticks: u64) -> u64 {
    if pm_ticks == 0 {
        return 0;
    }
    lapic_counts * HZ / pm_ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **幅で包み込む。** 24 ビットの一周を跨いだウィンドウでも、差はウィンドウの長さである。
    #[test]
    fn the_difference_wraps_at_the_declared_width() {
        // SAFETY: 作るだけで、ポートは読まない（`elapsed` は純粋な計算である）。以下のテストも同じ。
        let timer = unsafe { PmTimer::new(0x608, 24) };
        assert_eq!(timer.elapsed(10, 30), 20);
        assert_eq!(timer.elapsed(0xff_fff0, 0x10), 0x20, "24 ビットで一周した");
        // SAFETY: 上と同じ。
        let wide = unsafe { PmTimer::new(0x608, 32) };
        assert_eq!(
            wide.elapsed(0xff_fff0, 0x10),
            0xff00_0020,
            "32 ビットなら同じ値は一周していない"
        );
    }

    /// **幅の外のビットは差に混ぜない**（上位にゴミを返す機械でも、24 ビットぶんだけを見る）。
    #[test]
    fn bits_above_the_width_do_not_reach_the_difference() {
        // SAFETY: 作るだけで、ポートは読まない。
        let timer = unsafe { PmTimer::new(0x608, 24) };
        // 幅の外（上位 8 ビット）が違っていても、差は下位 24 ビットの差である。
        assert_eq!(timer.elapsed(0xdead_0010, 0xbeef_0030), 0x42_0020);
        assert_eq!(
            timer.elapsed(0xdead_0010, 0xbeef_0030),
            timer.elapsed(0xad_0010, 0xef_0030)
        );
    }

    /// **周波数はウィンドウの実時間で割る。**
    #[test]
    fn the_frequency_comes_from_the_observed_window() {
        // 100ms のウィンドウ（PM タイマのティック 357,954）で 1,000,000 数え下がれば約 10MHz。
        assert_eq!(lapic_hz_from_ticks(1_000_000, 357_954), 10_000_013);
        assert_eq!(lapic_hz_from_ticks(1_000_000, 0), 0, "割らない");
    }

    /// **3 回の読みの並び**（Linux の `acpi_pm_read_verified` と同じ判定）。一周をまたいだ並びは通す。
    #[test]
    fn three_reads_must_not_go_out_of_order() {
        assert!(reads_are_ordered(10, 20, 30));
        assert!(reads_are_ordered(10, 10, 10));
        assert!(reads_are_ordered(0xff_fffe, 0x1, 0x3), "一周をまたいだ");
        assert!(
            reads_are_ordered(0xff_fff0, 0xff_fff8, 0x2),
            "最後で一周した"
        );
        assert!(!reads_are_ordered(10, 5, 30), "真ん中だけが戻った");
        assert!(!reads_are_ordered(10, 40, 30), "真ん中だけが先へ飛んだ");
        assert!(!reads_are_ordered(10, 20, 15), "最後が戻った");
    }

    /// **2 回の読みの関係**（Linux の `init_acpi_pm_clocksource` と同じ判定）。
    #[test]
    fn a_decrease_counts_as_a_wrap_only_below_the_threshold() {
        assert_eq!(classify_change(5, 5), Change::Same);
        assert_eq!(classify_change(5, 6), Change::Forward);
        assert_eq!(classify_change(0xff_fff0, 0x10), Change::Wrapped);
        assert_eq!(
            classify_change(0xff_fff0, WRAP_THRESHOLD - 1),
            Change::Wrapped
        );
        assert_eq!(classify_change(0xff_fff0, WRAP_THRESHOLD), Change::Backward);
        assert_eq!(classify_change(0x8000, 0x7000), Change::Backward);
    }
}
