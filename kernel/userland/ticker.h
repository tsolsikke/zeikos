/* 2 本の Ring 3 が同時に進むことを見る（W1-c-4。ADR-0060）。本体。
 *
 * **`tickera.c` と `tickerb.c` がこれを取り込む。** **名前と周回数だけが違う。**
 * **引数で渡さないのは、`_start` が `main` へ引数を渡さないからである**
 * （`libc.c` の `exit(main())`。**`_start` を変えると、すべての C のイメージが動く**）。
 *
 * **2 本は別の空間で、同じ VA に `.data` の `name` を持つ。** **起動した後に自分の名前を
 * 書き、毎周読み直す。**
 *
 * **判定は行の順序ではなく内容で見る。** 終わりに 1 行出す。
 *
 *   ticker X done rounds=N name_ok=true tls_ok=true sum_ok=true
 *
 *   name_ok  自分の `.data` の名前が、書いた値のままだった（CR3 の入れ替え）
 *   tls_ok   FS の基底が、自分が入れた番地のままだった（FS の基底の入れ替え。2026-10-05）
 *   sum_ok   途中の和が毎周期待値どおりだった（FP の入れ替え）
 *
 * **FS の基底は、2 本で違う番地にする**——**2 本は同じ本体なので、同じ番地を入れると、入れ替えを省いても値が
 * 揃って見える。** `A` は配列の 0 番目、`B` は 1 番目を指す。**毎周、基底をカーネルに訊いて比べる**
 * （`arch_prctl` の `ARCH_GET_FS`）。訊いた番地が自分のものなら、`fs:0` から目印も読む。**先に訊くのは、基底が
 * 別の番地（0 を含む）のまま `fs:0` を読むと、ページフォルトで終わってしまい、判定の行が出なくなるからである。**
 *
 * **足す量を 2 本で変える**——**同じ量だと、入れ替えを省いても値が揃って見えうる。**
 *
 * **`A` は最後に `ud2` で終了させられる**（回復点の入れ替えを見る）。**`B` は `exit(0)` で終わる。** */

#include "libc.h"

/* 1 周で足す回数。**タイマが何度も食い込む長さにする**（`fptest.c` と同じ）。 */
#define TICKER_ADDS_PER_ROUND 2000000UL

/* **`volatile` にする**——**`-Os` は「書いてから読むまでに呼び出しが無い」を見て、
 * 比較を畳みうる。** **畳まれると、空間を取り違えても `name_ok` は真のままである。** */
static volatile char ticker_name[8] = "?";

/* FS の基底の先に置く語（2026-10-05）。**2 本で別の要素を使う**（上の説明）。 */
static volatile unsigned long ticker_tls[2];

#define TICKER_ARCH_PRCTL 158
#define TICKER_ARCH_SET_FS 0x1002
#define TICKER_ARCH_GET_FS 0x1003

static long ticker_arch_prctl(long code, unsigned long address) {
    long result;
    __asm__ volatile("int $0x80"
                     : "=a"(result)
                     : "a"((long)TICKER_ARCH_PRCTL), "D"(code), "S"(address)
                     : "rcx", "r11", "memory");
    return result;
}

int main(void) {
    const char me = TICKER_NAME;
    ticker_name[0] = me;

    /* 自分の要素へ目印を書き、その番地を FS の基底にする。もう片方の要素には、違う値を置く。 */
    const int tls_index = (me == 'A') ? 0 : 1;
    const unsigned long tls_mark = 0x7150000000000000UL | (unsigned long)me;
    ticker_tls[tls_index] = tls_mark;
    ticker_tls[1 - tls_index] = ~tls_mark;
    const unsigned long tls_base = (unsigned long)&ticker_tls[tls_index];
    int tls_ok = ticker_arch_prctl(TICKER_ARCH_SET_FS, tls_base) == 0;

    /* **2 進で割り切れる量にして、和を厳密に比べる。** */
    const double step = TICKER_STEP;
    double accumulator = 0.0;
    int name_ok = 1;
    int sum_ok = 1;
    unsigned long done = 0;
    for (; done < TICKER_ROUNDS; done++) {
        for (unsigned long add = 0; add < TICKER_ADDS_PER_ROUND; add++) {
            accumulator += step;
        }
        if (ticker_name[0] != me) {
            name_ok = 0;
        }
        unsigned long base_now = 0;
        if (ticker_arch_prctl(TICKER_ARCH_GET_FS, (unsigned long)&base_now) != 0 || base_now != tls_base) {
            tls_ok = 0;
        } else {
            unsigned long through_fs;
            __asm__ volatile("movq %%fs:0, %0" : "=r"(through_fs));
            if (through_fs != tls_mark) {
                tls_ok = 0;
            }
        }
        if (accumulator != step * (double)TICKER_ADDS_PER_ROUND * (double)(done + 1)) {
            sum_ok = 0;
        }
    }

    write(STDOUT, "ticker ", 7);
    write(STDOUT, &me, 1);
    write(STDOUT, " done rounds=", 13);
    putu(done);
    if (name_ok) {
        write(STDOUT, " name_ok=true", 13);
    } else {
        write(STDOUT, " name_ok=false", 14);
    }
    if (tls_ok) {
        write(STDOUT, " tls_ok=true", 12);
    } else {
        write(STDOUT, " tls_ok=false", 13);
    }
    puts(sum_ok ? " sum_ok=true" : " sum_ok=false");

#ifdef TICKER_FOLDS
    __builtin_trap();
#endif
    return 0;
}
