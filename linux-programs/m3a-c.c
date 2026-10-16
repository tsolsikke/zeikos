/* m3a-c: Linux 向けに musl-gcc で静的リンクした C のプログラム。musl の pthread で、pthread_create と pthread_join を
 * 行う（スレッドの検証の3）。出力に、環境で変わる値（アドレス、tid の値そのもの）は入れない。終了の状態は引数の数。 */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static int shared = 0;

static void *worker(void *argument) {
    int *value = argument;
    shared = *value + 7;
    printf("m3a-c: the worker runs with argument %d and sets shared to %d\n", *value, shared);
    return &shared;
}

int main(int argc, char **argv) {
    (void)argv;
    printf("m3a-c: %d argument(s)\n", argc);
    pthread_t thread;
    int argument = 35;
    int created = pthread_create(&thread, NULL, worker, &argument);
    if (created != 0) {
        printf("m3a-c: pthread_create failed with %d\n", created);
        return 99;
    }
    void *result = NULL;
    int joined = pthread_join(thread, &result);
    if (joined != 0) {
        printf("m3a-c: pthread_join failed with %d\n", joined);
        return 98;
    }
    int same_thread = pthread_equal(pthread_self(), thread);
    printf("m3a-c: joined, the worker returned %d; the worker was a different thread = %s\n",
           result ? *(int *)result : -1, same_thread ? "false" : "true");
    return argc;
}
