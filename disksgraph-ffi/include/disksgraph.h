#ifndef DISKSGRAPH_JSON_H
#define DISKSGRAPH_JSON_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* JSON memory bridge version. Stabby exports remain separately available. */
uint32_t disksgraph_json_api_version(void);
/* Input is borrowed UTF-8 JSON; returned NUL-terminated JSON must be freed below. */
char *disksgraph_json_open(const uint8_t *json, size_t length);
char *disksgraph_json_call(uint64_t context, const uint8_t *json, size_t length);
/* Joins the context worker before returning; strings still need explicit release. */
char *disksgraph_json_close(uint64_t context);
void disksgraph_json_free(char *reply);
/* Called on the owner thread with borrowed versioned JSON. Do not retain json or
 * reenter this context. Return 1=satisfied, 0=violated, -1/other=unknown.
 * Keep callback code and cookie resources alive until close has returned. */
typedef int32_t (*disksgraph_policy_fn)(uint64_t cookie, const uint8_t *json, size_t length);
char *disksgraph_json_open_with_policy(const uint8_t *json, size_t length,
                                     disksgraph_policy_fn callback, uint64_t cookie);
#ifdef __cplusplus
}
#endif
#endif
