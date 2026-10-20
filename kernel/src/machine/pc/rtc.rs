//! CMOS の RTC（MC146818 の形）から、起動時の壁の時刻を読む（2026-10-20）。
//!
//! # 読み方
//!
//! **レジスタは 1 バイトずつ、番号を 0x70 に書いて 0x71 から読む。** 時刻は秒・分・時・日・月・年の 6 つと、ACPI の
//! FADT の `CENTURY` が示す世紀のレジスタ（QEMU は 0x32）である。
//!
//! - **更新中は読まない。** 状態 A の bit 7（UIP）が立っている間は、時刻の更新の最中か直前である。下りるまで待つ
//!   （[`UIP_POLLS`] 回まで）。
//! - **2 回続けて同じ値が読めるまで読み直す**（[`READ_ATTEMPTS`] 回まで）。UIP が下りた後に読み始めても、読み終わる
//!   までに秒が変わりうる（Linux の `mc146818_get_time` と同じ形）。
//! - **値の形は状態 B が決める。** bit 2 が立っていれば 2 進、落ちていれば BCD。bit 1 が立っていれば 24 時間、
//!   落ちていれば 12 時間で、時の bit 7 が午後を示す。
//! - **世紀のレジスタが無ければ 20xx とみなす。** 年は 2000〜2099 だけを受ける。
//!
//! **読めなかったら、壁の時計を持たない**——**0 や推した値では答えない**（`clock_gettime` の決まり）。理由は
//! [`RtcError`] で名指しする。
//!
//! # 純粋な部分と、ポートを叩く部分を分ける
//!
//! この中の関数は、レジスタを 1 つ読む関数を引数で受ける。**ホストのテストは偽の CMOS を渡して、更新中・読む途中の
//! 秒の繰り上がり・BCD・12 時間・世紀を固定する。** **ポートを叩くのは [`read_register`] の 1 関数だけである**
//! （`xtask` の直にポートを読み書きする所の許可の表に、この 1 行が載る）。
//!
//! # 起動時に 1 回だけ読む
//!
//! **BSP が、割り込みを許す直前（`start_timer` の sti の前）に 1 回読み、(秒, そのときの単調なティック) を控える**
//! （[`read_at_boot`]・[`boot_wall_clock`]）。壁の時計の今は、控えた秒に、そこからのティックの経過を足して求める
//! （`common::time::wall_clock_from`）。**読み直さない**——ティックの経過で進めるので、RTC の秒の切り捨て（0〜1 秒）
//! より大きくはずれない。**時計を合わせる道（`clock_settime`）は持たない**ので、壁の時計は飛ばない。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::arch::x86_64::port::{inb, outb};
use common::critical::InterruptGuard;
use common::time::{unix_seconds, CivilTime};

/// CMOS の番号のポート。**bit 7 は NMI を止めるビットで、立てない**（Linux と同じ）。
const INDEX_PORT: u16 = 0x70;
/// CMOS の値のポート。
const DATA_PORT: u16 = 0x71;

/// 秒のレジスタ。
pub const REGISTER_SECONDS: u8 = 0x00;
/// 分のレジスタ。
pub const REGISTER_MINUTES: u8 = 0x02;
/// 時のレジスタ。
pub const REGISTER_HOURS: u8 = 0x04;
/// 日のレジスタ。
pub const REGISTER_DAY: u8 = 0x07;
/// 月のレジスタ。
pub const REGISTER_MONTH: u8 = 0x08;
/// 年（下 2 桁）のレジスタ。
pub const REGISTER_YEAR: u8 = 0x09;
/// 状態 A。
pub const REGISTER_STATUS_A: u8 = 0x0A;
/// 状態 B。
pub const REGISTER_STATUS_B: u8 = 0x0B;
/// 時計のレジスタの最後（状態 D）。**世紀のレジスタは、これより後の CMOS RAM に在る。**
pub const LAST_CLOCK_REGISTER: u8 = 0x0D;
/// CMOS の番号の最後（番号の bit 7 は NMI を止めるビットで、番号ではない）。
pub const LAST_REGISTER: u8 = 0x7F;

/// 状態 A の bit 7: 更新の最中か直前である。
pub const STATUS_A_UPDATE_IN_PROGRESS: u8 = 1 << 7;
/// 状態 B の bit 1: 24 時間で持つ。
pub const STATUS_B_24_HOUR: u8 = 1 << 1;
/// 状態 B の bit 2: 2 進で持つ（落ちていれば BCD）。
pub const STATUS_B_BINARY: u8 = 1 << 2;
/// 12 時間で持つときの、時の bit 7: 午後。
pub const HOUR_PM: u8 = 1 << 7;

