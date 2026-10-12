/*
 * host.c — M0 A/B harness.
 *
 * Emulates the game-side memory layout so the proxy's patch modes can be
 * exercised without the game: two fake regions are mapped at the REAL RVAs
 * of the host exe's image (a page containing the filter function at
 * 0x1A13B0, and a 64K region containing the whitelist table 0x6FC1C0 and the
 * FORM list 0x6FC430). The real FORM list bytes (extracted from the game exe)
 * are copied in, a proxy version.dll (C reference or Rust port) is loaded
 * with an atrain9_proxy.ini for the mode under test, and the patched filter
 * is called through its address, exactly like the game does.
 *
 * Build: x86_64-w64-mingw32-gcc -O2 -o host.exe host.c
 * Run (from a dir with host.exe, version.dll, atrain9_proxy.ini,
 *      atrain9_filter*.txt, form_list.bin):
 *   wine host.exe <form_list.bin> <mode>
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define RVA_FILTER 0x1A13B0u
#define RVA_TABLE  0x6FC1C0u
#define RVA_FORMS  0x6FC430u

/* the ORIGINAL bytes the M2 engine's address book records at its anchors —
 * the engine verifies these before any write, so the fake pages must carry
 * them (from the international Steam exe, file offset 0x1a07b0 / 0x6fb5c0) */
static const unsigned char g_filter_orig[16] = {
    0x48,0x89,0x5c,0x24,0x08,0x48,0x89,0x74,0x24,0x18,0x48,0x89,0x7c,0x24,0x20,0x48
};
static const unsigned char g_table_e0[4] = { 0x5f,0x00,0x00,0x00 }; /* whitelist entry 0 */
static const unsigned char g_table_e6[4] = { 0xbb,0x00,0x00,0x00 }; /* whitelist entry 6 (table113 target) */

typedef void *(*fn_t)(void *, DWORD *, DWORD *, void *);

static BYTE *g_base;

static void *map_fixed(UINT_PTR addr, SIZE_T size, DWORD prot) {
    /* VirtualAlloc rounds down to 64K as needed; we only need the requested
       range to end up mapped. */
    return VirtualAlloc((void *)addr, size, MEM_RESERVE | MEM_COMMIT, prot);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        printf("usage: host <form_list.bin> <mode>\n");
        return 2;
    }
    const char *mode = argv[2];
    g_base = (BYTE *)GetModuleHandleW(NULL);

    if (!map_fixed((UINT_PTR)g_base + 0x1A1000, 0x1000, PAGE_EXECUTE_READWRITE)) {
        printf("SKIP: could not map fn page at RVA 0x1A1000\n");
        return 3;
    }
    if (!map_fixed((UINT_PTR)g_base + 0x6FC000, 0x10000, PAGE_READWRITE)) {
        printf("SKIP: could not map data page at RVA 0x6FC000\n");
        return 3;
    }
    memset((BYTE *)g_base + 0x1A1000, 0xCC, 0x1000);
    /* the filter entry carries the real original prologue (expect-verified) */
    memcpy((BYTE *)g_base + RVA_FILTER, g_filter_orig, sizeof(g_filter_orig));
    /* the whitelist table carries its real original entries where recorded */
    memcpy((BYTE *)g_base + RVA_TABLE, g_table_e0, sizeof(g_table_e0));
    memcpy((BYTE *)g_base + RVA_TABLE + 24, g_table_e6, sizeof(g_table_e6));

    /* real FORM list bytes -> the mapped region (NUL-terminated UTF-16) */
    FILE *f = fopen(argv[1], "rb");
    if (!f) {
        printf("FAIL: cannot open %s\n", argv[1]);
        return 2;
    }
    static BYTE forms[0x10000];
    size_t n = fread(forms, 1, sizeof(forms) - 2, f);
    fclose(f);
    forms[n] = 0;
    forms[n + 1] = 0;
    memcpy((BYTE *)g_base + RVA_FORMS, forms, n + 2);

    /* load the proxy under test (app-dir search hits our version.dll) */
    HMODULE v = LoadLibraryA("version.dll");
    if (!v) {
        printf("FAIL: LoadLibrary(version.dll) err=%lu\n", (unsigned long)GetLastError());
        return 1;
    }

    /* forwarded-export sanity: must answer through the REAL system dll */
    FARPROC q = GetProcAddress(v, "GetFileVersionInfoSizeW");
    if (!q) {
        printf("FAIL: export GetFileVersionInfoSizeW missing\n");
        return 1;
    }
    DWORD sz = ((DWORD (*)(LPCWSTR, LPDWORD))q)(L"C:\\windows\\system32\\kernel32.dll", NULL);
    printf("forward GetFileVersionInfoSizeW(kernel32) = %lu %s\n",
           (unsigned long)sz, sz ? "(PASS)" : "(zero — wine resource quirk, still parity if both agree)");

    fn_t fnp = (fn_t)(g_base + RVA_FILTER);

    if (!strcmp(mode, "none")) {
        BYTE *fnb = g_base + RVA_FILTER;
        int unchanged = memcmp(fnb, g_filter_orig, sizeof(g_filter_orig)) == 0;
        printf("mode=none: filter bytes %s\n", unchanged ? "unchanged (PASS)" : "PATCHED (FAIL)");
        return 0;
    }
    if (!strcmp(mode, "table113")) {
        DWORD *tbl = (DWORD *)(g_base + RVA_TABLE);
        printf("mode=table113: tbl[6] = %lu %s\n", (unsigned long)tbl[6],
               tbl[6] == 136 ? "(PASS)" : "(FAIL)");
        return 0;
    }
    if (!strcmp(mode, "nop")) {
        DWORD arr[4] = {5, 3, 9, 1};
        void *out = NULL;
        void *ret = fnp(&out, arr, arr + 4, NULL);
        DWORD *ne = *(DWORD **)ret;
        int ok = (ret == &out) && (ne == arr + 4);
        printf("mode=nop: identity %s (ret==out:%d new_end==end:%d)\n",
               ok ? "(PASS)" : "(FAIL)", ret == &out, ne == arr + 4);
        return 0;
    }

    /* filter / model / modeltex */
    static DWORD ids[1024];
    for (int i = 0; i < 1024; i++) ids[i] = (DWORD)i;
    void *out = NULL;
    void *ret = fnp(&out, ids, ids + 1024, NULL);
    DWORD *ne = *(DWORD **)ret;
    printf("mode=%s: kept %ld / 1024 %s\n", mode, (long)(ne - ids),
           ret == &out ? "(ret==out)" : "(RET!=OUT FAIL)");
    return 0;
}
