//! プロセスの初めのスタックの形（`argc`・`argv`・`envp`・`auxv`。Linux と同じ形。S11-1）。
//!
//! **CPU によらない**——x86_64 も aarch64 も、entry の時点のスタックの指す先から、`argc`、`argv` のポインタ、NULL、
//! `envp` のポインタ、NULL、`auxv` の対と並ぶ（2026-09-30 に確かめた。`crt1.o` の `_start` は両方とも `argc` をスタック
//! の指す先から、`argv` をその 8 バイト上から読む。`libc.a` の `__libc_start_main` は両方とも `envp` を
//! `argv + argc + 1` とし、`envp` の NULL の次を `auxv` として読む。`AT_NULL` と `AT_PHDR` の値は `linux/auxvec.h` で
//! 同じ）。**16 バイト整列**: aarch64 の `_start` はスタックを整列し直さずに使い、x86_64 の `_start` は整列し直す。
//! 16 に揃えて積めば、どちらでも足りる。
//!
//! `ADR-0071` の決定 1 の 2 で、`crate::userland` の `build_initial_stack` から分けた（2026-09-30）。**`argv` と `envp` の
//! 数の上限を見るのと、スタックページを切り出すのは共通の側である。**

/// 補助ベクタ（`auxv`）に積む値の出所（2026-10-06）。**載せる側が、載せた結果から作って渡す。**
///
/// **Linux の静的なプログラム（musl の `_start` と `__libc_start_main`）が読むものを積む。** 位置独立の像は、
/// `AT_PHDR` から自分の載った位置を求めて、自分で再配置する。`AT_RANDOM` は、スタックの見張りの値の種になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Auxv<'a> {
    /// プログラムヘッダの表の番地（ずらした後）。**表がどの区画にも載らない像では `None`**——その像には
    /// `AT_PHDR` を積まない（番地の無いものを 0 として渡さない）。
    pub program_headers_at: Option<u64>,
    /// プログラムヘッダの数（`AT_PHNUM`）。
    pub program_header_count: u16,
    /// 入口の番地（ずらした後。`AT_ENTRY`）。
    pub entry: u64,
    /// `AT_RANDOM` が指す 16 バイト。**出所は載せる側が決める。暗号に使える値ではない**（`ADR-0074` の範囲の外。
    /// `docs/deferred-decisions.md` の「`AT_RANDOM` の 16 バイトが、暗号に使える乱数ではない」）。
    pub random: [u8; 16],
    /// `AT_EXECFN` が指す文字列（実行したファイルの名前）。
    pub exec_name: &'a [u8],
}

/// `auxv` の型の番号（`linux/auxvec.h`。x86_64 と aarch64 で同じ値）。
mod at {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const EXECFN: u64 = 31;
}

/// プログラムヘッダ 1 つの大きさ（`AT_PHENT`。64 ビットの ELF では 56）。
const PROGRAM_HEADER_SIZE: u64 = 56;
/// ページの大きさ（`AT_PAGESZ`）。
const PAGE_SIZE: u64 = 4096;
/// `AT_RANDOM` が指すバイト数。
const RANDOM_BYTES: usize = 16;

