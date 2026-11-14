/* Prints the ten largest matches of a query through zthing.dll.
 *
 * Build in a "x64 Native Tools" prompt from the repository root:
 *   cargo build --release -p zthing-ffi
 *   cl /nologo /utf-8 /I crates\zthing-ffi\include examples\c\search.c target\release\zthing.dll.lib
 * Run with zthing.dll next to search.exe or on PATH:
 *   search.exe "*.iso"
 */
#include <stdio.h>
#include <windows.h>

#include "zthing.h"

int main(int argc, char **argv) {
    const char *query = argc > 1 ? argv[1] : "*.dll";
    /* Paths are UTF-8, so the console has to print UTF-8 as well. */
    SetConsoleOutputCP(CP_UTF8);

    Zthing *zt = zthing_connect();
    zthing_set_query(zt, query);
    zthing_set_sort(zt, ZTHING_SORT_SIZE, 1);
    zthing_set_range(zt, 0, 10);
    if (zthing_execute(zt) != ZTHING_OK) {
        fprintf(stderr, "search failed: %s\n", zthing_last_error(zt));
        zthing_free(zt);
        return 2;
    }

    printf("%llu matches for %s, the largest:\n", zthing_total(zt), query);
    for (uint64_t i = 0; i < zthing_result_count(zt); i++) {
        printf("%12llu  %s\n", zthing_result_size(zt, i), zthing_result_path(zt, i));
    }
    zthing_free(zt);
    return 0;
}
