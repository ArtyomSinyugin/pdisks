#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <string.h>
#include <time.h>

#ifndef SCENARIO
#define SCENARIO 0
#endif

/* Mock-only plugin state; no device or filesystem mutator is called by this library. */
static atomic_int initialized = 0;
int bd_fs_init(void) {
    if (SCENARIO == 1 || atomic_exchange(&initialized, 1)) return 0;
    struct timespec pause = {0, 1000000};
    nanosleep(&pause, NULL);
    return 1;
}
void bd_fs_close(void) { assert(atomic_exchange(&initialized, 0) == 1); }
void g_free(void *pointer) { free(pointer); }

struct Features {
    int resize, mkfs, fsck, configure, features;
    const char *partition_id, *partition_type;
    uint64_t min_size, max_size;
};
static const struct Features features = {0, 11, 0, 0, 0, NULL, NULL, 1048576, 0};
const struct Features *bd_fs_features(const char *type, void **error) {
    assert(!strcmp(type, "ext4")); (void)error; return &features;
}
const char **bd_fs_supported_filesystems(void **error) {
    (void)error;
    const char **items = calloc(2, sizeof(*items));
    assert(items); items[0] = "ext4"; return items;
}
int bd_fs_can_mkfs(const char *type, int *options, char **utility, void **error) {
    assert(!strcmp(type, "ext4")); (void)error; *options = 11; *utility = NULL; return 1;
}
int bd_fs_can_resize(const char *type, int *flags, char **utility, void **error) {
    assert(!strcmp(type, "ext4")); (void)error; *flags = 0; *utility = NULL; return 1;
}
#define SIMPLE(name) int name(const char *type, char **utility, void **error) { assert(!strcmp(type, "ext4")); (void)error; *utility = NULL; return 1; }
SIMPLE(bd_fs_can_check)
SIMPLE(bd_fs_can_repair)
SIMPLE(bd_fs_can_set_label)
SIMPLE(bd_fs_can_set_uuid)
SIMPLE(bd_fs_can_get_size)
SIMPLE(bd_fs_can_get_free_space)
SIMPLE(bd_fs_can_get_min_size)
