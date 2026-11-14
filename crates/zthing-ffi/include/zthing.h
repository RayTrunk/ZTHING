/*
 * zthing.h - C interface of zthing.dll, a client of the ZTHING service.
 * The service must run; the DLL talks to it over the named pipe \.\pipe\zthing.
 *
 * Rules shared by all functions:
 * - Strings passed in are null-terminated, UTF-8 for char and UTF-16 for uint16_t
 *   (wchar_t on Windows).
 * - Strings returned belong to the handle and stay valid until the next
 *   zthing_execute or zthing_free on that handle. Do not free them.
 * - A handle must not be used by two threads at the same time.
 *   Use one handle per thread instead.
 *
 * Link against zthing.dll.lib (MSVC) or load zthing.dll at run time
 * (P/Invoke, ctypes). See docs/API.md.
 */
#ifndef ZTHING_H
#define ZTHING_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifndef ZTHING_API
#define ZTHING_API __declspec(dllimport)
#endif

typedef struct Zthing Zthing;

/* Return codes. */
#define ZTHING_OK 0
/* The service is unreachable or the connection broke. The next call reconnects. */
#define ZTHING_ERROR_SERVICE 1
/* The service rejected the query, for example a bad regex. */
#define ZTHING_ERROR_QUERY 2
#define ZTHING_ERROR_ARGUMENT 3

/* Sort keys for zthing_set_sort. */
#define ZTHING_SORT_NAME 0
#define ZTHING_SORT_PATH 1
#define ZTHING_SORT_SIZE 2
#define ZTHING_SORT_MODIFIED 3

/* Never returns NULL. If the service is unreachable, zthing_last_error says why
   and zthing_execute tries to connect again. */
ZTHING_API Zthing *zthing_connect(void);
ZTHING_API void zthing_free(Zthing *handle);

/* Query syntax as in the search window, see docs/API.md. */
ZTHING_API int32_t zthing_set_query(Zthing *handle, const char *query);
ZTHING_API int32_t zthing_set_query_w(Zthing *handle, const uint16_t *query);
/* sort: ZTHING_SORT_*. descending: 0 or 1. Default: name, ascending. */
ZTHING_API int32_t zthing_set_sort(Zthing *handle, int32_t sort, int32_t descending);
/* Skips offset matches and keeps at most limit. Default: offset 0, limit 1000.
   UINT64_MAX keeps all matches. */
ZTHING_API int32_t zthing_set_range(Zthing *handle, uint64_t offset, uint64_t limit);

/* Runs the query and holds the results in the handle. */
ZTHING_API int32_t zthing_execute(Zthing *handle);

/* All matches of the last query, also those outside the range. */
ZTHING_API uint64_t zthing_total(Zthing *handle);
/* Results held by the handle, indexed from 0. */
ZTHING_API uint64_t zthing_result_count(Zthing *handle);

/* NULL for an index out of range. */
ZTHING_API const char *zthing_result_name(Zthing *handle, uint64_t index);
ZTHING_API const uint16_t *zthing_result_name_w(Zthing *handle, uint64_t index);
/* Full path including the name. */
ZTHING_API const char *zthing_result_path(Zthing *handle, uint64_t index);
ZTHING_API const uint16_t *zthing_result_path_w(Zthing *handle, uint64_t index);
/* Bytes. 0 for folders. */
ZTHING_API uint64_t zthing_result_size(Zthing *handle, uint64_t index);
/* Unix seconds (UTC). */
ZTHING_API int64_t zthing_result_modified(Zthing *handle, uint64_t index);
ZTHING_API int32_t zthing_result_is_folder(Zthing *handle, uint64_t index);

/* Why the last call failed. Empty after success. Never NULL. */
ZTHING_API const char *zthing_last_error(Zthing *handle);
ZTHING_API const uint16_t *zthing_last_error_w(Zthing *handle);

#ifdef __cplusplus
}
#endif

#endif /* ZTHING_H */