/// UIP が下りるまで、状態 A を読む回数の上限。**更新は 2 ms ほどで終わる**（UIP は更新の 244 µs 前に立ち、更新に
/// 最大 1984 µs かかる）。**1 回の読みは 1 µs より長い**ので、20,000 回は 20 ms より長い。
pub const UIP_POLLS: u32 = 20_000;

/// 2 回続けて同じ値を読もうとする回数の上限。**1 秒に 1 度しか変わらない**ので、2 回目で揃うのがふつうである。
pub const READ_ATTEMPTS: u32 = 5;

/// 読んだままの値（形の解釈の前）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawRtc {
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub day: u8,
    pub month: u8,
    pub year: u8,
    /// 世紀のレジスタが無ければ `None`。
    pub century: Option<u8>,
    pub status_b: u8,
}

/// 読めなかった理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcError {
    /// FADT の世紀のレジスタの番号が、時計のレジスタか CMOS の外を指している。
    CenturyRegisterOutOfRange { register: u8 },
    /// [`UIP_POLLS`] 回読んでも、UIP が下りなかった。
    UpdateNeverEnded,
    /// [`READ_ATTEMPTS`] 回読み直しても、2 回続けて同じ値にならなかった。
    NeverStable,
    /// BCD の形でない値（どちらかの桁が 10 以上）。
    NotBcd { register: u8, value: u8 },
    /// 範囲の外の日時（12 時間の 0 時や 13 時、2 月 30 日、年が 2000〜2099 の外、など）。
    OutOfRange { raw: RawRtc },
}

/// BCD の 1 バイトを 2 進へ直す。**どちらかの桁が 10 以上なら `None`。**
pub const fn from_bcd(value: u8) -> Option<u8> {
    let (high, low) = (value >> 4, value & 0x0F);
    if high > 9 || low > 9 {
        None
    } else {
        Some(high * 10 + low)
    }
}

/// 読んだままの値を、暦の日時へ直す（状態 B に従う）。
pub fn decode(raw: RawRtc) -> Result<CivilTime, RtcError> {
    let binary = raw.status_b & STATUS_B_BINARY != 0;
    let as_number = |value: u8, register: u8| -> Result<u8, RtcError> {
        if binary {
            Ok(value)
        } else {
            from_bcd(value).ok_or(RtcError::NotBcd { register, value })
        }
    };
    let out_of_range = RtcError::OutOfRange { raw };
    let twenty_four = raw.status_b & STATUS_B_24_HOUR != 0;
    let hour = if twenty_four {
        as_number(raw.hours, REGISTER_HOURS)?
    } else {
        // 12 時間: 1〜12 と午後のビット。12 時（午前）は 0 時、12 時（午後）は 12 時である。
        let hour = as_number(raw.hours & !HOUR_PM, REGISTER_HOURS)?;
        if !(1..=12).contains(&hour) {
            return Err(out_of_range);
        }
        hour % 12 + if raw.hours & HOUR_PM != 0 { 12 } else { 0 }
    };
    let century = match raw.century {
        Some(value) => as_number(value, 0)?,
        None => 20,
    };
    let year = u16::from(century) * 100 + u16::from(as_number(raw.year, REGISTER_YEAR)?);
    if !(2000..=2099).contains(&year) {
        return Err(out_of_range);
    }
    let civil = CivilTime {
        year,
        month: as_number(raw.month, REGISTER_MONTH)?,
        day: as_number(raw.day, REGISTER_DAY)?,
        hour,
        minute: as_number(raw.minutes, REGISTER_MINUTES)?,
        second: as_number(raw.seconds, REGISTER_SECONDS)?,
    };
    unix_seconds(civil).map_err(|_| out_of_range)?;
    Ok(civil)
}

