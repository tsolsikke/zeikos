//! `AT_RANDOM` に渡す 16 バイトの出所（2026-10-06）。**暗号に使える乱数ではない。**
//!
//! Linux は、プロセスの初めのスタックに 16 バイトの乱数を置き、補助ベクタの `AT_RANDOM` で指す。libc は、それを
//! スタックの見張りの値（カナリア）の種などに使う。**このカーネルは、まだ乱数の仕組み（エントロピーをためて混ぜる
//! 所）を持たない。** プログラムが起動できるように、手元で取れる値を渡す。
//!
//! - **CPU が `RDRAND` を持てば、それを使う**（CPUID の葉 1 の ECX の 30 番のビット）。
//! - **持たなければ、タイムスタンプカウンタ（TSC）を、呼ぶたびに進む数と混ぜた値を使う。** 起動のたびに変わるが、
//!   予測できない値ではない。
//!
//! **どちらの出所でも、鍵を作るような用途には足りない**——`RDRAND` の値をそのまま使っていて、ほかの出所と混ぜて
//! いないし、後から混ぜ直すこともしない。`docs/deferred-decisions.md` の「`AT_RANDOM` の 16 バイトが、暗号に使える
//! 乱数ではない」に、決めどきを書いてある。

use core::sync::atomic::{AtomicU64, Ordering};

/// 16 バイトの出所。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandomSource {
    /// CPU の `RDRAND` 命令。
    Rdrand,
    /// タイムスタンプカウンタを、呼ぶたびに進む数と混ぜた値。
    TimeStampCounter,
}

impl core::fmt::Display for RandomSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RandomSource::Rdrand => write!(f, "RDRAND"),
            RandomSource::TimeStampCounter => {
                write!(f, "the time stamp counter mixed with a call counter")
            }
        }
    }
}

/// CPU が `RDRAND` を持つか（CPUID の葉 1 の ECX の 30 番のビット）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - CPUID を読むだけで、何も変えない。
pub fn has_rdrand() -> bool {
    // **`unsafe` は要らない**——`__cpuid` は x86_64 では safe fn である（葉 1 はどの CPU にも在る）。
    rdrand_bit(core::arch::x86_64::__cpuid(1).ecx)
}

/// 葉 1 の ECX から、`RDRAND` のビットを読む（純粋な論理）。
const fn rdrand_bit(leaf_one_ecx: u32) -> bool {
    leaf_one_ecx & (1 << 30) != 0
}

/// `RDRAND` を 1 度打つ。**取れなければ `None`**（命令は、値を用意できないときに CF を落とす）。
///
/// # Safety
///
/// CPU が `RDRAND` を持つこと（[`has_rdrand`]）。持たない CPU では、命令が未定義で #UD になる。
unsafe fn rdrand_once() -> Option<u64> {
    let value: u64;
    let ok: u8;
    // SAFETY: 呼び出し元の契約により、この CPU は RDRAND を持つ。命令はレジスタとフラグを書くだけで、メモリにも
    // スタックにも触らない。
    unsafe {
        core::arch::asm!(
            "rdrand {value}",
            "setc {ok}",
            value = out(reg) value,
            ok = out(reg_byte) ok,
            options(nomem, nostack),
        );
    }
    (ok != 0).then_some(value)
}

/// `RDRAND` を、取れるまで打つ回数の上限（Intel の手引きが 10 回としている）。
const RDRAND_ATTEMPTS: usize = 10;

/// 呼ぶたびに進む数（TSC と混ぜる。同じ TSC の値が 2 度読めても、同じ 16 バイトにならないようにする）。
static CALLS: AtomicU64 = AtomicU64::new(0);

/// 64 ビットの値を混ぜる（SplitMix64 の仕上げの段。純粋な論理）。**暗号の強さは無い。** 近い入力を、離れた出力に
/// するだけである。
const fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// TSC と、呼んだ回数から、16 バイトを作る（純粋な論理）。
const fn bytes_from_counter(time_stamp: u64, call: u64) -> [u8; 16] {
    let low = mix(time_stamp ^ call.rotate_left(32)).to_le_bytes();
    let high = mix(mix(time_stamp).wrapping_add(call)).to_le_bytes();
    let mut bytes = [0u8; 16];
    let mut index = 0;
    while index < 8 {
        bytes[index] = low[index];
        bytes[8 + index] = high[index];
        index += 1;
    }
    bytes
}

/// `AT_RANDOM` に渡す 16 バイトと、その出所。**暗号に使える値ではない**（モジュールの doc）。
///
/// # 契約（境界の関数。2026-10-06）
///
/// - CPU の命令（`RDRAND` か `RDTSC`）を打ち、呼んだ回数を 1 つ進める。ほかは何も変えない。どの文脈から呼んでもよい。
/// - `RDRAND` が続けて取れなかったときは、TSC の側へ落ちる（返す出所が、そう言う）。
pub fn weak_random_bytes() -> ([u8; 16], RandomSource) {
    if has_rdrand() {
        let mut words = [0u64; 2];
        let mut filled = 0;
        for _ in 0..RDRAND_ATTEMPTS {
            // SAFETY: 上で、この CPU が RDRAND を持つことを確かめた。
            if let Some(value) = unsafe { rdrand_once() } {
                words[filled] = value;
                filled += 1;
                if filled == words.len() {
                    let mut bytes = [0u8; 16];
                    bytes[..8].copy_from_slice(&words[0].to_le_bytes());
                    bytes[8..].copy_from_slice(&words[1].to_le_bytes());
                    return (bytes, RandomSource::Rdrand);
                }
            }
        }
    }
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `RDTSC` は Ring 0 でいつでも打てる（CR4.TSD は Ring 3 だけを縛る）。レジスタを書くだけである。
    let time_stamp = unsafe { core::arch::x86_64::_rdtsc() };
    (
        bytes_from_counter(time_stamp, call),
        RandomSource::TimeStampCounter,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RDRAND` のビットは、葉 1 の ECX の 30 番である。
    #[test]
    fn the_rdrand_bit_is_bit_thirty_of_leaf_one_ecx() {
        assert!(rdrand_bit(1 << 30));
        assert!(!rdrand_bit(!(1 << 30)));
        assert!(!rdrand_bit(0));
    }

    /// TSC の側の 16 バイトは、TSC か回数のどちらかが違えば変わる。同じ入力なら同じである（混ぜるだけで、
    /// 隠れた状態を持たない）。
    #[test]
    fn the_counter_bytes_change_with_either_input() {
        let base = bytes_from_counter(0x1234_5678, 0);
        assert_eq!(base, bytes_from_counter(0x1234_5678, 0));
        assert_ne!(base, bytes_from_counter(0x1234_5678, 1));
        assert_ne!(base, bytes_from_counter(0x1234_5679, 0));
        // 前半と後半が同じ 8 バイトにならない（2 つの語を、別の式で作っている）。
        assert_ne!(base[..8], base[8..]);
        // 0 と 0 からでも、0 だけの 16 バイトにはならない。
        assert_ne!(bytes_from_counter(0, 0), [0u8; 16]);
    }
}
