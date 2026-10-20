//! 単調な時刻とティックの換算（W2-d+。`ADR-0062`）。
//!
//! **ハードに依らない純粋な論理である**——**周波数は引数で受け取る。** **カーネルの
//! `clock_gettime` と `nanosleep` が使い、ホストのテストで固定する**
//! （`CLAUDE.md` の「ハードウェア依存部と純粋ロジックを分離」）。

/// 1 秒のナノ秒。
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// `timespec` の中身が範囲の外である（`nanosleep` が `-EINVAL` を返す場合）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTimespec;

/// ティック数を秒とナノ秒へ換算する（`clock_gettime` の答え）。
///
/// **ナノ秒は常に `NANOS_PER_SECOND` 未満である。**
pub fn timespec_from_ticks(ticks: u64, hz: u64) -> (u64, u64) {
    let seconds = ticks / hz;
    let nanos = (ticks % hz) * (NANOS_PER_SECOND / hz);
    (seconds, nanos)
}

/// 秒とナノ秒を、待つティック数へ換算する（`nanosleep` の締切）。
///
/// # 切り上げる
///
/// **求められた長さより短く眠ってはいけない**（POSIX の `nanosleep` の約束）。
/// **1 ティックに満たない端数は 1 ティックへ切り上げる。**
///
/// # 範囲
///
/// **秒が負、またはナノ秒が 0 未満か `NANOS_PER_SECOND` 以上なら範囲の外である**
/// （Linux の `nanosleep(2)` が `EINVAL` を返す条件。`timespec64_valid`）。
///
/// # 大きすぎる長さは切り詰める
///
/// **ティック数が `u64` に収まらない長さは、`u64::MAX`（事実上いつまでも）に切り詰める**（2026-10-09）。Linux は、
/// 範囲の内の大きすぎる秒を `KTIME_MAX` に切り詰めて眠る（`timespec64_to_ktime`）。**以前は掛け算があふれる長さを
/// 範囲の外として `EINVAL` にしていた**——100 Hz では、秒が約 1.8 × 10^17 を越えると Linux と答えが違った。
pub fn ticks_for_duration(seconds: i64, nanos: i64, hz: u64) -> Result<u64, InvalidTimespec> {
    if seconds < 0 || nanos < 0 || nanos as u64 >= NANOS_PER_SECOND {
        return Err(InvalidTimespec);
    }
    let nanos_per_tick = NANOS_PER_SECOND / hz;
    let whole = (seconds as u64).saturating_mul(hz);
    let partial = (nanos as u64).div_ceil(nanos_per_tick);
    Ok(whole.saturating_add(partial))
}

/// 暦の日時（UTC。壁の時計の出所が読んだ値）。**どの装置から読んだかによらない形である。**
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CivilTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// 暦の日時が範囲の外である（月が 13、2 月 30 日、など）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidCivilTime;

/// その年が閏年か（グレゴリオ暦）。
pub const fn is_leap_year(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// その月の日数。**`month` は 1〜12 であること。**
pub const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 暦の日時を、1970-01-01 00:00:00 UTC からの秒へ直す（`CLOCK_REALTIME` の秒）。
///
/// **範囲を先に確かめる**——月 1〜12、日 1〜その月の日数、時 0〜23、分と秒 0〜59、年 1970 以上。**閏秒（60 秒）は
/// 受けない**（Linux の `CLOCK_REALTIME` も閏秒を数えない）。日数は、1970 年からの年の日数と、その年の月の日数を
/// 足して求める（年は 4 桁までなので、足し算の回数は小さい）。
pub fn unix_seconds(civil: CivilTime) -> Result<u64, InvalidCivilTime> {
    let CivilTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = civil;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(InvalidCivilTime);
    }
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += u64::from(days_in_month(year, m));
    }
    days += u64::from(day - 1);
    Ok(days * 86_400 + u64::from(hour) * 3_600 + u64::from(minute) * 60 + u64::from(second))
}