/// UIP が下りるのを待ってから、全部のレジスタを 1 度読む。
fn read_once(
    read: &mut impl FnMut(u8) -> u8,
    century_register: Option<u8>,
) -> Result<RawRtc, RtcError> {
    let mut polls = 0;
    while read(REGISTER_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0 {
        polls += 1;
        if polls >= UIP_POLLS {
            return Err(RtcError::UpdateNeverEnded);
        }
    }
    Ok(RawRtc {
        seconds: read(REGISTER_SECONDS),
        minutes: read(REGISTER_MINUTES),
        hours: read(REGISTER_HOURS),
        day: read(REGISTER_DAY),
        month: read(REGISTER_MONTH),
        year: read(REGISTER_YEAR),
        century: century_register.map(&mut *read),
        status_b: read(REGISTER_STATUS_B),
    })
}

/// 2 回続けて同じ値が読めるまで読み、その値を返す（形はまだ解かない）。
///
/// `read` はレジスタの番号を受けてその値を返す。`century_register` は FADT の `CENTURY`（0 は `None` で渡す）。
pub fn read_stable(
    mut read: impl FnMut(u8) -> u8,
    century_register: Option<u8>,
) -> Result<RawRtc, RtcError> {
    if let Some(register) = century_register {
        if register <= LAST_CLOCK_REGISTER || register > LAST_REGISTER {
            return Err(RtcError::CenturyRegisterOutOfRange { register });
        }
    }
    let mut previous = read_once(&mut read, century_register)?;
    for _ in 0..READ_ATTEMPTS {
        let next = read_once(&mut read, century_register)?;
        if next == previous {
            return Ok(next);
        }
        previous = next;
    }
    Err(RtcError::NeverStable)
}

/// 読んで解き、Unix 秒にする（起動時の壁の時刻）。
pub fn read_unix_seconds(
    read: impl FnMut(u8) -> u8,
    century_register: Option<u8>,
) -> Result<u64, RtcError> {
    let raw = read_stable(read, century_register)?;
    let civil = decode(raw)?;
    unix_seconds(civil).map_err(|_| RtcError::OutOfRange { raw })
}

/// CMOS のレジスタを 1 つ読む。**ポートを叩く唯一の所である。**
///
/// **番号を書いて値を読む 2 手は、割り込みに挟まれてはならない**——挟まれた先が別の番号を書くと、違うレジスタを読む。
/// [`InterruptGuard`] の内側で行う。
pub fn read_register(register: u8) -> u8 {
    let _critical = InterruptGuard::enter();
    // SAFETY: 0x70 と 0x71 は PC の CMOS の番号と値のポートである（ACPI の FADT の `CENTURY` の欄も、この CMOS を前提に
    // している）。番号は 0〜0x7F に切り詰め、NMI を止める bit 7 は立てない。読むだけで、時計の設定は書かない。
    // 番号を書いてから値を読むまでは、この CPU では割り込みに挟まれない（上の `_critical`）。ほかの CPU は CMOS に
    // 触らない（読むのは BSP の起動時の 1 回だけである。[`read_at_boot`]）。
    unsafe {
        outb(INDEX_PORT, register & LAST_REGISTER);
        inb(DATA_PORT)
    }
}

/// 起動時の壁の時刻の秒と、読んだときの単調なティック（[`record_boot_wall_clock`] が書く）。
static BOOT_WALL_CLOCK_SECONDS: AtomicU64 = AtomicU64::new(0);
static BOOT_WALL_CLOCK_TICKS: AtomicU64 = AtomicU64::new(0);
/// 上の 2 つが書かれたか。**偽なら壁の時計は無い**（`CLOCK_REALTIME` は `-EINVAL`）。
static BOOT_WALL_CLOCK_KNOWN: AtomicBool = AtomicBool::new(false);

/// 起動時に RTC を 1 回読む。**読めたら (Unix 秒, 読んだままの値) を返し、控える。**
///
/// # 契約（境界の関数。2026-10-20）
///
/// - BSP が、起動時に 1 回だけ呼ぶ。ほかの CPU も、ユーザーのプロセスも、まだ壁の時計を読まない時点である。
/// - `century_register` は FADT の `CENTURY`（[`crate::machine::pc::acpi::FadtFacts`]）。
/// - 読めなければ何も控えない（[`boot_wall_clock`] は `None` のまま）。
pub fn read_at_boot(century_register: Option<u8>) -> Result<(u64, RawRtc), RtcError> {
    let raw = read_stable(read_register, century_register)?;
    let civil = decode(raw)?;
    let seconds = unix_seconds(civil).map_err(|_| RtcError::OutOfRange { raw })?;
    record_boot_wall_clock(seconds, crate::arch::x86_64::monotonic_ticks());
    Ok((seconds, raw))
}

fn record_boot_wall_clock(seconds: u64, ticks: u64) {
    BOOT_WALL_CLOCK_SECONDS.store(seconds, Ordering::Relaxed);
    BOOT_WALL_CLOCK_TICKS.store(ticks, Ordering::Relaxed);
    // **値を書いてから、書いたことを公開する。**
    BOOT_WALL_CLOCK_KNOWN.store(true, Ordering::Release);
}

/// 起動時の壁の時刻（Unix 秒, 読んだときの単調なティック）。**読めなかったか、まだ読んでいなければ `None`。**
///
/// # 契約（境界の関数。2026-10-20）
///
/// - どの CPU からも、BKL なしで呼んでよい（アトミックを読むだけ）。
/// - 一度 `Some` になったら、値は変わらない。
pub fn boot_wall_clock() -> Option<(u64, u64)> {
    BOOT_WALL_CLOCK_KNOWN.load(Ordering::Acquire).then(|| {
        (
            BOOT_WALL_CLOCK_SECONDS.load(Ordering::Relaxed),
            BOOT_WALL_CLOCK_TICKS.load(Ordering::Relaxed),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTURY: u8 = 0x32;

    /// 偽の CMOS。**2026-10-20 09:30:15 を BCD・24 時間で持つ**（QEMU の既定の形）。
    fn cmos() -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[REGISTER_SECONDS as usize] = 0x15;
        bytes[REGISTER_MINUTES as usize] = 0x30;
        bytes[REGISTER_HOURS as usize] = 0x09;
        bytes[REGISTER_DAY as usize] = 0x20;
        bytes[REGISTER_MONTH as usize] = 0x10;
        bytes[REGISTER_YEAR as usize] = 0x26;
        bytes[REGISTER_STATUS_B as usize] = STATUS_B_24_HOUR;
        bytes[CENTURY as usize] = 0x20;
        bytes
    }

    /// `date -u -d '2026-10-20 09:30:15' +%s`。
    const EXPECTED: u64 = 1_792_488_615;

    #[test]
    fn a_bcd_24_hour_clock_is_read() {
        let bytes = cmos();
        assert_eq!(
            read_unix_seconds(|register| bytes[register as usize], Some(CENTURY)),
            Ok(EXPECTED)
        );
    }

    #[test]
    fn a_binary_clock_is_read() {
        let mut bytes = cmos();
        bytes[REGISTER_SECONDS as usize] = 15;
        bytes[REGISTER_MINUTES as usize] = 30;
        bytes[REGISTER_HOURS as usize] = 9;
        bytes[REGISTER_DAY as usize] = 20;
        bytes[REGISTER_MONTH as usize] = 10;
        bytes[REGISTER_YEAR as usize] = 26;
        bytes[CENTURY as usize] = 20;
        bytes[REGISTER_STATUS_B as usize] = STATUS_B_24_HOUR | STATUS_B_BINARY;
        assert_eq!(
            read_unix_seconds(|register| bytes[register as usize], Some(CENTURY)),
            Ok(EXPECTED)
        );
    }

    /// 12 時間: 午前 12 時は 0 時、午後 12 時は 12 時、午後 9 時は 21 時。
    #[test]
    fn a_12_hour_clock_is_turned_into_24_hours() {
        let mut bytes = cmos();
        bytes[REGISTER_STATUS_B as usize] = 0;
        for (hours, want) in [
            (0x12, 0),
            (0x12 | HOUR_PM, 12),
            (0x09, 9),
            (0x09 | HOUR_PM, 21),
        ] {
            bytes[REGISTER_HOURS as usize] = hours;
            let civil =
                decode(read_stable(|register| bytes[register as usize], Some(CENTURY)).unwrap());
            assert_eq!(civil.unwrap().hour, want, "hours {hours:#x}");
        }
        // 12 時間の 0 時と 13 時は無い。
        for hours in [0x00, 0x13] {
            bytes[REGISTER_HOURS as usize] = hours;
            let raw = read_stable(|register| bytes[register as usize], Some(CENTURY)).unwrap();
            assert_eq!(
                decode(raw),
                Err(RtcError::OutOfRange { raw }),
                "hours {hours:#x}"
            );
        }
    }

    #[test]
    fn without_a_century_register_the_century_is_20() {
        let bytes = cmos();
        assert_eq!(
            read_unix_seconds(|register| bytes[register as usize], None),
            Ok(EXPECTED)
        );
    }

    /// **UIP が立っている間は読まない。** 下りてから読んだ値だけを使う。
    #[test]
    fn the_update_in_progress_flag_is_waited_for() {
        let bytes = cmos();
        let mut status_reads = 0;
        let result = read_unix_seconds(
            |register| {
                if register == REGISTER_STATUS_A {
                    status_reads += 1;
                    // 最初の 3 回は更新の最中。
                    if status_reads <= 3 {
                        return STATUS_A_UPDATE_IN_PROGRESS;
                    }
                }
                bytes[register as usize]
            },
            Some(CENTURY),
        );
        assert_eq!(result, Ok(EXPECTED));
        assert!(status_reads > 3);
    }

    #[test]
    fn an_update_that_never_ends_is_named() {
        let bytes = cmos();
        assert_eq!(
            read_unix_seconds(
                |register| if register == REGISTER_STATUS_A {
                    STATUS_A_UPDATE_IN_PROGRESS
                } else {
                    bytes[register as usize]
                },
                Some(CENTURY)
            ),
            Err(RtcError::UpdateNeverEnded)
        );
    }

    /// **読む途中で秒が繰り上がっても、2 回続けて揃った値を使う。** 1 回目の読みは 09:30:15、2 回目からは
    /// 09:30:16 である（1 回目の値は捨てる）。
    #[test]
    fn a_second_that_turns_over_while_reading_is_read_again() {
        let mut bytes = cmos();
        let mut full_reads = 0;
        let result = read_unix_seconds(
            |register| {
                if register == REGISTER_STATUS_B {
                    full_reads += 1;
                    // 1 回目の読みの終わりで、秒が進む。
                    if full_reads == 1 {
                        bytes[REGISTER_SECONDS as usize] = 0x16;
                    }
                }
                bytes[register as usize]
            },
            Some(CENTURY),
        );
        assert_eq!(result, Ok(EXPECTED + 1));
    }

    #[test]
    fn values_that_never_settle_are_named() {
        let mut bytes = cmos();
        let mut tick = 0u8;
        assert_eq!(
            read_stable(
                |register| {
                    if register == REGISTER_SECONDS {
                        tick = (tick + 1) % 10;
                        bytes[REGISTER_SECONDS as usize] = tick;
                    }
                    bytes[register as usize]
                },
                Some(CENTURY)
            ),
            Err(RtcError::NeverStable)
        );
    }

    #[test]
    fn a_value_that_is_not_bcd_is_named() {
        let mut bytes = cmos();
        bytes[REGISTER_MINUTES as usize] = 0x5A;
        let raw = read_stable(|register| bytes[register as usize], Some(CENTURY)).unwrap();
        assert_eq!(
            decode(raw),
            Err(RtcError::NotBcd {
                register: REGISTER_MINUTES,
                value: 0x5A
            })
        );
    }

    #[test]
    fn dates_out_of_range_are_refused() {
        for (register, value) in [
            (REGISTER_MONTH, 0x13),
            (REGISTER_DAY, 0x31),
            (REGISTER_MINUTES, 0x60),
            (CENTURY, 0x21),
            (CENTURY, 0x19),
        ] {
            let mut bytes = cmos();
            // 11 月は 30 日まで。
            bytes[REGISTER_MONTH as usize] = if register == REGISTER_DAY { 0x11 } else { 0x10 };
            bytes[register as usize] = value;
            let raw = read_stable(|r| bytes[r as usize], Some(CENTURY)).unwrap();
            assert_eq!(
                decode(raw),
                Err(RtcError::OutOfRange { raw }),
                "register {register:#x} = {value:#x}"
            );
        }
    }

    /// **世紀のレジスタが時計のレジスタか CMOS の外を指していたら、読まずに断る。**
    #[test]
    fn a_century_register_inside_the_clock_or_outside_cmos_is_refused() {
        let bytes = cmos();
        for register in [0x00, LAST_CLOCK_REGISTER, 0x80, 0xFF] {
            assert_eq!(
                read_stable(|r| bytes[(r & LAST_REGISTER) as usize], Some(register)),
                Err(RtcError::CenturyRegisterOutOfRange { register })
            );
        }
    }

    #[test]
    fn bcd_is_turned_into_binary() {
        assert_eq!(from_bcd(0x00), Some(0));
        assert_eq!(from_bcd(0x59), Some(59));
        assert_eq!(from_bcd(0x99), Some(99));
        assert_eq!(from_bcd(0x0A), None);
        assert_eq!(from_bcd(0xA0), None);
    }
}
