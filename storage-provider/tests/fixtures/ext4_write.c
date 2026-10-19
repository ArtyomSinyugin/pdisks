#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <string.h>
#include <time.h>
#include <sys/stat.h>
#include <unistd.h>
#include <stdio.h>

#ifndef SCENARIO
#define SCENARIO 0
#endif

/* Mock-only plugin state; no device or filesystem mutator is called by this library. */
static atomic_int initialized = 0;
static int info_live = 0, error_live = 0;
/* GLib-compatible owned error prefix used to verify cleanup and diagnostic redaction. */
struct Error { uint32_t domain; int code; char *message; };
/* Transfers a private owned error that must be freed even on native success. */
static void set_error(void **output) {
    struct Error *error = calloc(1, sizeof(*error)); assert(error);
    error->code = 17; error->message = strdup("sensitive native device path"); *output = error; ++error_live;
}
/* Rejects overlapping standalone sessions across Rust probe/readiness/write callers. */
int bd_fs_init(void) {
    if (SCENARIO == 1 || atomic_exchange(&initialized, 1)) return 0;
    struct timespec pause = {0, 1000000};
    nanosleep(&pause, NULL);
    return 1;
}
/* Verifies session balancing and release of every owned info/error result. */
void bd_fs_close(void) { assert(atomic_exchange(&initialized, 0) == 1); assert(!info_live && !error_live); }
/* Releases the transfer-full arrays, signatures and missing-utility strings. */
void g_free(void *pointer) { free(pointer); }

/* Exact BDFSFeatures ABI used by both ordinary capability probes and native readiness. */
struct Features {
    int resize, mkfs, fsck, configure, features;
    const char *partition_id, *partition_type;
    uint64_t min_size, max_size;
};
static const struct Features features = {0, 11, 0, 0, 0, NULL, NULL, SCENARIO == 21 ? 0 : (SCENARIO == 17 ? 33554432 : 1048576), SCENARIO == 22 ? 512 : (SCENARIO == 18 ? 8388608 : 0)};
/* Returns static capacity constraints without allocating or touching any file. */
const struct Features *bd_fs_features(const char *type, void **error) {
    assert(!strcmp(type, "ext4")); (void)error; return &features;
}
/* Publishes one borrowed filesystem name inside a caller-owned pointer array. */
const char **bd_fs_supported_filesystems(void **error) {
    (void)error;
    const char **items = calloc(2, sizeof(*items));
    assert(items); items[0] = "ext4"; return items;
}
/* Advertises exact mkfs options, optionally simulating a missing utility. */
int bd_fs_can_mkfs(const char *type, int *options, char **utility, void **error) {
    assert(!strcmp(type, "ext4")); (void)error; *options = SCENARIO == 4 ? 0 : 11; *utility = SCENARIO == 5 ? strdup("mkfs.ext4") : NULL; return SCENARIO != 5;
}
/* Keeps the existing capability-probe ABI complete without enabling resize execution. */
int bd_fs_can_resize(const char *type, int *flags, char **utility, void **error) {
    assert(!strcmp(type, "ext4")); (void)error; *flags = 0; *utility = NULL; return 1;
}
/* Remaining read-only capability entrypoints return a known test-only availability. */
#define SIMPLE(name) int name(const char *type, char **utility, void **error) { assert(!strcmp(type, "ext4")); (void)error; *utility = NULL; return 1; }
SIMPLE(bd_fs_can_check)
SIMPLE(bd_fs_can_repair)
SIMPLE(bd_fs_can_set_label)
SIMPLE(bd_fs_can_set_uuid)
SIMPLE(bd_fs_can_get_size)
SIMPLE(bd_fs_can_get_free_space)
SIMPLE(bd_fs_can_get_min_size)


/* No host mkfs or device writes: creation changes only these private-memory records. */
static int created = 0;
static char output_uuid[40], output_label[17];
/* Generic mkfs options layout, including reserved bytes checked by the mock mutator. */
struct MkfsOptions {
    const char *label, *uuid;
    int dry_run, no_discard, force, no_pt;
    unsigned char reserve[32];
};
/* Transfer-full ext4 metadata queried after mock creation. */
struct ExtInfo {
    char *label, *uuid, *state;
    uint64_t block_size, block_count, free_blocks;
};
/* Checks that the caller requests only ext4 creation and result queries. */
int bd_fs_is_tech_avail(int tech, uint64_t modes, void **error) {
    assert(tech == 4 && modes == 33); (void)error; return SCENARIO != 2;
}
#if SCENARIO != 3
/* Checks descriptor addressing and options; changes only private-memory output records. */
int bd_fs_mkfs(const char *path, const char *type, struct MkfsOptions *options, const void **extra, void **error) {
    assert(atomic_load(&initialized) == 1 && !created);
    assert(!strcmp(type, "ext4") && extra == NULL && error && *error == NULL);
    char prefix[64]; snprintf(prefix, sizeof(prefix), "/proc/%ld/fd/", (long)getpid());
    assert(!strncmp(path, prefix, strlen(prefix)));
    struct stat info; assert(!stat(path, &info) && info.st_size == 16 * 1024 * 1024);
    assert(options && !options->dry_run && options->no_discard == 1 && !options->force && !options->no_pt);
    for (unsigned i = 0; i < sizeof(options->reserve); ++i) assert(options->reserve[i] == 0);
    assert(options->uuid && strlen(options->uuid) == 36);
    strcpy(output_uuid, options->uuid);
    if (options->label) { assert(strlen(options->label) <= 16); strcpy(output_label, options->label); }
    assert(SCENARIO != 6 && SCENARIO != 7 && SCENARIO != 17 && SCENARIO != 18);
    created = 1;
    if (SCENARIO == 9) set_error(error);
    return SCENARIO != 8;
}
#endif
/* Distinguishes absent signatures from query errors and unexpected post-call types. */
char *bd_fs_get_fstype(const char *path, void **error) {
    (void)path;
    if (!created && SCENARIO == 6) return strdup("xfs");
    if (!created && SCENARIO == 7) { set_error(error); return NULL; }
    if (created && SCENARIO == 16) set_error(error);
    return created ? strdup(SCENARIO == 11 ? "xfs" : "ext4") : NULL;
}
/* Returns owned metadata, including optional contradictory or incomplete readback scenarios. */
struct ExtInfo *bd_fs_ext4_get_info(const char *path, void **error) {
    (void)path; assert(created);
    if (SCENARIO == 13) return NULL;
    struct ExtInfo *info = calloc(1, sizeof(*info)); assert(info); ++info_live;
    info->uuid = SCENARIO == 20 ? NULL : strdup(SCENARIO == 10 ? "00000000-0000-0000-0000-000000000001" : output_uuid);
    info->label = strdup(SCENARIO == 12 ? "unexpected" : output_label); info->state = strdup("clean");
    info->block_size = SCENARIO == 14 ? 512 : 4096;
    info->block_count = SCENARIO == 19 ? UINT64_MAX : 4000;
    if (SCENARIO == 15) set_error(error);
    return info;
}
/* Releases all transferred metadata fields exactly once. */
void bd_fs_ext4_info_free(struct ExtInfo *info) {
    if (info) { assert(info_live-- == 1); free(info->uuid); free(info->label); free(info->state); free(info); }
}
/* Releases the sensitive native message together with its owned error prefix. */
void g_error_free(struct Error *error) { assert(error && error_live-- == 1); free(error->message); free(error); }