/// 壁の時計の今（`CLOCK_REALTIME` の秒とナノ秒）。**起動時に 1 度読んだ秒に、そのときからのティックの経過を足す。**
///
/// `ticks_now` が `ticks_at_read` より小さいことは無い（単調なティックである）。**万一小さければ、経過を 0 とする**
/// ——戻る向きには答えない。
pub fn wall_clock_from(
    seconds_at_read: u64,
    ticks_at_read: u64,
    ticks_now: u64,
    hz: u64,
) -> (u64, u64) {
    let (elapsed_seconds, nanos) = timespec_from_ticks(ticks_now.saturating_sub(ticks_at_read), hz);
    (seconds_at_read.saturating_add(elapsed_seconds), nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn civil(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> CivilTime {
        CivilTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// 値は GNU の `date -u -d '<日時>' +%s` で確かめたもの。
    #[test]
    fn civil_times_become_unix_seconds() {
        assert_eq!(unix_seconds(civil(1970, 1, 1, 0, 0, 0)), Ok(0));
        assert_eq!(unix_seconds(civil(2000, 1, 1, 0, 0, 0)), Ok(946_684_800));
        assert_eq!(unix_seconds(civil(2000, 2, 29, 12, 0, 0)), Ok(951_825_600));
        assert_eq!(
            unix_seconds(civil(2026, 10, 20, 9, 30, 15)),
            Ok(1_792_488_615)
        );
        assert_eq!(
            unix_seconds(civil(2099, 12, 31, 23, 59, 59)),
            Ok(4_102_444_799)
        );
    }

    #[test]
    fn leap_years_follow_the_gregorian_rule() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2100));
        assert!(!is_leap_year(2026));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 12), 31);
    }

    #[test]
    fn civil_times_out_of_range_are_refused() {
        for bad in [
            civil(1969, 12, 31, 23, 59, 59),
            civil(2026, 0, 1, 0, 0, 0),
            civil(2026, 13, 1, 0, 0, 0),
            civil(2026, 2, 29, 0, 0, 0),
            civil(2026, 4, 31, 0, 0, 0),
            civil(2026, 1, 0, 0, 0, 0),
            civil(2026, 1, 1, 24, 0, 0),
            civil(2026, 1, 1, 0, 60, 0),
            civil(2026, 1, 1, 0, 0, 60),
        ] {
            assert_eq!(unix_seconds(bad), Err(InvalidCivilTime), "{bad:?}");
        }
    }

    #[test]
    fn the_wall_clock_adds_the_ticks_since_the_read() {
        assert_eq!(
            wall_clock_from(1_792_488_615, 500, 500, 100),
            (1_792_488_615, 0)
        );
        assert_eq!(
            wall_clock_from(1_792_488_615, 500, 1_234, 100),
            (1_792_488_622, 340_000_000)
        );
        // 経過が戻る形は起きないが、起きても戻る向きには答えない。
        assert_eq!(
            wall_clock_from(1_792_488_615, 500, 400, 100),
            (1_792_488_615, 0)
        );
    }

    #[test]
    fn ticks_become_seconds_and_nanos() {
        assert_eq!(timespec_from_ticks(0, 100), (0, 0));
        assert_eq!(timespec_from_ticks(4382, 100), (43, 820_000_000));
        assert_eq!(timespec_from_ticks(99, 100), (0, 990_000_000));
    }

    #[test]
    fn whole_seconds_become_exact_ticks() {
        assert_eq!(ticks_for_duration(1, 0, 100), Ok(100));
        assert_eq!(ticks_for_duration(0, 0, 100), Ok(0));
    }

    #[test]
    fn a_partial_tick_rounds_up_so_the_sleep_is_never_short() {
        assert_eq!(ticks_for_duration(0, 1, 100), Ok(1));
        assert_eq!(ticks_for_duration(0, 10_000_000, 100), Ok(1));
        assert_eq!(ticks_for_duration(0, 10_000_001, 100), Ok(2));
        assert_eq!(ticks_for_duration(0, 500_000_000, 100), Ok(50));
    }

    #[test]
    fn out_of_range_values_are_refused() {
        assert_eq!(ticks_for_duration(-1, 0, 100), Err(InvalidTimespec));
        assert_eq!(ticks_for_duration(0, -1, 100), Err(InvalidTimespec));
        assert_eq!(
            ticks_for_duration(0, NANOS_PER_SECOND as i64, 100),
            Err(InvalidTimespec)
        );
    }

    /// **大きすぎる長さは `-EINVAL` にせず、`u64::MAX` に切り詰める**（2026-10-09。Linux の `KTIME_MAX`）。掛け算の
    /// あふれと、足し算のあふれの両方。あふれない最大の秒は、そのまま換算する。
    #[test]
    fn durations_too_long_for_the_tick_count_are_clamped() {
        assert_eq!(ticks_for_duration(i64::MAX, 0, 100), Ok(u64::MAX));
        assert_eq!(ticks_for_duration(i64::MAX, 999_999_999, 100), Ok(u64::MAX));
        let largest = (u64::MAX / 100) as i64;
        assert_eq!(
            ticks_for_duration(largest, 0, 100),
            Ok(largest as u64 * 100)
        );
        assert_eq!(ticks_for_duration(largest + 1, 0, 100), Ok(u64::MAX));
        // 掛け算はあふれず、端数の足し算であふれる形。
        assert_eq!(ticks_for_duration(largest, 999_999_999, 100), Ok(u64::MAX));
        // 負の秒と範囲の外のナノ秒は、大きさによらず `-EINVAL` のまま。
        assert_eq!(ticks_for_duration(i64::MIN, 0, 100), Err(InvalidTimespec));
        assert_eq!(ticks_for_duration(i64::MAX, -1, 100), Err(InvalidTimespec));
    }
}