/// ユーザープログラムの初期スタックを **Linux と同じ形で**積む（S11-1）。**収まらなければ `None`。**
///
/// `page` はスタックの 1 ページ、`page_base` はその先頭の利用者の仮想アドレスである（文字列を指すポインタに使う）。
/// 返すのは entry へ入るときのスタックの位置（`argc` を指す）。
///
/// # 形は実測で確かめた
///
/// **ホストで、`_start` から `rsp` をたどる自作の静的バイナリを走らせて観測した。**
/// `rsp` の指す先から順に——`argc`、`argv` のポインタ、NULL、`envp` のポインタ、
/// NULL、そして `auxv` の `(type, value)` の対が続き、`type == 0`（`AT_NULL`）で
/// 終わる。**文字列そのものはこの表より上（高位）に置かれる。**
///
/// # 何を積むか
///
/// **`argc`・`argv`・`envp` と、`auxv` を積む**（`auxv` の中身は 2026-10-06 に足した。それまでは終端だけだった）。
///
/// `auxv` は、この順である——`AT_PHDR`（表が載っている像だけ）・`AT_PHENT`・`AT_PHNUM`・`AT_PAGESZ`・`AT_ENTRY`・
/// `AT_UID`・`AT_EUID`・`AT_GID`・`AT_EGID`（どれも 0。利用者の区別は、まだ無い）・`AT_SECURE`（0）・`AT_RANDOM`・
/// `AT_EXECFN`・`AT_NULL`。**積んでいないもの**——`AT_HWCAP`・`AT_CLKTCK`・`AT_BASE`・`AT_SYSINFO_EHDR`
/// （vDSO は無い）。要る利用者が出たら足す。
///
/// # 16 バイト整列
///
/// **`rsp` は entry の時点で 16 の倍数である**（SysV の規約。Linux もそう積む）。
/// **詰め物は表と文字列の間に入る。**
///
/// # 文字列の位置
///
/// **文字列を上から詰めた後、同じ順にたどり直して位置を求める**——控えの配列を持たない（`argv` と `envp` の数の
/// 上限は共通の側が見る）。**`argv` と `envp` の文字列の下に、`AT_EXECFN` の文字列、`AT_RANDOM` の 16 バイトの順で
/// 置く。**
pub fn build_initial_stack(
    page: &mut [u8],
    page_base: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &Auxv<'_>,
) -> Option<u64> {
    /// 表の項の大きさ。
    const WORD: usize = 8;
    /// 表の固定部——`argc`・`argv` の終端・`envp` の終端。
    ///
    /// **`argv` と `envp` の本体と、`auxv` の対は、ここに入らない。** 下で数を足す。
    const FIXED_WORDS: usize = 1 + 1 + 1;

    let mut cursor = page.len();

    // **文字列を上から詰める。**
    //
    // **`argv` と `envp` を同じ手順で詰める（EV）。** **並びの上では
    // `argv` の表が先に来るが、文字列の置き場に順序の要求は無い**
    // ——ポインタで指すためである。
    for item in argv.iter().chain(envp) {
        // NUL 終端のぶんを含めて下げる。
        cursor = cursor.checked_sub(item.len() + 1)?;
        page[cursor..cursor + item.len()].copy_from_slice(item);
        page[cursor + item.len()] = 0;
    }
    // **`AT_EXECFN` の文字列と、`AT_RANDOM` の 16 バイト。**
    cursor = cursor.checked_sub(aux.exec_name.len() + 1)?;
    page[cursor..cursor + aux.exec_name.len()].copy_from_slice(aux.exec_name);
    page[cursor + aux.exec_name.len()] = 0;
    let exec_name_at = page_base + cursor as u64;
    cursor = cursor.checked_sub(RANDOM_BYTES)?;
    page[cursor..cursor + RANDOM_BYTES].copy_from_slice(&aux.random);
    let random_at = page_base + cursor as u64;

    // **`auxv` の対を、積む順に並べる。** `AT_PHDR` は、表が載っている像だけである。
    let pairs = [
        aux.program_headers_at.map(|at| (at::PHDR, at)),
        Some((at::PHENT, PROGRAM_HEADER_SIZE)),
        Some((at::PHNUM, u64::from(aux.program_header_count))),
        Some((at::PAGESZ, PAGE_SIZE)),
        Some((at::ENTRY, aux.entry)),
        Some((at::UID, 0)),
        Some((at::EUID, 0)),
        Some((at::GID, 0)),
        Some((at::EGID, 0)),
        Some((at::SECURE, 0)),
        Some((at::RANDOM, random_at)),
        Some((at::EXECFN, exec_name_at)),
    ];
    // 終端（`AT_NULL`）の 1 対を足した数。
    let pair_count = pairs.iter().flatten().count() + 1;

    // 表を置く位置。**表の先頭が 16 の倍数になるように下げる。**
    let strings_bottom = cursor;
    cursor &= !0xF;
    cursor = cursor.checked_sub((FIXED_WORDS + argv.len() + envp.len() + 2 * pair_count) * WORD)?;
    cursor &= !0xF;

    // **文字列の位置は、上から詰めた順にたどり直して求める。**
    let mut string_at = page.len();
    let mut at = cursor;
    let mut put = |value: u64| {
        page[at..at + WORD].copy_from_slice(&value.to_le_bytes());
        at += WORD;
    };
    put(argv.len() as u64);
    for item in argv {
        string_at -= item.len() + 1;
        put(page_base + string_at as u64);
    }
    put(0); // argv の終端
            // **環境（EV。ADR-0041）。** **並びは変えていない**——ここに中身が入った
            // だけである。**空なら終端だけになり、S11-1 の形と同じである。**
    for item in envp {
        string_at -= item.len() + 1;
        put(page_base + string_at as u64);
    }
    put(0); // envp の終端

    for (kind, value) in pairs.iter().flatten() {
        put(*kind);
        put(*value);
    }
    // 破壊テスト (S11-1, no-auxv-terminator): **終端を書かない。** 終端の場所に、終端でない対を置き、表と文字列の間の
    // 詰め物も 0 でない値で埋める（0 のままだと、詰め物が終端に見える。実際のスタックの空きは 0 とは限らない）。
    // **終端が無いことは、終端まで歩いた者にしか分からない**——`syscall-test` が歩いて、見つからないと言う。
    if cfg!(feature = "syscall-test-no-auxv-terminator") {
        /// `AT_IGNORE`。読む側が読み飛ばす型である。
        const AT_IGNORE: u64 = 1;
        put(AT_IGNORE);
        put(1);
        let table_end = at;
        page[table_end..strings_bottom].fill(0xFF);
    } else {
        put(at::NULL); // auxv の終端（type）
        put(0); //                 （value）
    }

    Some(page_base + cursor as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_at(page: &[u8], at: usize) -> u64 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&page[at..at + 8]);
        u64::from_le_bytes(raw)
    }

    /// 試験で使う `auxv` の値。
    fn aux(program_headers_at: Option<u64>) -> Auxv<'static> {
        Auxv {
            program_headers_at,
            program_header_count: 5,
            entry: 0x40_1010,
            random: *b"0123456789abcdef",
            exec_name: b"/bin/x",
        }
    }

    /// `at` から始まる `auxv` が、`expected` の対の並びで、その次が `AT_NULL` であること。
    fn assert_auxv(page: &[u8], at: usize, expected: &[(u64, u64)]) {
        for (index, pair) in expected.iter().enumerate() {
            let found = (
                word_at(page, at + index * 16),
                word_at(page, at + index * 16 + 8),
            );
            assert_eq!(found, *pair, "auxv pair {index}");
        }
        let end = at + expected.len() * 16;
        assert_eq!(
            (word_at(page, end), word_at(page, end + 8)),
            (0, 0),
            "AT_NULL"
        );
    }

    /// 表の並び（`argc`・`argv`・NULL・`envp`・NULL・`auxv`）と、文字列の置き場と、16 バイト整列。
    #[test]
    fn the_initial_stack_follows_the_linux_layout() {
        const BASE: u64 = 0x7FFF_0000_0000;
        let mut page = [0xEE; 4096];
        let sp = build_initial_stack(
            &mut page,
            BASE,
            &[b"a", b"bc"],
            &[b"X=1"],
            &aux(Some(0x40_0040)),
        )
        .unwrap();
        assert_eq!(sp % 16, 0, "16-byte aligned");
        let at = (sp - BASE) as usize;
        assert_eq!(word_at(&page, at), 2, "argc");
        assert_eq!(word_at(&page, at + 8), BASE + 4094, "argv[0]");
        assert_eq!(word_at(&page, at + 16), BASE + 4091, "argv[1]");
        assert_eq!(word_at(&page, at + 24), 0, "the end of argv");
        assert_eq!(word_at(&page, at + 32), BASE + 4087, "envp[0]");
        assert_eq!(word_at(&page, at + 40), 0, "the end of envp");
        assert_eq!(&page[4094..4096], b"a\0");
        assert_eq!(&page[4091..4094], b"bc\0");
        assert_eq!(&page[4087..4091], b"X=1\0");
        // **`auxv` は、決めた順に並び、`AT_NULL` で終わる。** 文字列と 16 バイトは、`argv` と `envp` の文字列の下に在る。
        assert_eq!(&page[4080..4087], b"/bin/x\0");
        assert_eq!(&page[4064..4080], b"0123456789abcdef");
        assert_auxv(
            &page,
            at + 48,
            &[
                (3, 0x40_0040),
                (4, 56),
                (5, 5),
                (6, 4096),
                (9, 0x40_1010),
                (11, 0),
                (12, 0),
                (13, 0),
                (14, 0),
                (23, 0),
                (25, BASE + 4064),
                (31, BASE + 4080),
            ],
        );
    }

    /// プログラムヘッダの表が載っていない像には、`AT_PHDR` を積まない（0 を番地として渡さない）。ほかは同じである。
    #[test]
    fn at_phdr_is_left_out_when_the_table_is_not_loaded() {
        let mut page = [0; 4096];
        let sp = build_initial_stack(&mut page, 0, &[b"a"], &[], &aux(None)).unwrap();
        assert_eq!(sp % 16, 0);
        // 表の先頭が `AT_PHENT` で、11 対の次が `AT_NULL` である（`AT_PHDR` は無い）。
        assert_auxv(
            &page,
            sp as usize + 32,
            &[
                (4, 56),
                (5, 5),
                (6, 4096),
                (9, 0x40_1010),
                (11, 0),
                (12, 0),
                (13, 0),
                (14, 0),
                (23, 0),
                (25, 4071),
                (31, 4087),
            ],
        );
    }

    /// 文字列と表がページに収まらなければ `None` である。
    #[test]
    fn an_initial_stack_that_does_not_fit_is_refused() {
        let mut page = [0; 64];
        let long = [b'x'; 60];
        assert_eq!(
            build_initial_stack(&mut page, 0, &[&long[..]], &[], &aux(None)),
            None
        );
        // `auxv` だけでも収まらない大きさ。
        let mut small = [0; 128];
        assert_eq!(
            build_initial_stack(&mut small, 0, &[b"a"], &[], &aux(None)),
            None
        );
    }
}
