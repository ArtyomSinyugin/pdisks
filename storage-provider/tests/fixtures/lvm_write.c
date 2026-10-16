/* Disk-free libblockdev ABI fixture. Each loaded copy owns isolated in-memory state. */
#include <stdint.h>
#include <stdlib.h>

#ifndef SCENARIO
#define SCENARIO 0
#endif

/* Prefix layouts match the libblockdev lvm.h records consumed by the Rust adapter. */
typedef struct { uint32_t domain; int code; char *message; } Error;
typedef struct {
    char *name, *uuid; uint64_t free, size, offset; char *vg_name, *vg_uuid;
    uint64_t vg_size, vg_free, extent_size, extent_count, free_count, pv_count;
    char **tags; int missing;
} Pv;
typedef struct {
    char *name, *uuid; uint64_t size, free, extent_size, extent_count, free_count, pv_count;
    int exported; char **tags;
} Vg;
typedef struct {
    char *name, *vg_name, *uuid; uint64_t size;
    char *attr, *segtype, *origin, *pool, *data, *metadata, *roles, *move_pv;
    uint64_t data_percent, metadata_percent, copy_percent;
    char **tags, **data_lvs, **metadata_lvs; void **segments;
} Lv;

/* Count mutator invocations separately from whether the requested effect became visible. */
static unsigned calls;

/* Return an owned numeric error without retaining caller-owned storage. */
static void fail(Error **error) {
    *error = calloc(1, sizeof(Error));
    (*error)->code = 37;
}

/* Standalone lifecycle is deliberately trivial and touches no system resources. */
int bd_lvm_init(void) { return SCENARIO != 18; }
/* Closing a session must preserve the in-memory topology across fresh queries. */
void bd_lvm_close(void) {}
/* Require the BASIC CREATE|QUERY ABI constants. */
int bd_lvm_is_tech_avail(int tech, uint64_t mode, Error **error) {
    (void)error;
    return SCENARIO != 12 && tech == 0 && mode == 17;
}
/* All fixture record strings are static; native record ownership is heap-only. */
void g_free(void *pointer) { free(pointer); }
/* Fixture errors have no dynamically allocated message. */
void g_error_free(Error *error) { free(error); }
/* Release one PV allocated by the matching query. */
void bd_lvm_pvdata_free(Pv *pv) { free(pv); }
/* Release one VG allocated by the matching query. */
void bd_lvm_vgdata_free(Vg *vg) { free(vg); }
/* Release one LV allocated by the matching query. */
void bd_lvm_lvdata_free(Lv *lv) { free(lv); }

/* Publish a complete member or an explicit query failure/missing member. */
Pv **bd_lvm_pvs(Error **error) {
    if (SCENARIO == 11 || (SCENARIO == 14 && calls)) { fail(error); return NULL; }
    Pv **items = calloc(2, sizeof(Pv *));
    items[0] = calloc(1, sizeof(Pv));
    *items[0] = (Pv){ .name = "/dev/mock", .uuid = "pv-uuid", .size = 16384,
        .free = 8192, .offset = 4096, .vg_name = "vg", .vg_uuid = "vg-uuid",
        .missing = SCENARIO == 6 };
    return items;
}
/* Free capacity exposes call count so tests can prove that rejected requests never invoked create. */
Vg **bd_lvm_vgs(Error **error) {
    (void)error;
    Vg **items = calloc(2, sizeof(Vg *));
    items[0] = calloc(1, sizeof(Vg));
    *items[0] = (Vg){ .name = "vg", .uuid = SCENARIO == 1 ? "replacement" : "vg-uuid",
        .size = 16384, .free = SCENARIO == 3 ? 0 : 8192 - calls * 4096,
        .extent_size = SCENARIO == 4 ? 8192 : 4096,
        .pv_count = SCENARIO == 17 ? 2 : 1, .exported = SCENARIO == 5 };
    return items;
}
/* Publish an old colliding LV or the requested effect only after the mock mutator ran. */
Lv **bd_lvm_lvs(const char *vg, Error **error) {
    (void)vg; (void)error;
    Lv **items = calloc(3, sizeof(Lv *));
    if ((calls && SCENARIO != 8) || SCENARIO == 2 || SCENARIO == 10) {
        items[0] = calloc(1, sizeof(Lv));
        *items[0] = (Lv){ .name = SCENARIO == 10 && !calls ? "old" : "data",
            .vg_name = SCENARIO == 15 ? "foreign" : "vg", .uuid = "new-uuid",
            .size = SCENARIO == 9 ? 8192 : 4096, .segtype = "linear" };
        if (SCENARIO == 16 && calls) {
            items[1] = calloc(1, sizeof(Lv)); *items[1] = *items[0];
        }
    }
    return items;
}
/* This is the only mutation: a counter in this shared object's private memory. */
int bd_lvm_lvcreate(const char *vg, const char *lv, uint64_t size, const char *type,
    const char **pvs, const void **extra, Error **error) {
    (void)vg; (void)lv; (void)size; (void)type; (void)pvs; (void)extra;
    calls++;
    if (SCENARIO == 7 || SCENARIO == 13) fail(error);
    return SCENARIO != 7;
}
