//! 実行ファイルの「範囲を読む口」（2026-10-05）。
//!
//! # なぜ在るのか
//!
//! **プログラムを載せる側は、以前、実行ファイルの全体を 1 本のバイト列として受け取っていた。** ファイルシステムの
//! 上のファイルは、ブロックが続いているとは限らないので、載せる前に、静的な配列へ全体を写していた（上限は
//! 32 KiB）。**数 MiB の実行ファイルでは、その形は取れない。**
//!
//! **載せる側が要るのは、全体ではなく範囲である**——先頭の部分（ELF のヘッダとプログラムヘッダ）と、区画ごとの、
//! ページに写す分だけ。**この口は「どこから何バイト」を受けて、渡されたバッファへ写す。** 全体を持つ者は居ない。
//!
//! # 実装は 2 つ
//!
//! - [`SliceImage`]——メモリに在るバイト列（カーネルに埋め込んだプログラムと、ホストの試験）。
//! - `crate::ext2::FileImage`——ext2 の上のファイル。ブロックごとに引いて写す。
//!
//! # 契約
//!
//! - **範囲がファイルの外へ出るなら、1 バイトも写さずに断る**（[`ImageReadError::OutOfRange`]）。黙って短く読まない。
//! - **読めなかったら、理由を名指しして返す。** 途中まで写したバッファの中身は、当てにしてはならない。

use crate::ext2::Ext2Error;

/// 範囲を読めなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageReadError {
    /// 範囲がファイルの外へ出る（`offset + len > file_len`。足し算があふれる場合も）。
    OutOfRange {
        offset: u64,
        len: u64,
        file_len: u64,
    },
    /// ファイルシステムが、そのブロックを読めなかった。**2 段目の間接ブロックを使う大きなファイルは、ここで
    /// `IndirectBlockUnsupported` として断られる**（黙って切らない）。
    Ext2(Ext2Error),
    /// 引けたブロックが、要る長さより短い（ファイルの長さと、ブロックの並びが食い違っている）。
    ShortBlock { index: u32 },
}

/// 実行ファイルの範囲を読む口（モジュールの doc）。
pub trait ImageSource {
    /// ファイルの長さ（バイト）。
    fn len(&self) -> u64;

    /// 長さが 0 か。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `offset` から `buf.len()` バイトを `buf` へ写す。**範囲がファイルの外なら、写さずに断る。**
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ImageReadError>;
}

/// 範囲がファイルの中に収まるかを確かめる（純粋な論理。実装が共通で使う）。
pub fn check_range(offset: u64, len: usize, file_len: u64) -> Result<(), ImageReadError> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= file_len => Ok(()),
        _ => Err(ImageReadError::OutOfRange {
            offset,
            len: len as u64,
            file_len,
        }),
    }
}

/// メモリに在るバイト列を、範囲を読む口として見せる。
#[derive(Debug, Clone, Copy)]
pub struct SliceImage<'a>(pub &'a [u8]);

impl ImageSource for SliceImage<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ImageReadError> {
        check_range(offset, buf.len(), self.len())?;
        let start = offset as usize;
        buf.copy_from_slice(&self.0[start..start + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 範囲の中は、そのまま写る。端ちょうどまで読める。
    #[test]
    fn a_slice_image_reads_any_range_inside_it() {
        let bytes: [u8; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let image = SliceImage(&bytes);
        assert_eq!(image.len(), 10);
        let mut buf = [0u8; 4];
        image.read_at(3, &mut buf).unwrap();
        assert_eq!(buf, [3, 4, 5, 6]);
        image.read_at(6, &mut buf).unwrap();
        assert_eq!(buf, [6, 7, 8, 9]);
        let mut none = [0u8; 0];
        image.read_at(10, &mut none).unwrap();
    }

    /// 範囲がファイルの外へ出るなら、**1 バイトも写さずに断る。** 足し算があふれる位置も断る。
    #[test]
    fn a_range_past_the_end_is_refused_without_copying() {
        let bytes = [7u8; 10];
        let image = SliceImage(&bytes);
        let mut buf = [0xAAu8; 4];
        assert_eq!(
            image.read_at(7, &mut buf),
            Err(ImageReadError::OutOfRange {
                offset: 7,
                len: 4,
                file_len: 10
            })
        );
        assert_eq!(buf, [0xAA; 4], "nothing was copied");
        assert!(image.read_at(u64::MAX, &mut buf).is_err());
        assert!(image.read_at(11, &mut [0u8; 0]).is_err());
    }
}
