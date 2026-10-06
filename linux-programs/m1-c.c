/* m1-c: Linux 向けに musl-gcc で静的リンクした C のプログラム。m1-rust と同じことを stdio と malloc で行う。
 * 出力に、環境で変わる値（環境変数の中身、番地、時刻、1 つ目の引数の道）は入れない。終了の状態は引数の数。 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern char **environ;

int main(int argc, char **argv) {
    int envc = 0;
    for (char **e = environ; *e; e++) envc++;
    printf("m1-c: %d argument(s), %d environment variable(s)\n", argc, envc);
    for (int i = 2; i < argc; i++) printf("m1-c: argv[%d] = %s\n", i, argv[i]);

    if (argc > 1) {
        FILE *f = fopen(argv[1], "r");
        if (!f) {
            printf("m1-c: open failed\n");
        } else {
            char buf[4096];
            size_t n = fread(buf, 1, sizeof buf - 1, f);
            buf[n] = 0;
            printf("m1-c: read %zu byte(s) from the file named by argv[1]\n", n);
            char *save = NULL;
            for (char *line = strtok_r(buf, "\n", &save); line; line = strtok_r(NULL, "\n", &save))
                printf("m1-c: | %s\n", line);
            fclose(f);
        }
    }

    size_t big_len = 1u << 20;
    unsigned char *big = calloc(big_len, 1);
    if (!big) return 99;
    big[0] = 0x5a;
    big[big_len - 1] = 0xa5;
    unsigned sum = 0;
    for (size_t i = 0; i < big_len; i++) sum += big[i];
    printf("m1-c: 1 MiB buffer, ends 0x%x 0x%x, sum %u\n", big[0], big[big_len - 1], sum);
    free(big);

    fflush(stdout);
    return argc;
}
