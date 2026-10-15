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
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>

#define RVA_FILTER 0x1A13B0u
#define RVA_TABLE  0x6FC1C0u
#define RVA_FORMS  0x6FC430u
#define RVA_CFILE  0x41D40Cu   /* MFC CFile::Open — the M3 overlay hook point */

/* M4 fetch-probe anchors (the four runtime fetch hooks the probe pins) */
#define RVA_PACK 0x306E20u   /* FUN_140306e20 CPackFileBuffer::fetch */
#define RVA_TEX  0x3010D0u   /* FUN_1403010d0 texture_fetch_miss_path */
#define RVA_CCOM 0x2EC0E0u   /* FUN_1402ec0e0 CCommonShader::fetch */
#define RVA_CACHE 0x3089B0u  /* FUN_1403089b0 global fetch-cache get */

/* M4 inject-mode anchors (builds.toml famb_runner_call + the per-build
 * constants the injector stub pins for build 5.00.5634) */
#define RVA_INJ_SITE 0x3F0259u   /* the famb loader's pool-runner call (E8) */
#define RVA_FAKE_RUNNER 0x504C0u /* the runner the E8 named = FUN_1400504c0 */
#define RVA_POOL 0xF02470u       /* pool object {rows ptr, count, chunk arr} */
#define RVA_POOL_ROWS 0xF02700u  /* the fake rows array (0x148 stride) */
#define RVA_POOL_CHUNK 0xF02660u /* the chunk-size array (u32[8]) */
#define RVA_POOL_CAP 0xD65DF0u   /* DAT_140d65df0 worker-row capacity */
#define RVA_WSOUT 0x4F86088u     /* DAT_144f86088 workspace out ptr */
#define RVA_OUTBUF 0x4F87000u    /* the fake decoded family-B stream */

/* M5 family-A tailgate anchors (builds.toml pack_lzss_*; the fake pages the
 * loader's expect-verification reads) */
#define RVA_TG_DECODE 0x314BF0u  /* pack_lzss_decode — the fake family-A decoder */
#define RVA_TG_IDAT_SITE 0x31516Fu /* i.dat's CALL pack_lzss_decode */
#define RVA_TG_VP_SITE 0x3F1434u /* vp's CALL pack_lzss_decode */
#define RVA_TG_TP_SITE 0x3F0F4Du /* tp's CALL pack_lzss_decode */
#define RVA_TG_CAVE 0x5A1D13u    /* the shared relay cave (19-byte CC run) */
#define RVA_TG_OUT 0x4F87000u    /* == RVA_OUTBUF: the fake workspace out buffer */
#define RVA_TG_IN 0x4F89000u     /* the fake encoded family-A file (in-buffer) */
#define RVA_TG_WORK 0x1000u      /* the fake workspace out span we clear per call */

/* M5-T family-A decode workspace swap (builds.toml workspace_*). The site is
 * boot_pack_system_driver's CALL pack_master_map_access, right after both
 * _aligned_malloc calls; the fake pages carry the address book's site/cave
 * bytes AND reproduce the game's own _aligned_malloc/_aligned_free prologues
 * (so the loader's entry-byte verification passes) with their internal `call`
 * redirected to the harness bodies below. */
#define RVA_WS_SITE 0x31981Au    /* the swap site (E8 -> pack_master_map_access) */
#define RVA_WS_CALLEE 0x314F20u  /* the site's pinned target (page 0x314000 exists) */
#define RVA_WS_MALLOC 0x597BA8u  /* _aligned_malloc */
#define RVA_WS_FREE 0x597B8Cu    /* _aligned_free */
#define RVA_WS_CAVE 0x592363u    /* the M5-T relay cave (19-byte CC run) */
#define RVA_WS_GLOBALS 0x4F86088u /* the workspace singleton (== RVA_WSOUT) */
#define RVA_WS_OLDOUT 0x4F8A000u /* the "game-allocated" OUT buffer */
#define RVA_WS_OLDIN 0x4F8B000u  /* the "game-allocated" IN buffer */
#define RVA_WS_NEWOUT 0x4F8C000u /* what the fake _aligned_malloc hands back */

static const unsigned char g_ws_site_orig[5] = { 0xE8,0x01,0xB7,0xFF,0xFF };

static unsigned g_ws_malloc_size, g_ws_malloc_calls, g_ws_free_calls;
static void *g_ws_malloc_ret;
static void *g_ws_freed;

/* the bodies the faked CRT prologues call into (arguments intact: the malloc
 * body sees rcx=size/rdx=align, the free body rcx = the raw [ptr-8] pointer) */
__attribute__((used, noinline)) void *ws_malloc_body(unsigned long size, unsigned long align)
{
    (void)align;
    g_ws_malloc_calls++;
    g_ws_malloc_size = (unsigned)size;
    return g_ws_malloc_ret;
}
__attribute__((used, noinline)) void ws_free_body(void *raw)
{
    g_ws_free_calls++;
    g_ws_freed = raw;
}

/* calls the spliced swap site exactly as the loader does (a plain call from
 * the wrapper's entry RSP, so the site's own E8 lands the shim at RSP ≡ 8) */
extern uintptr_t ws_call_site(uintptr_t site);
__asm__(
    ".globl ws_call_site\n"
    "ws_call_site:\n"
    "callq *%rcx\n"           /* the site's E8 -> cave -> version.dll stub */
    "retq\n");

/* the ORIGINAL bytes the M2 engine's address book records at its anchors —
 * the engine verifies these before any write, so the fake pages must carry
 * them (from the international Steam exe, file offset 0x1a07b0 / 0x6fb5c0) */
static const unsigned char g_filter_orig[16] = {
    0x48,0x89,0x5c,0x24,0x08,0x48,0x89,0x74,0x24,0x18,0x48,0x89,0x7c,0x24,0x20,0x48
};
static const unsigned char g_table_e0[4] = { 0x5f,0x00,0x00,0x00 }; /* whitelist entry 0 */
static const unsigned char g_table_e6[4] = { 0xbb,0x00,0x00,0x00 }; /* whitelist entry 6 (table113 target) */

/* the CFile::Open prologue (the address book's cfile_open expect bytes) —
 * push rbx/rbp/rsi/rdi/r14/r15; sub rsp,0x288 (pure stack setup, no
 * rip-relative instructions, so a [prologue][jmp entry+16] trampoline works) */
static const unsigned char g_cfile_orig[16] = {
    0x40,0x53,0x55,0x56,0x57,0x41,0x56,0x41,0x57,0x48,0x81,0xec,0x88,0x02,0x00,0x00
};

/* M6: the path the LAST CFile::Open actually used — the loose-asset redirect
 * evidence (cfile_open_body records it; the `loose` host mode reads it back so
 * each open's tag can be tied to the path the loader handed over) */
static wchar_t g_last_path[1024];

/* the M4 fetch-probe prologue RECORDS (the address book's expect bytes;
 * machine-verified boundaries — each ends at an instruction boundary
 * >= 16 so the 16-byte hook write fits and the trampoline re-enters
 * cleanly) */
static const unsigned char g_pack_orig[22] = {
    0x40,0x55,0x53,0x56,0x57,0x41,0x56,0x48,0x8d,0xac,0x24,0x00,0xf8,0xff,0xff,
    0x48,0x81,0xec,0x00,0x09,0x00,0x00
};  /* pushes rbp,rbx,rsi,rdi,r14; lea rbp,[rsp-0x800]; sub rsp,0x900 */
static const unsigned char g_tex_orig[19] = {
    0x40,0x55,0x53,0x56,0x57,0x41,0x54,0x41,0x56,0x41,0x57,0x48,0x8d,0xac,0x24,
    0x20,0xe0,0xff,0xff
};  /* pushes rbp,rbx,rsi,rdi,r12,r14,r15; lea rbp,[rsp-0x1fe0] */
static const unsigned char g_ccom_orig[20] = {
    0x40,0x55,0x53,0x56,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x8d,0xac,
    0x24,0x50,0xe8,0xff,0xff
};  /* pushes rbp,rbx,rsi,r12,r13,r14,r15; lea rbp,[rsp-0x17b0] */
static const unsigned char g_cache_orig[16] = {
    0x48,0x8b,0xc4,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x83,0xec,0x40
};  /* mov rax,rsp; pushes rdi,r12..r15; sub rsp,0x40 */

/* the famb pool-runner call bytes the M4 inject address book records —
 * E8 rel32 (the original call target = RVA_FAKE_RUNNER; RVA-relative, so
 * the same bytes also aim the fake host's E8 at ITS image + 0x504c0) plus
 * the two bytes that follow: `85 db` test ebx,ebx at site+5 (READ-ONLY
 * fingerprint — the splice rewrites only the E8's 5 bytes) and a `74 00 C3`
 * continuation so a plain call of the site entry returns cleanly */
static const unsigned char g_famb_orig[7] = { 0xE8,0x62,0x02,0xC6,0xFF,0x85,0xDB };

/* the M5 tailgate call-site bytes (builds.toml pack_lzss_*_call expect values;
 * the same RVA-relative E8s also aim the fake host's calls at ITS image +
 * 0x314bf0 — the fake family-A decoder) */
static const unsigned char g_tg_vp_orig[5]   = { 0xE8,0xB7,0x37,0xF2,0xFF };
static const unsigned char g_tg_tp_orig[5]   = { 0xE8,0x9E,0x3C,0xF2,0xFF };
static const unsigned char g_tg_idat_orig[5] = { 0xE8,0x7C,0xFA,0xFF,0xFF };

/* the fake original's body: entered via the 16-byte prologue above (from the
 * mapped page or through the loader's trampoline), this thunk undoes the
 * prologue's stack adjustment and tail-jumps into a plain C body — the RET
 * of the body then returns to the original caller either way. */
/* the asm thunk's symbol — defined by the __asm__ block below */
extern void cfile_undo_thunk(void);

__asm__(
    ".globl cfile_undo_thunk\n"
    "cfile_undo_thunk:\n"
    "addq $0x288, %rsp\n"
    "popq %r15\n"
    "popq %r14\n"
    "popq %rdi\n"
    "popq %rsi\n"
    "popq %rbp\n"
    "popq %rbx\n"
    "jmp cfile_open_body\n");

/* non-static + used: referenced only from the asm thunk, so the compiler
 * must not inline or eliminate it */
__attribute__((used, noinline)) int cfile_open_body(void *cfile, const wchar_t *path, unsigned flags, void *perr)
{
    HANDLE h;
    (void)flags; (void)perr;
    /* the path the open used — THE redirect evidence for the harness */
    if (path) {
        wcsncpy(g_last_path, path, 1023);
        g_last_path[1023] = 0;
    } else {
        g_last_path[0] = 0;
    }
    printf("cfile_open_body: path=%ls\n", path);
    h = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, NULL,
                    OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h != INVALID_HANDLE_VALUE) {
        *(uint64_t *)((char *)cfile + 8) = (uint64_t)h;
        *(int *)((char *)cfile + 0x10) = 1;
        return 1;
    }
    *(uint64_t *)((char *)cfile + 8) = (uint64_t)-1;
    *(int *)((char *)cfile + 0x10) = 0;
    return 0;
}

typedef int (*cfile_open_fn)(void *, const wchar_t *, unsigned, void *);

/* ---- M4 fetch-probe fakes: for each anchor the fake page ends with an
 * undo thunk (cancels the record's stack effects and pops the pushed
 * registers) then the plain C body. Page-base-only content: the loader's
 * trampoline re-enters at entry + record_len where the E9 lands in the
 * undo thunk — same mechanics as the cfile fake above. The chkstk amounts
 * are hard constants in the records (mov eax,0x20e0 / 0x18b0) and __chkstk
 * returns its input in rax untouched, so the undo amounts are exact. */
extern void pack_undo_thunk(void);
extern void tex_undo_thunk(void);
extern void ccom_undo_thunk(void);
extern void cache_undo_thunk(void);

__asm__(
    ".globl pack_undo_thunk\n"
    "pack_undo_thunk:\n"
    "addq $0x900, %rsp\n"
    "popq %r14\n"
    "popq %rdi\n"
    "popq %rsi\n"
    "popq %rbx\n"
    "popq %rbp\n"
    "jmp pack_fetch_body\n"
    ".globl tex_undo_thunk\n"
    "tex_undo_thunk:\n"
    "addq $0x20e0, %rsp\n"
    "popq %r15\n"
    "popq %r14\n"
    "popq %r12\n"
    "popq %rdi\n"
    "popq %rsi\n"
    "popq %rbx\n"
    "popq %rbp\n"
    "jmp tex_fetch_body\n"
    ".globl ccom_undo_thunk\n"
    "ccom_undo_thunk:\n"
    "addq $0x18b0, %rsp\n"
    "popq %r15\n"
    "popq %r14\n"
    "popq %r13\n"
    "popq %r12\n"
    "popq %rsi\n"
    "popq %rbx\n"
    "popq %rbp\n"
    "jmp ccom_fetch_body\n"
    ".globl cache_undo_thunk\n"
    "cache_undo_thunk:\n"
    "addq $0x40, %rsp\n"
    "popq %r15\n"
    "popq %r14\n"
    "popq %r13\n"
    "popq %r12\n"
    "popq %rdi\n"
    "jmp cache_get_body\n");

__attribute__((used, noinline)) void *pack_fetch_body(void *pack, void *dest, const void *name_obj)
{
    (void)pack; (void)dest; (void)name_obj;
    printf("pack_fetch_body: called\n");
    return 0;
}

__attribute__((used, noinline)) void *tex_fetch_body(void *ctx, const wchar_t *src, const wchar_t *refname, unsigned mode)
{
    (void)ctx; (void)src; (void)refname; (void)mode;
    printf("tex_fetch_body: called\n");
    return 0;
}

__attribute__((used, noinline)) void *ccom_fetch_body(void *mgr, unsigned long long a2, const wchar_t *refname, unsigned long long a4, void **buf_out, unsigned *size_out)
{
    (void)mgr; (void)a2; (void)a4; (void)refname;
    /* fulfill the out-arg contract the cache-hit path would */
    if (buf_out) *buf_out = 0;
    if (size_out) *size_out = 0;
    printf("ccom_fetch_body: called\n");
    return 0;
}

__attribute__((used, noinline)) void *cache_get_body(void *cache, const wchar_t *name)
{
    (void)cache; (void)name;
    printf("cache_get_body: called\n");
    return 0;
}

/* ---- M4 inject fakes: the pool-runner stand-in (the spliced E8's target)
 * reports the pool state the injector rebuilt, and an asm thunk calls the
 * spliced site under caller-register discipline (a live EBX sentinel that
 * the loader's own `test ebx,ebx` at site+5 makes load-bearing in-game). */
extern uintptr_t inject_ebx_call(uintptr_t site, uintptr_t pool);

/* calls `site` with the loader's argument shape (rcx = pool, as the game's
 * `lea rcx,[pool]` sets before the E8) and the loader's stack alignment —
 * reached with RSP 16-aligned so the site's own push lands the stub at the
 * standard called alignment. Returns the site's rax untouched. */
extern uintptr_t inject_call_site(uintptr_t site, uintptr_t pool);

__asm__(
    ".globl inject_call_site\n"
    "inject_call_site:\n"
    "movq %rcx, %r10\n"
    "movq %rdx, %rcx\n"
    "callq *%r10\n"
    "retq\n");

__attribute__((used, noinline)) uintptr_t fake_runner_body(void *pool)
{
    unsigned long count = *(unsigned int *)((char *)pool + 8);
    void **rows = *(void ***)pool;
    printf("inject runner: count=%lu\n", count);
    for (unsigned long i = 0; i < count && i < 8; i++) {
        unsigned char *r = (unsigned char *)rows + i * 0x148;
        printf("  row%lu: start=%lu entries=%lu mode=0x%02x array=%p\n",
               i,
               (unsigned long)*(unsigned int *)(r + 0x13c),
               (unsigned long)*(unsigned int *)(r + 0x140),
               *(unsigned char *)(r + 0x41),
               *(void **)(r + 0x38));
    }
    return 0x4711;
}

/* calls `site` with the loader's argument shape (rcx = pool), an EBX
 * sentinel (the loader's own `test ebx,ebx` at site+5 makes it
 * load-bearing in-game), and the loader's stack alignment; returns EBX
 * after the call (rax is clobbered by design here — the rax-forward check
 * runs from the plain path) */
__asm__(
    ".globl inject_ebx_call\n"
    "inject_ebx_call:\n"
    "pushq %rbx\n"
    "subq $0x28, %rsp\n"
    "movq %rcx, %r10\n"
    "movq %rdx, %rcx\n"
    "movl $0x1337, %ebx\n"
    "callq *%r10\n"
    "movl %ebx, %eax\n"
    "addq $0x28, %rsp\n"
    "popq %rbx\n"
    "retq\n");

typedef void *(*fn_t)(void *, DWORD *, DWORD *, void *);

/* ---- M5 tailgate fakes. The loader splices the family-A decode call sites
 * over to a relay cave → the version.dll stub, which CALLS the original
 * decoder at RVA_TG_DECODE (the shim below) and then appends into the fake
 * workspace. The fake decoder copies a canned decoded stream into dest and
 * writes its length back to *cap — exactly the ABI the stub relies on. */
static unsigned char *g_tg_decode_src;   /* canned decoded bytes */
static unsigned int g_tg_decode_force_len; /* 0 = use the declared size (else a mismatch) */
/* decoder tail junk: the live decoder decodes the staged input to exhaustion
 * and produces a few bytes MORE than the declared size (vp +119, tp +45,
 * i.dat +21). The fake reproduces that so the tailgate's declared-base append
 * is exercised against the exact live shape. */
static unsigned int g_tg_decode_delta;

__attribute__((used, noinline)) int tg_decode_body(unsigned char *dest, unsigned int *cap,
                                                   const unsigned char *src, unsigned int srclen)
{
    (void)srclen;
    unsigned int declared = *(const unsigned int *)(src - 4);
    unsigned int n = g_tg_decode_force_len
                        ? g_tg_decode_force_len
                        : declared + g_tg_decode_delta;
    if (n > *cap) {
        printf("tg decode: declared=%u over cap=%u -> fail\n", declared, *cap);
        return 0;
    }
    if (g_tg_decode_src) {
        unsigned int copy = n < declared ? n : declared;
        memcpy(dest, g_tg_decode_src, copy);
        if (n > copy) memset(dest + copy, 0xEE, n - copy); /* tail junk */
    } else {
        memset(dest, 0, n);
    }
    *cap = n;
    printf("tg decode: declared=%u produced=%u\n", declared, n);
    return 1;
}

/* calls the spliced decode site with the loader's argument shape (rcx=dest,
 * rdx=&cap, r8=src, r9d=srclen) and the loader's stack discipline: at the
 * `callq` RSP is 16-aligned so the site's own E8 pushes its continuation and
 * the stub sees RSP ≡ 8 (the M4 lesson — a misaligned entry faults the stub's
 * movaps). The 5th C argument (srclen) sits in the caller shadow space. */
extern uintptr_t tg_call_site(uintptr_t site, void *dest, unsigned *cap, void *src, uintptr_t srclen);

__asm__(
    ".globl tg_call_site\n"
    "tg_call_site:\n"
    "movq %rcx, %r10\n"       /* r10 = site */
    "movq %rdx, %rcx\n"       /* rcx = dest */
    "movq %r8, %rdx\n"        /* rdx = &cap */
    "movq %r9, %r8\n"         /* r8  = src  */
    "movq 0x28(%rsp), %r9\n"  /* r9  = srclen (the 5th argument) */
    "callq *%r10\n"           /* the site's E8 -> cave -> version.dll stub */
    "retq\n");

static BYTE *g_base;

static void *map_fixed(UINT_PTR addr, SIZE_T size, DWORD prot) {
    /* VirtualAlloc rounds down to 64K as needed; we only need the requested
       range to end up mapped. */
    return VirtualAlloc((void *)addr, size, MEM_RESERVE | MEM_COMMIT, prot);
}

/* small file helpers for the M5 tailgate host mode (canned decoded streams
 * in, the post-decode workspace out) */
static long read_whole(const char *path, unsigned char *buf, size_t cap) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    size_t n = fread(buf, 1, cap, f);
    fclose(f);
    return (long)n;
}

static void write_whole(const char *path, const unsigned char *buf, size_t n) {
    FILE *f = fopen(path, "wb");
    if (!f) return;
    (void)fwrite(buf, 1, n, f);
    fclose(f);
}

/* ---- M5-T: build the fake swap environment (see the RVA_WS_* block above).
 * `fail_alloc` makes the fake _aligned_malloc return NULL (the fail-open case). */
static void ws_setup(int fail_alloc)
{
    BYTE *g = (BYTE *)g_base + RVA_WS_GLOBALS;
    BYTE *site = (BYTE *)g_base + RVA_WS_SITE;
    BYTE *callee = (BYTE *)g_base + RVA_WS_CALLEE;
    BYTE *m = (BYTE *)g_base + RVA_WS_MALLOC;
    BYTE *f = (BYTE *)g_base + RVA_WS_FREE;

    /* the singleton: the game's own allocations + the shipped caps */
    *(void **)(g + 0x00) = (void *)(g_base + RVA_WS_OLDOUT);
    *(void **)(g + 0x08) = (void *)(g_base + RVA_WS_OLDIN);
    *(unsigned *)(g + 0x10) = 0xC800000u;
    *(unsigned *)(g + 0x14) = 0x9600000u;

    /* the site: the address book's bytes (E8 -> the pinned callee) + a clean
     * return so calling the site comes back to us */
    memcpy(site, g_ws_site_orig, 5);
    site[5] = 0xC3;

    /* the pinned callee: a value in RAX so the shim's tail jump is observable */
    callee[0] = 0xB8;
    *(unsigned *)(callee + 1) = 0xDEADBEEFu;
    callee[5] = 0xC3;

    /* the cave: the address book records a 16-byte CC run here */
    memset((BYTE *)g_base + RVA_WS_CAVE, 0xCC, 19);

    /* the fakes are simple trampolines to the bodies below (`mov rax, body;
     * jmp rax`) — no prologue to reproduce, no stack change */
    {
        BYTE *m = (BYTE *)g_base + RVA_WS_MALLOC;
        BYTE *f = (BYTE *)g_base + RVA_WS_FREE;
        void *mbody = (void *)ws_malloc_body, *fbody = (void *)ws_free_body;
        m[0] = 0x48; m[1] = 0xB8; memcpy(m + 2, &mbody, 8); m[10] = 0xFF; m[11] = 0xE0;
        f[0] = 0x48; f[1] = 0xB8; memcpy(f + 2, &fbody, 8); f[10] = 0xFF; f[11] = 0xE0;
    }

    g_ws_malloc_calls = 0;
    g_ws_free_calls = 0;
    g_ws_malloc_size = 0;
    g_ws_freed = NULL;
    g_ws_malloc_ret = fail_alloc ? NULL : (void *)(g_base + RVA_WS_NEWOUT);
}

/* probe-page mapping status (set by setup_probe_pages, read by mode_probe) */
static int g_probe_ok;

/* ==== fake-page setup =====================================================
 * The fake game's pages are all written BEFORE the proxy DLL loads: the
 * loader's address book verifies those bytes at boot and splices the sites.
 * Each helper preserves the original print order (the harness greps them). */

/* the filter fn page + the whitelist/FORM data page. The fn page is mapped as
 * the whole 64K block 0x1A0000..0x1B0000 (VirtualAlloc's granularity): the M7-1
 * spec-table row accessor 0x1AACB0 lives in that block, so its page must be
 * mapped too (a 4K mapping at 0x1AA000 would round DOWN to 0x1A0000 and clash
 * with the filter's own page). */
static int setup_filter_pages(void)
{
    if (!map_fixed((UINT_PTR)g_base + 0x1A0000, 0x10000, PAGE_EXECUTE_READWRITE)) {
        printf("SKIP: could not map fn page at RVA 0x1A0000\n");
        return 0;
    }
    if (!map_fixed((UINT_PTR)g_base + 0x6FC000, 0x10000, PAGE_READWRITE)) {
        printf("SKIP: could not map data page at RVA 0x6FC000\n");
        return 0;
    }
    memset((BYTE *)g_base + 0x1A0000, 0xCC, 0x10000);
    /* the filter entry carries the real original prologue (expect-verified) */
    memcpy((BYTE *)g_base + RVA_FILTER, g_filter_orig, sizeof(g_filter_orig));
    /* the whitelist table carries its real original entries where recorded */
    memcpy((BYTE *)g_base + RVA_TABLE, g_table_e0, sizeof(g_table_e0));
    memcpy((BYTE *)g_base + RVA_TABLE + 24, g_table_e6, sizeof(g_table_e6));
    return 1;
}

/* the fake CFile::Open at RVA_CFILE: the 16-byte original prologue
 * (expect-verified by the loader's address book) + a tail jump into the
 * undo thunk; the E9 rel32 is patched after mapping (fixed page) */
static int setup_cfile_page(void)
{
    if (!map_fixed((UINT_PTR)g_base + 0x41D000, 0x1000, PAGE_EXECUTE_READWRITE)) {
        printf("SKIP: could not map cfile page at RVA 0x41D000\n");
        return 0;
    }
    memset((BYTE *)g_base + 0x41D000, 0xCC, 0x1000);
    memcpy((BYTE *)g_base + RVA_CFILE, g_cfile_orig, sizeof(g_cfile_orig));
    {
        BYTE *e9 = (BYTE *)g_base + RVA_CFILE + 16;
        intptr_t rel = (intptr_t)&cfile_undo_thunk - (intptr_t)(e9 + 5);
        e9[0] = 0xE9;
        memcpy((BYTE *)e9 + 1, &rel, 4);
    }
    return 1;
}

/* the four M4 fetch-probe fake pages (same mechanics as the cfile fake,
 * each with its own undo thunk — constexpr amounts above). VirtualAlloc
 * honors the base hint only at the 64K allocation granularity, so the
 * pages are aligned: pack/tex/cache anchors share the 0x300000 page,
 * the ccommon anchor sits in its own 0x2e0000 page. Mapping failure is
 * a hard error only for the probe mode; m2/m3 scenarios run without. */
static int setup_probe_pages(void)
{
    int probe_ok = 1;
    {
        struct { UINT_PTR page; UINT_PTR span; const char *label; } prooms[] = {
            { 0x2E0000, 0x10000, "0x2e0000" },
            { 0x300000, 0x10000, "0x300000" },
        };
        for (unsigned i = 0; i < sizeof(prooms) / sizeof(prooms[0]); i++) {
            void *got = map_fixed((UINT_PTR)g_base + prooms[i].page, prooms[i].span,
                                  PAGE_EXECUTE_READWRITE);
            if (!got) {
                printf("WARN: could not map probe page at RVA %s err=%lu\n",
                       prooms[i].label, (unsigned long)GetLastError());
                probe_ok = 0;
                continue;
            }
            memset((BYTE *)g_base + prooms[i].page, 0xCC, prooms[i].span);
        }
        struct { UINT_PTR rva; unsigned char *orig; size_t len; void *thunk; const char *label; } pfakes[] = {
            { RVA_PACK,  (unsigned char *)g_pack_orig,  sizeof(g_pack_orig),  (void *)&pack_undo_thunk,  "pack_fetch 0x306e20" },
            { RVA_TEX,   (unsigned char *)g_tex_orig,   sizeof(g_tex_orig),   (void *)&tex_undo_thunk,   "texture_fetch 0x3010d0" },
            { RVA_CCOM,  (unsigned char *)g_ccom_orig,  sizeof(g_ccom_orig),  (void *)&ccom_undo_thunk,  "ccommon_fetch 0x2ec0e0" },
            { RVA_CACHE, (unsigned char *)g_cache_orig, sizeof(g_cache_orig), (void *)&cache_undo_thunk, "fetch_cache_get 0x3089b0" },
        };
        for (unsigned i = 0; i < sizeof(pfakes) / sizeof(pfakes[0]); i++) {
            memcpy((BYTE *)g_base + pfakes[i].rva, pfakes[i].orig, pfakes[i].len);
            BYTE *tail = (BYTE *)g_base + pfakes[i].rva + pfakes[i].len;
            /* the two chkstk anchors end their record BEFORE the alloca —
             * complete the stack setup (mov eax,N; sub rsp,rax; the game's
             * own __chkstk is not mapped, the amount lives in the record)
             * so the undo thunk's add rsp matches. The trampoline back-jump
             * lands at entry + record_len — exactly here — keeping the
             * loader-side and fake-side paths equivalent. */
            if (pfakes[i].rva == RVA_TEX || pfakes[i].rva == RVA_CCOM) {
                tail[0] = 0xB8;
                *(unsigned int *)(tail + 1) = (pfakes[i].rva == RVA_TEX) ? 0x20e0u : 0x18b0u;
                tail[5] = 0x48; tail[6] = 0x2B; tail[7] = 0xE0; /* sub rsp,rax */
                tail += 8;
            }
            intptr_t rel = (intptr_t)pfakes[i].thunk - (intptr_t)(tail + 5);
            tail[0] = 0xE9;
            memcpy((BYTE *)tail + 1, &rel, 4);
            printf("fake: %s mapped\n", pfakes[i].label);
        }
    }
    return probe_ok;
}

/* ---- M7 purchase-window type-cycle fakes (builds.toml train_list_tab_*).
 * The tab rebuild entry carries its real 17-byte prologue + an undo thunk into
 * a plain C body; the scan site carries the 25 real bytes the fix rewrites; the
 * cave page stays 0xCC (the fix's expect bytes). The fake spec-table manager +
 * rows live in host-owned storage (the probe reads them). */
#define RVA_TAB_SCAN 0x19E7EEu   /* the vanilla-whitelist scan the fix rewrites */
#define RVA_TAB      0x19E760u   /* FUN_14019e760 type-tab rebuild entry */
#define RVA_TAB_CAVE 0x5B1E96u   /* the fix's relay cave */
#define RVA_WINDOW   0x2CA400u   /* train_list_big_list_window */
#define RVA_MGR      0xE14510u   /* DAT_140e14510 spec-table manager */
#define MGR_ROWS_N   400u
#define MGR_VEC_CAP  512u

/* M7b: the custom-train-form editor's SECOND vanilla-whitelist scan
 * (builds.toml train_form_scan + train_form_cave). Both live inside the
 * 0x190000 / 0x5B1000 pages setup_tab_pages already maps. */
#define RVA_FORM_SCAN 0x19E9DFu  /* the scan FUN_14019e950 the fix rewrites */
#define RVA_FORM_CAVE 0x5B2015u  /* the fix's relay cave */
#define FORM_FOUND_OFF 0x73u     /* 0x14019ea52 — the primary admit path */

static const unsigned char g_tab_scan_orig[25] = {
    0x48,0x8d,0x05,0xcb,0xd9,0x55,0x00,0x39,0x38,0x74,0x0e,0x48,0x83,0xc0,0x04,
    0x49,0x3b,0xc5,0x7c,0xf3,0xe9,0x9c,0x00,0x00,0x00
};
static const unsigned char g_form_scan_orig[20] = {
    0x48,0x8d,0x05,0xda,0xd7,0x55,0x00,0x39,0x18,0x74,0x68,
    0x48,0x83,0xc0,0x04,0x48,0x3b,0xc1,0x7c,0xf3
};
static const unsigned char g_tab_orig[17] = {
    0x40,0x53,0x57,0x41,0x54,0x41,0x56,0x41,0x57,0x48,0x83,0xEC,0x20,0x48,0x8B,0x41,0x48
};
static const unsigned char g_window_orig[19] = {
    0x48,0x8B,0xC4,0x48,0x89,0x48,0x08,0x55,0x53,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57
};

static unsigned *g_mgr_rows;   /* MGR_ROWS_N * 0x140 bytes, heap (image-size neutral) */
static unsigned *g_vec_unf, *g_vec_flt, *g_vec_typ;

extern void tab_undo_thunk(void);
extern void window_undo_thunk(void);

__asm__(
    ".globl tab_undo_thunk\n"
    "tab_undo_thunk:\n"
    "addq $0x20, %rsp\n"
    "popq %r15\n"
    "popq %r14\n"
    "popq %r12\n"
    "popq %rdi\n"
    "popq %rbx\n"
    "jmp tab_body\n"
    ".globl window_undo_thunk\n"
    "window_undo_thunk:\n"
    "popq %r15\n"
    "popq %r14\n"
    "popq %r13\n"
    "popq %r12\n"
    "popq %rdi\n"
    "popq %rsi\n"
    "popq %rbx\n"
    "popq %rbp\n"
    "jmp window_body\n");

/* M7b: run the rewritten custom-train-form scan with the game's stack shape.
 * The scan is reached by fall-through in the game with RSP == 0 (mod 16), so
 * the thunk pushes rbx (RSP 8 -> 0) and jumps — the scan's own `call cave`
 * then presents the ABI's RSP == 8 at the composed-keep stub. The branch
 * markers pop rbx back and return, so the C caller reads the decision in eax. */
void *g_form_scan_ptr = 0;   /* set by mode_formfix; the thunk jumps here */
extern unsigned form_scan_call(unsigned id);
__asm__(
    ".text\n"
    ".globl form_scan_call\n"
    "form_scan_call:\n"
    "  pushq %rbx\n"
    "  movl %ecx, %ebx\n"   /* Win64: the first integer arg is in RCX */
    "  jmpq *g_form_scan_ptr(%rip)\n");

__attribute__((used, noinline)) void tab_body(void *manager, const int *tab)
{
    /* replicate the PATCHED FUN_14019e760: valid row + composed membership
     * (through the loader's relay cave) + tab type match -> append to +0x48 */
    if (*(BYTE *)(g_base + RVA_TAB_SCAN) != 0x4D) {
        printf("tab_body: fix not installed — no-op\n");
        return;
    }
    typedef unsigned char (*mem_fn)(unsigned);
    mem_fn mem = (mem_fn)(g_base + RVA_TAB_CAVE);
    BYTE *m = (BYTE *)manager;
    BYTE *rows = *(BYTE **)(m + 0x18);
    BYTE *rows_end = *(BYTE **)(m + 0x20);
    unsigned nrows = (unsigned)((rows_end - rows) / 0x140);
    unsigned *out = *(unsigned **)(m + 0x48);
    *(void **)(m + 0x50) = out;
    for (unsigned i = 0; i < nrows; i++) {
        BYTE *row = rows + (size_t)i * 0x140;
        if (!row[0]) continue;
        if (!mem(i)) continue;
        int ty = *(int *)(row + 4), ok = 0;
        for (int k = 0; k < 10; k++) {
            if (tab[k] == -1) break;
            if (tab[k] == ty) { ok = 1; break; }
        }
        if (!ok) continue;
        *out++ = i;
    }
    *(void **)(m + 0x50) = out;
    printf("tab_body: called (composed rebuild)\n");
}
__attribute__((used, noinline)) void window_body(void *obj)
{
    (void)obj;
    printf("window_body: called\n");
}

/* map the M7 fake pages and lay down the expect bytes + undo thunks. Called
 * before the proxy loads, like every other fake page. */
static int setup_tab_pages(void)
{
    struct { UINT_PTR page; UINT_PTR span; const char *label; } prooms[] = {
        { 0x190000, 0x10000, "0x190000" },  /* tab rebuild + scan */
        { 0x5B1000, 0x10000, "0x5b1000" },  /* the relay cave */
        { 0x2C0000, 0x10000, "0x2c0000" },  /* the window */
        { 0xE14000, 0x10000, "0xe14000" },  /* the manager */
    };
    for (unsigned i = 0; i < sizeof(prooms) / sizeof(prooms[0]); i++) {
        if (!map_fixed((UINT_PTR)g_base + prooms[i].page, prooms[i].span, PAGE_EXECUTE_READWRITE)) {
            printf("WARN: could not map tab page at RVA %s err=%lu\n",
                   prooms[i].label, (unsigned long)GetLastError());
            return 0;
        }
        memset((BYTE *)g_base + prooms[i].page, 0xCC, prooms[i].span);
    }
    /* the fake manager's rows + index vectors: heap, so the host image size
     * stays below the M4 inject fake runner (RVA 0x504c0) */
    g_mgr_rows = (unsigned *)VirtualAlloc(NULL, (SIZE_T)MGR_ROWS_N * 0x140,
                                          MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    g_vec_unf = (unsigned *)VirtualAlloc(NULL, MGR_VEC_CAP * 4, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    g_vec_flt = (unsigned *)VirtualAlloc(NULL, MGR_VEC_CAP * 4, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    g_vec_typ = (unsigned *)VirtualAlloc(NULL, MGR_VEC_CAP * 4, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!g_mgr_rows || !g_vec_unf || !g_vec_flt || !g_vec_typ) {
        printf("WARN: could not allocate the fake manager storage\n");
        return 0;
    }
    memcpy((BYTE *)g_base + RVA_TAB_SCAN, g_tab_scan_orig, sizeof(g_tab_scan_orig));    memcpy((BYTE *)g_base + RVA_TAB, g_tab_orig, sizeof(g_tab_orig));
    {
        BYTE *t = (BYTE *)g_base + RVA_TAB + sizeof(g_tab_orig);
        intptr_t rel = (intptr_t)&tab_undo_thunk - (intptr_t)(t + 5);
        t[0] = 0xE9;
        memcpy(t + 1, &rel, 4);
    }
    memcpy((BYTE *)g_base + RVA_WINDOW, g_window_orig, sizeof(g_window_orig));
    {
        BYTE *w = (BYTE *)g_base + RVA_WINDOW + sizeof(g_window_orig);
        intptr_t rel = (intptr_t)&window_undo_thunk - (intptr_t)(w + 5);
        w[0] = 0xE9;
        memcpy(w + 1, &rel, 4);
    }
    /* M7b: the custom-train-form scan + the two branch-target markers the
     * rewritten scan reaches: +0x14 (fall-through = not kept) and +0x73
     * (found = kept). `mov eax,N; pop rbx; ret` — pop rbx undoes the call
     * thunk's push, so the host reads the scan's own decision. */
    memcpy((BYTE *)g_base + RVA_FORM_SCAN, g_form_scan_orig, sizeof(g_form_scan_orig));
    {
        BYTE *fall = (BYTE *)g_base + RVA_FORM_SCAN + sizeof(g_form_scan_orig);
        BYTE *found = (BYTE *)g_base + RVA_FORM_SCAN + FORM_FOUND_OFF;
        fall[0] = 0xB8; fall[1] = 2; fall[2] = 0; fall[3] = 0; fall[4] = 0; fall[5] = 0x5B; fall[6] = 0xC3;
        found[0] = 0xB8; found[1] = 1; found[2] = 0; found[3] = 0; found[4] = 0; found[5] = 0x5B; found[6] = 0xC3;
    }
    return 1;
}

/* the fake spec-table manager: 400 valid rows (all type 0) + the three index
 * vectors. +0x48 is pre-seeded with the composed keep-set the loader's filter
 * would have produced (ids 4/16/42 are vanilla-whitelisted, 329 is not). */
static void mgr_setup(void)
{
    BYTE *m = (BYTE *)g_base + RVA_MGR;
    for (unsigned i = 0; i < MGR_ROWS_N; i++) {
        BYTE *row = (BYTE *)g_mgr_rows + (size_t)i * 0x140;
        row[0] = 1;
        *(int *)(row + 4) = 0;
    }
    *(void **)(m + 0x18) = g_mgr_rows;
    *(void **)(m + 0x20) = (BYTE *)g_mgr_rows + MGR_ROWS_N * 0x140;
    *(void **)(m + 0x30) = g_vec_unf;
    *(void **)(m + 0x38) = g_vec_unf;
    *(void **)(m + 0x40) = g_vec_unf + MGR_VEC_CAP;
    g_vec_flt[0] = 4; g_vec_flt[1] = 16; g_vec_flt[2] = 42; g_vec_flt[3] = 329;
    *(void **)(m + 0x48) = g_vec_flt;
    *(void **)(m + 0x50) = g_vec_flt + 4;
    *(void **)(m + 0x58) = g_vec_flt + MGR_VEC_CAP;
    *(void **)(m + 0x60) = g_vec_typ;
    *(void **)(m + 0x68) = g_vec_typ;
    *(void **)(m + 0x70) = g_vec_typ + MGR_VEC_CAP;
}

/* ---- M7-2 save-path probe fakes (builds.toml mmf_paged_write /
 * mmf_paged_write_bulk / save_mmf_write_block / mmf_read_u32_vector).
 * The four save-side mmf helpers carry their real position-independent
 * prologues (expect-verified by the loader's address book) + an E9 into an
 * undo thunk that reverses the prologue's single push and jumps to a plain C
 * body. The caller stubs live in the already-mapped 0x300000 block at fixed
 * RVAs, so the probe's logged "caller" is a deterministic in-image RVA. */
#define RVA_SAVE_PW     0x308FE0u   /* mmf_paged_write (the byte-level writer) */
#define RVA_SAVE_PWB    0x308E20u   /* mmf_paged_write_bulk (the bulk writer) */
#define RVA_SAVE_WBLOCK 0x41500u    /* save_mmf_write_block (the u32-vector writer) */
#define RVA_SAVE_RVEC   0x41820u    /* mmf_read_u32_vector (the u32-vector READER) */
#define RVA_SAVE_STUB   0x300000u   /* 4 caller stubs, 0x10 apart */

static const unsigned char g_save_pw_orig[17] = {
    0x48,0x89,0x6c,0x24,0x10,0x48,0x89,0x74,0x24,0x18,0x48,0x89,0x7c,0x24,0x20,0x41,0x56
};
static const unsigned char g_save_pwb_orig[17] = {
    0x48,0x89,0x5c,0x24,0x10,0x48,0x89,0x74,0x24,0x18,0x48,0x89,0x7c,0x24,0x20,0x41,0x56
};
static const unsigned char g_save_wblock_orig[16] = {
    0x48,0x89,0x6c,0x24,0x18,0x48,0x89,0x74,0x24,0x20,0x48,0x89,0x4c,0x24,0x08,0x57
};
static const unsigned char g_save_rvec_orig[16] = {
    0x48,0x89,0x5c,0x24,0x10,0x48,0x89,0x74,0x24,0x18,0x48,0x89,0x4c,0x24,0x08,0x57
};

static unsigned long g_save_calls;

extern void save_pw_undo_thunk(void);
extern void save_pwb_undo_thunk(void);
extern void save_wblock_undo_thunk(void);
extern void save_rvec_undo_thunk(void);

__asm__(
    ".text\n"
    ".globl save_pw_undo_thunk\n"
    "save_pw_undo_thunk:\n"
    "popq %r14\n"
    "jmp save_pw_body\n"
    ".globl save_pwb_undo_thunk\n"
    "save_pwb_undo_thunk:\n"
    "popq %r14\n"
    "jmp save_pwb_body\n"
    ".globl save_wblock_undo_thunk\n"
    "save_wblock_undo_thunk:\n"
    "popq %rdi\n"
    "jmp save_wblock_body\n"
    ".globl save_rvec_undo_thunk\n"
    "save_rvec_undo_thunk:\n"
    "popq %rdi\n"
    "jmp save_rvec_body\n");

__attribute__((used, noinline)) void save_pw_body(void *ring, const void *src, unsigned size)
{
    (void)ring; (void)src;
    g_save_calls++;
    printf("save_pw_body: called size=0x%x\n", size);
}
__attribute__((used, noinline)) void save_pwb_body(void *ring, const void *src, unsigned size)
{
    (void)ring; (void)src;
    g_save_calls++;
    printf("save_pwb_body: called size=0x%x\n", size);
}
__attribute__((used, noinline)) void save_wblock_body(unsigned long a1, const void *vec)
{
    (void)a1; (void)vec;
    g_save_calls++;
    printf("save_wblock_body: called\n");
}
__attribute__((used, noinline)) void save_rvec_body(unsigned long a1, const void *vec)
{
    (void)a1; (void)vec;
    g_save_calls++;
    printf("save_rvec_body: called\n");
}

/* lay down the four fake prologues + their E9 tails, and build the four
 * in-image caller stubs (`movabs r11,target; call r11; ret`). Both pages are
 * already mapped by the other setups (0x410000 via setup_cfile_page's 64K
 * granularity, 0x300000 via setup_probe_pages), so no new mapping is needed. */
static void setup_save_pages(void)
{
    /* the 0x41500 / 0x41820 anchors live in their own page (the cfile fake
     * maps 0x41d000, a different page). VirtualAlloc honours the exact
     * page-aligned base here (verified: the cfile fake's 0x41d40c is mapped
     * while 0x41500 is not), so one 4K page covers both. */
    if (!map_fixed((UINT_PTR)g_base + 0x41000, 0x1000, PAGE_EXECUTE_READWRITE)) {
        printf("WARN: could not map save page at RVA 0x41000 err=%lu\n",
               (unsigned long)GetLastError());
        return;
    }
    struct { UINT_PTR rva; const unsigned char *orig; size_t len; void *thunk; } fakes[] = {
        { RVA_SAVE_PW,     g_save_pw_orig,     sizeof(g_save_pw_orig),     (void *)&save_pw_undo_thunk },
        { RVA_SAVE_PWB,    g_save_pwb_orig,    sizeof(g_save_pwb_orig),    (void *)&save_pwb_undo_thunk },
        { RVA_SAVE_WBLOCK, g_save_wblock_orig, sizeof(g_save_wblock_orig), (void *)&save_wblock_undo_thunk },
        { RVA_SAVE_RVEC,   g_save_rvec_orig,   sizeof(g_save_rvec_orig),   (void *)&save_rvec_undo_thunk },
    };
    for (unsigned i = 0; i < sizeof(fakes) / sizeof(fakes[0]); i++) {
        memcpy((BYTE *)g_base + fakes[i].rva, fakes[i].orig, fakes[i].len);
        BYTE *tail = (BYTE *)g_base + fakes[i].rva + fakes[i].len;
        intptr_t rel = (intptr_t)fakes[i].thunk - (intptr_t)(tail + 5);
        tail[0] = 0xE9;
        memcpy(tail + 1, &rel, 4);
    }
    /* the caller stubs: movabs r11,<entry>; sub rsp,0x28; call r11; add rsp,0x28;
     * ret. The 0x28 reserves the Win64 32-byte home area (the helpers'
     * prologues store rbp/rsi/rdi at [rsp+0x10]/[rsp+0x18]/[rsp+0x20] — without
     * it they overwrite this stub's frame) and leaves RSP ≡ 0 before the call,
     * so the hooked entry sees the ABI's RSP ≡ 8. The return address lands at
     * stub+17, i.e. RVA 0x300011 / 0x300031 / 0x300051 / 0x300071. */
    UINT_PTR targets[4] = { RVA_SAVE_PW, RVA_SAVE_PWB, RVA_SAVE_WBLOCK, RVA_SAVE_RVEC };
    for (unsigned i = 0; i < 4; i++) {
        BYTE *s = (BYTE *)g_base + RVA_SAVE_STUB + i * 0x20;
        s[0] = 0x49; s[1] = 0xBB;
        void *t = (void *)(g_base + targets[i]);
        memcpy(s + 2, &t, 8);
        s[10] = 0x48; s[11] = 0x83; s[12] = 0xEC; s[13] = 0x28; /* sub rsp,0x28 */
        s[14] = 0x41; s[15] = 0xFF; s[16] = 0xD3;               /* call r11 */
        s[17] = 0x48; s[18] = 0x83; s[19] = 0xC4; s[20] = 0x28; /* add rsp,0x28 */
        s[21] = 0xC3;                                           /* ret */
    }
}

/* ---- M7-0 save-id clamp guard fakes (builds.toml save_group_head /
 * save_group_head_cave / save_car_read / save_car_read_cave). The head site
 * carries the 7 real bytes the rewrite overwrites (+ a `ret` continuation at
 * +7); the car site carries the real E8 whose target (0x308f20) is the fake
 * paged reader below (+ a `ret` continuation at +5); both caves stay 0xCC.
 * The guard is ALWAYS ON, so these pages are mapped unconditionally — every
 * scenario's boot installs it. The caves live in 64K blocks no other fake uses
 * (0x5d0000 / 0x5c0000); the sites in 0x1b0000 / 0x60000. */
#define RVA_CLAMP_HEAD      0x1B6F0Cu  /* the head row-offset math (7 bytes) */
#define RVA_CLAMP_HEAD_CAVE 0x5D28F0u  /* the head relay cave (16-byte CC run) */
#define RVA_CLAMP_CAR       0x61F8Bu   /* the car-block read E8 */
#define RVA_CLAMP_CAR_CAVE  0x66790u   /* the car relay cave (16-byte CC run) */
#define RVA_CLAMP_READ      0x308F20u  /* mmf_paged_read (the car site's target) */
#define CLAMP_ROWS          421u       /* the fake spec-vector row count */
#define CLAMP_HEAD_TARGET   417u       /* the engine's auto-named invalid row */
#define CLAMP_CAR_TARGET    160u       /* FORM_DUMMY_03 */

static const unsigned char g_clamp_head_orig[7] = { 0x49,0x63,0xC6,0x48,0x8D,0x0C,0x80 };
static const unsigned char g_clamp_car_orig[5]  = { 0xE8,0x90,0x6F,0x2A,0x00 };

static const unsigned char *g_clamp_read_src;  /* what the fake reader copies */

__attribute__((used, noinline)) void clamp_read_body(void *ring, void *dest, unsigned size)
{
    (void)ring;
    if (g_clamp_read_src && size <= 0x1000) memcpy(dest, g_clamp_read_src, size);
}

static int setup_clamp_pages(void)
{
    /* one entry per 64K host block: the head site, the head cave, and the car
     * site + car cave (which share the 0x60000 block) */
    struct { UINT_PTR page; UINT_PTR span; const char *label; } crooms[] = {
        { 0x1B6000, 0x1000,  "0x1b6000" },  /* the head site */
        { 0x5D2000, 0x1000,  "0x5d2000" },  /* the head cave */
        { 0x60000,  0x10000, "0x60000"  },  /* the car site + the car cave */
    };
    int ok = 1;
    for (unsigned i = 0; i < sizeof(crooms) / sizeof(crooms[0]); i++) {
        if (!map_fixed((UINT_PTR)g_base + crooms[i].page, crooms[i].span,
                       PAGE_EXECUTE_READWRITE)) {
            printf("WARN: could not map clamp page at RVA %s err=%lu\n",
                   crooms[i].label, (unsigned long)GetLastError());
            ok = 0;
            continue;
        }
        memset((BYTE *)g_base + crooms[i].page, 0xCC, crooms[i].span);
    }
    if (!ok) return 0;
    /* the head site: the 7 real bytes + a `ret` continuation at +7 (the shim's
     * ret lands on the 2 nops, falls through to this ret and returns to the
     * host's call thunk) */
    memcpy((BYTE *)g_base + RVA_CLAMP_HEAD, g_clamp_head_orig, sizeof(g_clamp_head_orig));
    ((BYTE *)g_base + RVA_CLAMP_HEAD)[7] = 0xC3;
    /* the car site: the real E8 + a `ret` continuation at +5 */
    memcpy((BYTE *)g_base + RVA_CLAMP_CAR, g_clamp_car_orig, sizeof(g_clamp_car_orig));
    ((BYTE *)g_base + RVA_CLAMP_CAR)[5] = 0xC3;
    /* the fake mmf_paged_read at 0x308f20 (inside setup_probe_pages' 0x300000
     * span, between the save fakes 0x308e20/0x308fe0): movabs rax,<body>; jmp rax */
    {
        BYTE *shim = (BYTE *)g_base + RVA_CLAMP_READ;
        intptr_t target = (intptr_t)&clamp_read_body;
        shim[0] = 0x48; shim[1] = 0xB8;
        memcpy(shim + 2, &target, 8);
        shim[10] = 0xFF; shim[11] = 0xE0;
    }
    return 1;
}

/* the head site is reached by FALL-THROUGH in the game with RSP == 0 (mod 16),
 * so the thunk keeps RSP == 8 before its `call` (entry 8 -> two pushes -> 8);
 * r14d carries the id in and the clamped id out. */
void *g_clamp_head_ptr = 0;
extern unsigned clamp_head_call(unsigned id);
__asm__(
    ".text\n"
    ".globl clamp_head_call\n"
    "clamp_head_call:\n"
    "  pushq %r14\n"
    "  pushq %rbx\n"
    "  movl %ecx, %r14d\n"
    "  callq *g_clamp_head_ptr(%rip)\n"
    "  movl %r14d, %eax\n"
    "  popq %rbx\n"
    "  popq %r14\n"
    "  retq\n");

/* the car site is also fall-through (RSP == 0 at its E8). The thunk is entered
 * with RSP == 8 and its single `call` makes the site see RSP == 0; arguments
 * rcx/rdx/r8 pass straight through, the result comes back in rax. */
void *g_clamp_car_ptr = 0;
extern unsigned long clamp_car_call(void *ring, void *dest, unsigned size);
__asm__(
    ".text\n"
    ".globl clamp_car_call\n"
    "clamp_car_call:\n"
    "  callq *g_clamp_car_ptr(%rip)\n"
    "  retq\n");

/* ---- M7-1 direct spec-row injection fakes (builds.toml spec_table_resize /
 * spec_table_realloc / spec_table_row_accessor / row_name_ctor). The three
 * CALLED helpers carry their real prologues + an E9 into an undo thunk that
 * reverses the prologue and jumps to a plain C body; the accessor is
 * position-independent and self-contained (lea/shl/add/ret), so its real 12
 * bytes run as-is — no fake at all. The resize site is HOOKED by the loader
 * (trampoline), so its E9 at +19 is reached only through the trampoline. */
#define RVA_ROWS_RESIZE   0x19F150u  /* std::vector<A9_SpecRow_140>::resize(421) */
#define RVA_ROWS_REALLOC  0x19FC50u  /* the vector's reallocate (game allocator) */
#define RVA_ROWS_ACCESSOR 0x1AACB0u  /* vec.begin + id*0x140 (self-contained) */
#define RVA_ROWS_CTOR     0x372F0u   /* the builder's auto-name ctor */
#define ROWS_TABLE_N      421u       /* the fake spec-vector row count */
#define ROWS_TEMPLATE_ID  0u         /* the shipped valid template row */
#define ROWS_RESERVED_ID  421u       /* the first reserved id (= the live count) */
#define ROWS_ROW_STRIDE   0x140u
#define ROWS_PFORMNAME    0x138u
#define ROWS_NAME_JP      0xa8u
#define ROWS_NAME_EN      0xb0u
#define ROWS_MARKER_OFF   0x100u     /* a clone-property marker the host sets */
#define ROWS_MARKER       0xA9C0DE01u
#define ROWS_NAMEALT      0xf0u      /* awNameAlt[0] (M7-2 clone-fix exercise) */

static const unsigned char g_rows_resize_orig[19] = {
    0x48,0x89,0x5c,0x24,0x08,0x48,0x89,0x74,0x24,0x10,0x57,0x48,0x83,0xec,0x30,
    0x4c,0x8b,0x41,0x08
};
static const unsigned char g_rows_realloc_orig[20] = {
    0x41,0x56,0x48,0x83,0xec,0x40,0x48,0xc7,0x44,0x24,0x30,0xfe,0xff,0xff,0xff,
    0x48,0x89,0x5c,0x24,0x50
};
static const unsigned char g_rows_accessor_orig[12] = {
    0x48,0x8d,0x04,0x92,0x48,0xc1,0xe0,0x06,0x48,0x03,0x01,0xc3
};
static const unsigned char g_rows_ctor_orig[19] = {
    0x48,0x89,0x54,0x24,0x10,0x4c,0x89,0x44,0x24,0x18,0x4c,0x89,0x4c,0x24,0x20,
    0x48,0x83,0xec,0x28
};

static unsigned char *g_rows_base;      /* the fake 421-row spec table */
static unsigned g_rows_resize_calls, g_rows_realloc_calls, g_rows_ctor_calls;
static unsigned long g_rows_realloc_new;
static void *g_rows_new_block;
static int g_rows_ctor_null;            /* the refusal scenario: the ctor returns NULL */

extern void rows_resize_undo(void);
extern void rows_realloc_undo(void);
extern void rows_ctor_undo(void);

__asm__(
    ".globl rows_resize_undo\n"
    "rows_resize_undo:\n"
    "  movq 0x40(%rsp), %rbx\n"
    "  movq 0x48(%rsp), %rsi\n"
    "addq $0x30, %rsp\n"
    "popq %rdi\n"
    "jmp rows_resize_body\n"
    ".globl rows_realloc_undo\n"
    "rows_realloc_undo:\n"
    "addq $0x40, %rsp\n"
    "popq %r14\n"
    "jmp rows_realloc_body\n"
    ".globl rows_ctor_undo\n"
    "rows_ctor_undo:\n"
    "addq $0x28, %rsp\n"
    "jmp rows_ctor_body\n");

/* the fake resize: the real one re-pins the vector to EXACTLY 421 rows, which
 * is why a growth done before it is silently undone */
__attribute__((used, noinline)) void rows_resize_body(void *vec)
{
    g_rows_resize_calls++;
    *(void **)vec = g_rows_base;
    *(void **)((char *)vec + 8) = g_rows_base + (size_t)ROWS_TABLE_N * ROWS_ROW_STRIDE;
    *(void **)((char *)vec + 0x10) = g_rows_base + (size_t)ROWS_TABLE_N * ROWS_ROW_STRIDE;
    printf("rows resize_body: re-pinned the vector to %u rows\n", ROWS_TABLE_N);
}

/* the fake reallocate: allocate (the host's allocator stands in for the game's
 * 0x14040a838), copy the live rows, set {begin,end,cap} — the same shape as the
 * real 0x14019fc50 */
__attribute__((used, noinline)) void rows_realloc_body(void *vec, unsigned long new_count)
{
    unsigned char *begin = *(unsigned char **)vec;
    unsigned char *end = *(unsigned char **)((char *)vec + 8);
    unsigned long old_count = (unsigned long)((end - begin) / ROWS_ROW_STRIDE);
    unsigned char *nb = (unsigned char *)VirtualAlloc(NULL, new_count * ROWS_ROW_STRIDE,
                                                      MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    g_rows_realloc_calls++;
    g_rows_realloc_new = new_count;
    g_rows_new_block = nb;
    if (!nb) { printf("rows realloc_body: alloc FAILED\n"); return; }
    memcpy(nb, begin, (size_t)old_count * ROWS_ROW_STRIDE);
    *(void **)vec = nb;
    *(void **)((char *)vec + 8) = nb + old_count * ROWS_ROW_STRIDE;
    *(void **)((char *)vec + 0x10) = nb + new_count * ROWS_ROW_STRIDE;
    /* the accessor derives the row address from the vector, so the stand-in must
     * follow the reallocation the way the game's own vector does — otherwise the
     * loader's row writes and the host's verification would read two different
     * blocks (the old one's slack). */
    g_rows_base = nb;
    printf("rows realloc_body: %lu -> %lu rows (game-allocator stand-in, block=%p)\n",
           old_count, new_count, (void *)nb);
}

/* the fake name constructor: the real one builds a valid A9_RcStr from a wide
 * format + one integer arg (the builder's `form_id_%d`). `g_rows_ctor_null`
 * models the failure the mechanism must refuse. */
/* `row_name_ctor` is the printf-style wide-string constructor. The M7 row build
 * calls it with a TEXT format and an unused value; the car build (and the lane
 * re-format) call it with a static `%s` and a wide-string POINTER in the value
 * slot — so the value must be read as a pointer when the format says `%s`. */
__attribute__((used, noinline)) void **rows_ctor_body(void **dest, const wchar_t *fmt,
                                                     unsigned long long value)
{
    static wchar_t bufs[32][64];
    static unsigned n;
    int is_text = (fmt && wcscmp(fmt, L"%s") == 0);
    g_rows_ctor_calls++;
    if (g_rows_ctor_null && !is_text) { *dest = NULL; return NULL; }
    {
        wchar_t *b = bufs[n++ & 31];
        if (is_text) {
            const wchar_t *src = (const wchar_t *)(unsigned long long)value;
            swprintf(b, 64, L"%s", src ? src : L"(null)");
        } else {
            swprintf(b, 64, fmt ? fmt : L"%u", (int)value);
        }
        *dest = b;
        /* two shapes: the engine's own `form_id_%d` / `modrow_%d` auto-name (the value
         * is an id) and the `%s` assignment the M7-2 lanes and the car-build cells use
         * (the value is a wide-string pointer). Print what the call actually carried. */
        if (is_text) {
            printf("rows ctor_body: fmt=%ls text -> %ls\n", fmt, b);
        } else {
            printf("rows ctor_body: fmt=%ls id=%d -> %ls\n", fmt ? fmt : L"(null)",
                   (int)value, b);
        }
    }
    return dest;
}

/* the shipped valid template row (row 0): bValid=1, a car, three names and a
 * clone marker the harness reads back from the injected row */
static void rows_make_template(void)
{
    unsigned char *r = g_rows_base + ROWS_TEMPLATE_ID * ROWS_ROW_STRIDE;
    /* The live donor row's per-car block is ZERO for the consist lengths it does not
     * describe (AR4's own 1..3-car records are zero; only 4..10 carry data), and the
     * loader deliberately leaves those lengths alone so the injected row inherits
     * them. The fake row must therefore start zeroed, not with the canary fill. */
    memset(r + 0x39, 0, 0x64);
    static wchar_t tname[] = L"FORM_TEMPLATE";
    static wchar_t jp[] = L"TEMPLATE_JP";
    static wchar_t en[] = L"TEMPLATE_EN";
    r[0] = 1;
    *(unsigned int *)(r + 4) = 0;
    r[0x38] = 1;
    *(void **)(r + ROWS_NAME_JP) = jp;
    *(void **)(r + ROWS_NAME_EN) = en;
    *(void **)(r + ROWS_PFORMNAME) = tname;
    *(unsigned int *)(r + ROWS_MARKER_OFF) = ROWS_MARKER;
    /* two extra non-NULL lanes so the M7-2 clone fix has something to clone
     * (awName[2] and awNameAlt[0]) */
    *(void **)(r + ROWS_NAME_JP + 2 * 8) = jp;
    *(void **)(r + ROWS_NAMEALT) = jp;
}

/* write the fake pages and point the manager's rows vector at the fake 421-row
 * table. Called by mode_rows AFTER the DLL load: the loader verifies these
 * anchors at export-call time, not at boot. The 0x1A0000 / 0x190000 / 0xE14000
 * blocks are already mapped (setup_filter_pages / setup_tab_pages); the
 * row_name_ctor page (0x30000..0x40000) is free only while SizeOfImage < 0x30000
 * — the host keeps its big FORM buffer on the heap for exactly that reason. */
static int setup_rows_pages(void)
{
    if (!map_fixed((UINT_PTR)g_base + 0x30000, 0x10000, PAGE_EXECUTE_READWRITE)) {
        printf("WARN: could not map the rows ctor page at RVA 0x30000 err=%lu\n",
               (unsigned long)GetLastError());
        return 0;
    }
    memset((BYTE *)g_base + 0x30000, 0xCC, 0x10000);
    /* the accessor runs as-is (self-contained) */
    memcpy((BYTE *)g_base + RVA_ROWS_ACCESSOR, g_rows_accessor_orig,
           sizeof(g_rows_accessor_orig));
    /* the three called helpers: real prologue + E9 undo thunk */
    struct { UINT_PTR rva; const unsigned char *orig; size_t len; void *undo; } rfakes[] = {
        { RVA_ROWS_RESIZE,  g_rows_resize_orig,  sizeof(g_rows_resize_orig),  (void *)&rows_resize_undo },
        { RVA_ROWS_REALLOC, g_rows_realloc_orig, sizeof(g_rows_realloc_orig), (void *)&rows_realloc_undo },
        { RVA_ROWS_CTOR,    g_rows_ctor_orig,    sizeof(g_rows_ctor_orig),    (void *)&rows_ctor_undo },
    };
    for (unsigned i = 0; i < sizeof(rfakes) / sizeof(rfakes[0]); i++) {
        memcpy((BYTE *)g_base + rfakes[i].rva, rfakes[i].orig, rfakes[i].len);
        BYTE *t = (BYTE *)g_base + rfakes[i].rva + rfakes[i].len;
        intptr_t rel = (intptr_t)rfakes[i].undo - (intptr_t)(t + 5);
        t[0] = 0xE9;
        memcpy(t + 1, &rel, 4);
    }
    /* the fake spec table: 421 rows, all zero (invalid) except the template */
    g_rows_base = (unsigned char *)VirtualAlloc(NULL, (SIZE_T)ROWS_TABLE_N * ROWS_ROW_STRIDE,
                                                MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!g_rows_base) { printf("WARN: rows table alloc failed\n"); return 0; }
    memset(g_rows_base, 0, (SIZE_T)ROWS_TABLE_N * ROWS_ROW_STRIDE);
    rows_make_template();
    /* the manager's rows vector {begin,end,cap} = the fake table */
    {
        BYTE *m = (BYTE *)g_base + RVA_MGR;
        *(void **)(m + 0x18) = g_rows_base;
        *(void **)(m + 0x20) = g_rows_base + (size_t)ROWS_TABLE_N * ROWS_ROW_STRIDE;
        *(void **)(m + 0x28) = g_rows_base + (size_t)ROWS_TABLE_N * ROWS_ROW_STRIDE;
        *(unsigned int *)(m + 0x90) = 0; /* the display-name lane: JP */
    }
    g_rows_resize_calls = g_rows_realloc_calls = g_rows_ctor_calls = 0;
    g_rows_realloc_new = 0;
    g_rows_new_block = NULL;
    g_rows_ctor_null = 0;
    return 1;
}

/* ---- M7-2 train-injection fakes. The `[[train]]` compiler is pure (a9-mod);
 * this mode drives the BOOT half: the engine's real boot (which runs when the
 * DLL loads) compiles the scenario's `[[train]]`, installs the composed
 * keep-set (with the reserved ids), the overlay plan and the injection hook.
 * The host then calls the `train_list_builder` site to fire the hook.
 *
 * Two anchors need fake pages the other modes never map:
 *   - `train_list_builder` 0x19D9C0 (the injection site; the 0x190000 block is
 *     already mapped by setup_tab_pages — we only write the site bytes);
 *   - `wstr_clone_ref` 0x1D7D0 (the name-pointer clone fix; CALLED, never
 *     hooked). Its RVA falls in the image's own tail (SizeOfImage 0x21000), so
 *     the page is committed rather than freshly mapped — the host writes the
 *     recorded prologue over it, exactly like the other fakes. */
#define RVA_TLB          0x19D9C0u   /* train_list_builder (injection site) */
#define RVA_CLONEREF     0x1D7D0u    /* wstr_clone_ref (called) */
#define RVA_ANNOT_BOUND  0x3B0A7Fu   /* preview_form_list_annotator: CMP EBP,0x1a1 */
#define RVA_MODEL_FETCH    0x19B9F0u /* train_model_fetch(wrapper,&path_cell,row,flag) */
#define RVA_MODEL_REGISTER 0x97A90u  /* model_register(manager, wrapper) */
#define RVA_MODEL_INDEX    0x19F120u /* model_index_alloc(manager) -> index */
#define RVA_RCSTR_CTOR     0x1D940u  /* rcstr_ctor(dest): an empty A9_RcStr at dest */
#define RVA_RCSTR_CALLTGT  0x40BA84u /* the accessor rcstr_ctor's prologue calls */
/* The car build CALLS these four game functions — it hooks nothing — so the fakes
 * must both satisfy the address book's expect bytes (the loader verifies them
 * before it will call) and survive execution: the recorded prologue runs, a
 * synthetic byte completes the cut last instruction, then an E9 jumps to a body
 * that undoes the prologue's frame and restores the register it pushed. */
/* These must be EXACTLY the address book's expect lengths: a longer array
 * appends implicit 0x00 bytes that then execute (`add [rax],al`) before the E9. */
static const unsigned char g_fetch_orig[21] = {
    0x48,0x89,0x5c,0x24,0x08,0x48,0x89,0x74,0x24,0x10,0x48,0x89,0x7c,0x24,0x18,0x41,0x56,
    0x48,0x83,0xec,0x20
};
static const unsigned char g_register_orig[16] = {
    0x48,0x89,0x5c,0x24,0x08,0x57,0x48,0x83,0xec,0x20,0x48,0x8b,0xd9,0x48,0x8b,0x49
};
static const unsigned char g_index_orig[16] = {
    0x48,0x8b,0x51,0x08,0x48,0xb8,0xab,0xaa,0xaa,0xaa,0xaa,0xaa,0xaa,0x2a,0x48,0x2b
};
static const unsigned char g_rcstr_orig[16] = {
    0x40,0x53,0x48,0x83,0xec,0x20,0x48,0x8b,0xd9,0xe8,0x36,0xe1,0x3e,0x00,0x48,0x8b
};
extern void fetch_undo(void), register_undo(void), index_undo(void), rcstr_undo(void);
static const unsigned char g_annot_orig[6] = { 0x81, 0xfd, 0xa1, 0x01, 0x00, 0x00 };

static const unsigned char g_tlb_orig[17] = {
    0x40,0x53,0x56,0x57,0x48,0x83,0xec,0x20,0x48,0x8b,0x41,0x30,0x48,0x89,0x6c,0x24,0x48
};
static const unsigned char g_cloneref_orig[16] = {
    0x40,0x53,0x48,0x83,0xec,0x20,0x48,0x8b,0xd9,0x48,0x8b,0x0a,0x48,0x83,0xe9,0x18
};

static unsigned g_clone_calls;
static unsigned g_tlb_calls;

/* Padding so the host image's .bss reaches RVA 0x1D7D0: the M7-2 clone-fix
 * anchor (wstr_clone_ref) lives in the image's own tail, which VirtualAlloc
 * cannot commit (ERROR_INVALID_ADDRESS) — so the host grows its .bss over that
 * RVA and writes the fake bytes in place. SizeOfImage stays < 0x30000, so the
 * M7-1 row_name_ctor page (RVA 0x372f0) remains mappable. */
static unsigned char g_train_pad[0x3000];

extern void tlb_undo(void);
extern void cloneref_undo(void);

__asm__(
    ".globl tlb_undo\n"
    "tlb_undo:\n"
    "addq $0x20, %rsp\n"
    "popq %rdi\n"
    "popq %rsi\n"
    "popq %rbx\n"
    "jmp tlb_body\n"
    ".globl cloneref_undo\n"
    "cloneref_undo:\n"
    "addq $0x20, %rsp\n"
    "popq %rbx\n"
    "jmp cloneref_body\n");

/* the fake purchase-list builder body: the injection hook has already run */
__attribute__((used, noinline)) void tlb_body(void *mgr)
{
    g_tlb_calls++;
    printf("train tlb_body: purchase-list builder ran (manager=%p)\n", mgr);
}

/* the fake wstr_clone_ref: a stand-in refcount bump (shares the pointer) */
/* The fake wstr_clone_ref. NOTE the entry contract: the site's first 16 bytes are
 * the REAL prologue and they are replayed before this body runs, so the registers
 * are already the post-prologue ones — rcx holds the RcStr OBJECT (`*src - 0x18`,
 * what the real 0x14001d800 refcount-bumps) and rdx the src slot. Treating rcx as a
 * destination slot (the obvious reading) writes 8 bytes at `*src - 0x18`, which for
 * this harness's string literals lands in the harness's own .data and, when the
 * source string is a heap one, corrupts a Rust String header — that was the source
 * of the spurious "capacity overflow" panics and the garbage error payloads. The
 * real function's refcount bump is meaningless for literals, so the stand-in only
 * records the call. */
__attribute__((used, noinline)) void cloneref_body(void *obj, void **src)
{
    (void)obj;
    (void)src;
    g_clone_calls++;
}

/* a locale-independent view of a wide string: its UTF-16 code units as hex
 * (the host's C locale cannot print the Japanese names through %ls) */
static const char *whex(const wchar_t *s)
{
    static char buf[8][512];
    static int n;
    char *o = buf[n++ & 7];
    o[0] = 0;
    if (!s) { strcpy(o, "(null)"); return o; }
    for (int i = 0; s[i] && i < 60; i++) {
        char t[8];
        snprintf(t, sizeof t, "%04x ", (unsigned)s[i]);
        strcat(o, t);
    }
    return o;
}

/* find a FORM name's index in the mapped FORM list (UTF-16, comma-separated) */
static int form_id_of(const wchar_t *name)
{
    const wchar_t *p = (const wchar_t *)((BYTE *)g_base + RVA_FORMS);
    int idx = 0;
    size_t nlen = wcslen(name);
    while (*p) {
        const wchar_t *s = p;
        while (*p && *p != L',') p++;
        const wchar_t *e = p;
        while (s < e && (*s == L' ' || *s == L'\t')) s++;
        while (e > s && (e[-1] == L' ' || e[-1] == L'\t')) e--;
        if ((size_t)(e - s) == nlen && wcsncmp(s, name, nlen) == 0) return idx;
        idx++;
        if (*p == L',') p++;
    }
    return -1;
}

/* ---- car-build fakes: the game functions the loader CALLS during the build ---- */
static unsigned g_fetch_calls, g_register_calls, g_index_calls, g_rcstr_calls;
static unsigned g_fetch_capacity, g_fetch_running;
static unsigned long long g_fetch_price;
static wchar_t g_fetch_path[160];
static unsigned char g_fake_car_obj[0x40];   /* the object the fake loader returns */
static unsigned char g_fake_car_model[0x2c8];

void __attribute__((ms_abi)) fake_car_dtor(void *obj, unsigned flags)
{
    (void)obj; (void)flags;
    printf("train: fake car dtor called\n");
}
/* train_model_fetch(wrapper, &path_cell, row, flag): report success, store a
 * non-NULL object in the wrapper's first field, and dump what the loader handed
 * over — the wrapper's three numbers (capacity / running cost / price) and the 17
 * pattern-row cells (the caller keeps the A9_RcStr objects alive). */
unsigned char __attribute__((used, ms_abi)) fake_fetch_body(unsigned char *wrapper,
                                                            const void *path_cell,
                                                            const unsigned char *row,
                                                            unsigned flag)
{
    unsigned i;
    g_fetch_calls++;
    g_fetch_capacity = *(unsigned *)(wrapper + 0x08);
    g_fetch_running  = *(unsigned *)(wrapper + 0x0c);
    g_fetch_price    = *(unsigned long long *)(wrapper + 0x10);
    if (path_cell && *(const void **)path_cell) {
        const wchar_t *w = *(const wchar_t *const *)path_cell;
        wcsncpy(g_fetch_path, w, 159);
        g_fetch_path[159] = 0;
    }
    printf("train: fetch wrapper capacity=%u running=%u price=%llu flag=%u path=%ls\n",
           g_fetch_capacity, g_fetch_running, g_fetch_price, flag, g_fetch_path);
    for (i = 0; i < 17; i++) {
        const wchar_t *c = *(const wchar_t *const *)(row + i * 8);
        printf("train: fetch row cell[%u] = %ls\n", i, c ? c : L"(null)");
    }
    *(void **)wrapper = g_fake_car_obj;
    return 1;
}
/* model_register(manager, wrapper): the manager copies what it needs. */
void __attribute__((used, ms_abi)) fake_register_body(const unsigned char *manager,
                                                      const unsigned char *wrapper)
{
    (void)manager;
    g_register_calls++;
    printf("train: model_register called (capacity=%u running=%u price=%llu)\n",
           *(const unsigned *)(wrapper + 0x08), *(const unsigned *)(wrapper + 0x0c),
           *(const unsigned long long *)(wrapper + 0x10));
}
/* model_index_alloc(manager): the pipeline stores index-1 in the row's slot. */
unsigned short __attribute__((used, ms_abi)) fake_index_body(const unsigned char *manager)
{
    (void)manager;
    g_index_calls++;
    return 500;
}
/* rcstr_ctor(dest): leave a non-NULL object at dest (the printf-style cell ctor
 * then assigns over it). */
void *__attribute__((used, ms_abi)) fake_rcstr_body(void **dest)
{
    g_rcstr_calls++;
    *dest = g_fake_car_model;
    return dest;
}

static int setup_train_pages(void)
{
    /* the clone-ref anchor sits in the image tail: verify the padded .bss
     * really covers it, then make it executable (the fake bytes go in place) */
    {
        BYTE *cr = (BYTE *)g_base + RVA_CLONEREF;
        MEMORY_BASIC_INFORMATION mbi;
        DWORD old;
        if (!VirtualQuery(cr, &mbi, sizeof mbi) || mbi.State != MEM_COMMIT
            || mbi.Protect == PAGE_NOACCESS
            || !VirtualProtect(cr, 0x100, PAGE_EXECUTE_READWRITE, &old)) {
            printf("WARN: the clone-ref RVA 0x1d7d0 is not writable (.bss pad too small?)\n");
            return 0;
        }
    }
    /* the site bytes: real prologue + E9 into the undo thunk */
    struct { UINT_PTR rva; const unsigned char *orig; size_t len; void *undo; } fakes[] = {
        { RVA_TLB,      g_tlb_orig,      sizeof(g_tlb_orig),      (void *)&tlb_undo },
        { RVA_CLONEREF, g_cloneref_orig, sizeof(g_cloneref_orig), (void *)&cloneref_undo },
    };
    for (unsigned i = 0; i < sizeof(fakes) / sizeof(fakes[0]); i++) {
        BYTE *s = (BYTE *)g_base + fakes[i].rva;
        memcpy(s, fakes[i].orig, fakes[i].len);
        BYTE *t = s + fakes[i].len;
        intptr_t rel = (intptr_t)fakes[i].undo - (intptr_t)(t + 5);
        t[0] = 0xE9;
        memcpy(t + 1, &rel, 4);
    }
    g_clone_calls = 0;
    g_tlb_calls = 0;
    /* the rcstr_ctor site's own prologue contains a `call` (0x1403ee144) inside the
     * planted bytes, so it EXECUTES before the E9 reaches the fake. The harness's
     * image at that target is unmapped/zero code, and executing it scribbles over
     * whatever the registers point at (that noise showed up as corrupted Rust
     * strings and host .data). Redirect the call to a bare `xor eax,eax; ret` stub. */
    {
        BYTE *site = (BYTE *)g_base + RVA_RCSTR_CTOR;
        for (size_t i = 0; i + 5 <= sizeof(g_rcstr_orig); i++) {
            if (g_rcstr_orig[i] != 0xE8) continue;
            int32_t rel;
            memcpy(&rel, g_rcstr_orig + i + 1, 4);
            BYTE *target = site + i + 5 + rel;
            if (map_fixed((UINT_PTR)target & ~(UINT_PTR)0xFFF, 0x1000, PAGE_EXECUTE_READWRITE)) {
                target[0] = 0x31; /* xor eax,eax */
                target[1] = 0xC0;
                target[2] = 0xC3; /* ret */
                printf("fake: rcstr prologue call redirected to a ret stub at %p\n",
                       (void *)target);
            } else {
                printf("WARN: could not map the rcstr prologue call target %p\n",
                       (void *)target);
            }
        }
    }
    /* the four car-build helpers: the loader calls them, so plant the address
     * book's expect bytes + a synthetic byte completing the cut last instruction
     * + an E9 to the undo thunk (see the layout comment above the arrays). */
    {
        struct { UINT_PTR rva; const unsigned char *orig; size_t len; void *undo;
                 int completing; } cfakes[] = {
            { RVA_MODEL_FETCH,    g_fetch_orig,    sizeof(g_fetch_orig),    (void *)&fetch_undo,    -1 },
            { RVA_MODEL_REGISTER, g_register_orig, sizeof(g_register_orig), (void *)&register_undo, 0x00 },
            { RVA_MODEL_INDEX,    g_index_orig,    sizeof(g_index_orig),    (void *)&index_undo,    0xc2 },
            { RVA_RCSTR_CTOR,     g_rcstr_orig,    sizeof(g_rcstr_orig),    (void *)&rcstr_undo,    0x03 },
        };
        unsigned k;
        for (k = 0; k < sizeof(cfakes) / sizeof(cfakes[0]); k++) {
            BYTE *site = (BYTE *)g_base + cfakes[k].rva;
            DWORD old;
            MEMORY_BASIC_INFORMATION mbi;
            if (!VirtualQuery(site, &mbi, sizeof mbi) || mbi.State != MEM_COMMIT) {
                /* model_register (0x97A90) is not inside any other mode's block */
                if (!map_fixed((UINT_PTR)(site - 0x90), 0x1000, PAGE_EXECUTE_READWRITE)) {
                    printf("WARN: could not map the car helper page 0x%lx\n",
                           (unsigned long)cfakes[k].rva);
                    continue;
                }
            } else if (!VirtualProtect(site, 0x100, PAGE_EXECUTE_READWRITE, &old)) {
                printf("WARN: could not unprotect the car helper 0x%lx\n",
                       (unsigned long)cfakes[k].rva);
                continue;
            }
            memcpy(site, cfakes[k].orig, cfakes[k].len);
            {
                BYTE *tail = site + cfakes[k].len;
                intptr_t rel;
                if (cfakes[k].completing >= 0) *tail++ = (BYTE)cfakes[k].completing;
                rel = (intptr_t)cfakes[k].undo - (intptr_t)(tail + 5);
                tail[0] = 0xE9;
                memcpy(tail + 1, &rel, 4);
            }
        }
        /* rcstr_ctor's prologue CALLS a game accessor at a fixed RVA: give that
         * page bare `ret`s so the recorded prologue can run exactly as recorded. */
        {
            /* VirtualAlloc rounds the base down to the 64K granularity, so ask
             * for an aligned 64K and fill it: the call target 0x40BA84 (inside
             * this block) then holds `ret`. */
            BYTE *pg = (BYTE *)map_fixed((UINT_PTR)g_base + 0x400000u, 0x10000,
                                         PAGE_EXECUTE_READWRITE);
            if (!pg) printf("WARN: could not map the rcstr call-target block\n");
            else memset(pg, 0xC3, 0x10000);
        }
        *(void **)g_fake_car_obj = (void *)&fake_car_dtor;
        g_fetch_calls = g_register_calls = g_index_calls = g_rcstr_calls = 0;
        g_fetch_capacity = g_fetch_running = 0;
        g_fetch_price = 0;
        g_fetch_path[0] = 0;
    }
    /* the preview annotator's 417-row loop bound: plain bytes the loader rewrites
     * in place (no hook). The 0x3b0000 block is inside the reserved fake image
     * but not committed, so commit it (the earlier read-back crashed until it
     * was). */
    {
        BYTE *blk = (BYTE *)g_base + 0x3B0000u;
        if (!map_fixed((UINT_PTR)blk, 0x1000, PAGE_EXECUTE_READWRITE)) {
            printf("WARN: could not commit the annotator bound block 0x%lx\n",
                   (unsigned long)(UINT_PTR)blk);
        } else {
            memcpy(blk + (RVA_ANNOT_BOUND - 0x3B0000u), g_annot_orig, sizeof(g_annot_orig));
        }
    }
    printf("fake: train pages mapped (tlb 0x%lx, clone_ref 0x%lx, annot_bound 0x%lx)\n",
           (unsigned long)RVA_TLB, (unsigned long)RVA_CLONEREF,
           (unsigned long)RVA_ANNOT_BOUND);
    return 1;
}

/* ---- M5 tailgate fakes (same page set as the inject pages). The family-A * decode entry carries a movabs+branch shim to the plain C decoder; the
 * vp/tp/i.dat call sites carry the address book's E8 expect bytes followed by
 * a clean `ret` continuation the spliced stub returns into; the relay cave
 * page stays 0xCC (the cave expect bytes). */
static void setup_tailgate_pages(void)
{
    {
        BYTE *shim = (BYTE *)g_base + RVA_TG_DECODE;
        intptr_t target = (intptr_t)&tg_decode_body;
        shim[0] = 0x48; shim[1] = 0xB8;
        memcpy(shim + 2, &target, 8);
        shim[10] = 0xFF; shim[11] = 0xE0;
    }
    {
        struct { UINT_PTR site; const unsigned char *bytes; const char *tag; } tsites[] = {
            { RVA_TG_VP_SITE,   g_tg_vp_orig,   "vp" },
            { RVA_TG_TP_SITE,   g_tg_tp_orig,   "tp" },
            { RVA_TG_IDAT_SITE, g_tg_idat_orig, "i.dat" },
        };
        for (unsigned i = 0; i < sizeof(tsites) / sizeof(tsites[0]); i++) {
            BYTE *s = (BYTE *)g_base + tsites[i].site;
            memcpy(s, tsites[i].bytes, 5);
            s[5] = 0xC3; /* return to the host caller when the stub comes back */
            printf("fake: tailgate %s site at RVA 0x%lX\n",
                   tsites[i].tag, (unsigned long)tsites[i].site);
        }
    }
    *(void **)((BYTE *)g_base + RVA_WSOUT) = (void *)((BYTE *)g_base + RVA_TG_OUT);
    printf("fake: tailgate pages mapped\n");
}

/* M5-T: the fake workspace environment (the singleton, the alloc tail site,
 * the CRT fakes) — written BEFORE the DLL is loaded, because the loader
 * verifies those bytes at boot. Only for the workspace modes: it repoints the
 * workspace singleton, which the other modes set up for themselves. */
static void setup_workspace_pages(const char *mode)
{
    ws_setup(0);
    /* ws_setup repoints the singleton at its own "game" buffer; the
     * tailgate stub compares `dest` against it, so the combined mode
     * hands it back the tailgate's workspace out */
    if (!strcmp(mode, "tailgate-ws")) {
        *(void **)((BYTE *)g_base + RVA_WSOUT) =
            (void *)((BYTE *)g_base + RVA_TG_OUT);
    }
    printf("fake: workspace pages mapped\n");
}

/* ---- the M4 inject fake pages. The site page carries the address
 * book's famb_runner_call bytes (E8 + the read-only test ebx,ebx pair +
 * a ret) — the split's rel32 is RVA-relative so the same bytes aim the
 * fake host's call at ITS image + 0x504c0 (the fake runner). Data pages
 * hold the pool object/rows, the worker-capacity global and the shared
 * workspace + decoded stream the injector reads. Mapping is
 * unconditional; only the "inject" host mode touches them below. */
static void setup_inject_pages(const char *mode)
{
    int inj_ok = 1;
    struct { UINT_PTR page; UINT_PTR span; DWORD prot; } irooms[] = {
        { 0x3F0000, 0x10000, PAGE_EXECUTE_READWRITE }, /* the spliced site (+ vp/tp) */
        { 0x50000,  0x10000, PAGE_EXECUTE_READWRITE }, /* the fake runner */
        { 0xF02000, 0x4000,  PAGE_READWRITE },         /* pool + rows */
        { 0xD65000, 0x1000,  PAGE_READWRITE },         /* capacity global */
        { 0x4F86000, 0x10000, PAGE_READWRITE },        /* workspace + stream + in-buffer; M5-T: the game out/in + new out */
        { 0x59A000, 0x2000, PAGE_EXECUTE_READWRITE },  /* the M4 relay cave (0x59a071) */
        { 0x314000, 0x6000, PAGE_EXECUTE_READWRITE },  /* M5: decode + i.dat site; M5-T: the alloc-tail site 0x31981a */
        { 0x5A1000, 0x1000, PAGE_EXECUTE_READWRITE },  /* M5: the tailgate relay cave */
        { 0x597000, 0x1000, PAGE_EXECUTE_READWRITE },  /* M5-T: malloc/free fakes */
        { 0x592000, 0x1000, PAGE_EXECUTE_READWRITE },  /* M5-T: the workspace relay cave */
    };
    for (unsigned i = 0; i < sizeof(irooms) / sizeof(irooms[0]); i++) {
        BYTE *want = (BYTE *)g_base + irooms[i].page;
        int fresh = 1;
        if (!map_fixed((UINT_PTR)want, irooms[i].span, irooms[i].prot)) {
            /* the range may already be committed because the host image
             * grew over it (the fake runner RVA 0x504c0 sits inside the
             * host's own .text once the harness is big enough) — accept a
             * committed range but NEVER memset it: that would clobber the
             * host's own code. */
            MEMORY_BASIC_INFORMATION mbi;
            if (VirtualQuery(want, &mbi, sizeof mbi) && mbi.State == MEM_COMMIT
                && mbi.Protect != PAGE_NOACCESS) {
                fresh = 0;
            } else {
                printf("WARN: could not map inject page at RVA 0x%lX err=%lu\n",
                       (unsigned long)irooms[i].page, (unsigned long)GetLastError());
                inj_ok = 0;
                continue;
            }
        }
        if (fresh) memset(want, 0xCC, irooms[i].span);
    }
    if (!inj_ok) return;

    /* the famb call site: book bytes + a clean continuation */
    memcpy((BYTE *)g_base + RVA_INJ_SITE, g_famb_orig, sizeof(g_famb_orig));
    ((BYTE *)g_base + RVA_INJ_SITE)[7] = 0x74;
    ((BYTE *)g_base + RVA_INJ_SITE)[8] = 0x00; /* je +0 (falls through) */
    ((BYTE *)g_base + RVA_INJ_SITE)[9] = 0xC3; /* ret */
    /* the fake runner: movabs rax,<body>; jmp rax */
    {
        BYTE *shim = (BYTE *)g_base + RVA_FAKE_RUNNER;
        intptr_t target = (intptr_t)&fake_runner_body;
        shim[0] = 0x48; shim[1] = 0xB8;
        memcpy(shim + 2, &target, 8);
        shim[10] = 0xFF; shim[11] = 0xE0;
    }
    /* the worker-row capacity the injector reads */
    *(unsigned int *)((BYTE *)g_base + RVA_POOL_CAP) = 8;
    /* the fake decoded family-B stream:
     * [u32 count]{0x200 name|w|h|stored|payload}* (the layout the
     * loader's rsp+0x60 pointer arithmetic walks) */
    {
        static const wchar_t *names[2] = { L"zero.dds", L"one.dds" };
        static const unsigned ws[2] = { 4, 8 }, hs[2] = { 4, 8 },
            st[2] = { 8, 4 };
        BYTE *out = (BYTE *)g_base + RVA_OUTBUF;
        unsigned count = 2;
        *(unsigned int *)out = count;
        /* entry(i) = out + 4 + Σ_{k<i}(0x20c + st[k]) — the loader's
         * own running arithmetic, which the injector walk mirrors */
        unsigned run = 4;
        for (unsigned i = 0; i < count; i++) {
            BYTE *e = out + run;
            memset(e, 0, 0x200);
            for (unsigned j = 0; names[i][j]; j++) {
                *(wchar_t *)(e + j * 2) = names[i][j];
            }
            *(unsigned int *)(e + 0x200) = ws[i];
            *(unsigned int *)(e + 0x204) = hs[i];
            *(unsigned int *)(e + 0x208) = st[i];
            run += 0x20c + st[i];
        }
        printf("fake: workspace stream at RVA_OUTBUF: %u entries\n", count);
    }
    /* the hook only consults the workspace pointer */
    *(void **)((BYTE *)g_base + RVA_WSOUT) = (void *)((BYTE *)g_base + RVA_OUTBUF);
    /* ---- M5 tailgate fakes (same page set), then M5-T's workspace env for
     * the workspace modes (it repoints the singleton, so it runs last) */
    setup_tailgate_pages();
    if (!strcmp(mode, "workspace") || !strcmp(mode, "workspace-fail")
        || !strcmp(mode, "tailgate-ws")) {
        setup_workspace_pages(mode);
    }
}

/* ==== mode handlers ====================================================== */

static int mode_none(void)
{
    BYTE *fnb = g_base + RVA_FILTER;
    int unchanged = memcmp(fnb, g_filter_orig, sizeof(g_filter_orig)) == 0;
    printf("mode=none: filter bytes %s\n", unchanged ? "unchanged (PASS)" : "PATCHED (FAIL)");
    return 0;
}

/* the M3 game: open the pack through the (possibly hooked) CFile::Open
 * at RVA_CFILE, read it whole through the returned handle, and report
 * WHICH path the open used + its size — the redirect evidence; the
 * content is verified offline by the harness script via a9ml */
static int mode_overlaypackcsv(const char *mode)
{
    const wchar_t *path = !strcmp(mode, "overlaypack")
        ? L"data\\mtfake.bin" : L"data\\csvp.bin";
    unsigned char cfile[0x28];
    cfile_open_fn cfo = (cfile_open_fn)(g_base + RVA_CFILE);
    int r;
    HANDLE h;
    DWORD lo, hi, rd = 0;
    BYTE *buf;
    memset(cfile, 0, sizeof(cfile));
    r = cfo(cfile, path, 0, 0);
    if (!r) {
        printf("overlay %s: open FAILED (ret=0)\n", mode);
        return 0;
    }
    h = *(HANDLE *)(cfile + 8);
    lo = GetFileSize(h, &hi);
    buf = (BYTE *)malloc(lo ? lo : 1);
    if (!ReadFile(h, buf, lo, &rd, NULL) || rd != lo) {
        printf("overlay %s: short read (%lu of %lu)\n", mode,
               (unsigned long)rd, (unsigned long)lo);
        CloseHandle(h);
        free(buf);
        return 0;
    }
    CloseHandle(h);
    free(buf);
    printf("overlay %s: %lu bytes, handle-read ok\n", mode, (unsigned long)lo);
    return 0;
}

/* M5 W1: open data\i.dat (stage 1 — the loader builds every host
 * pack's cache first, then the merged index) and data\vp.bin (stage 2
 * — the family-A host the appended asset bytes ride) through the
 * (possibly hooked) CFile::Open. Evidence: WHICH path each open used
 * (cfile_open_body prints it) + the byte size. */
static int mode_overlaydat(void)
{
    const wchar_t *paths[2] = { L"data\\i.dat", L"data\\vp.bin" };
    const char *tags[2] = { "i.dat", "vp.bin" };
    for (int i = 0; i < 2; i++) {
        unsigned char cfile[0x28];
        cfile_open_fn cfo = (cfile_open_fn)(g_base + RVA_CFILE);
        HANDLE h;
        DWORD lo, hi, rd = 0;
        BYTE *buf;
        int ok;
        memset(cfile, 0, sizeof(cfile));
        int r = cfo(cfile, paths[i], 0, 0);
        if (!r) {
            printf("overlay %s: open FAILED (ret=0)\n", tags[i]);
            continue;
        }
        h = *(HANDLE *)(cfile + 8);
        lo = GetFileSize(h, &hi);
        buf = (BYTE *)malloc(lo ? lo : 1);
        ok = ReadFile(h, buf, lo, &rd, NULL) && rd == lo;
        CloseHandle(h);
        free(buf);
        printf("overlay %s: %lu bytes, handle-read %s\n",
               tags[i], (unsigned long)lo, ok ? "ok" : "SHORT");
    }
    return 0;
}

/* M5-U: stage 1 opens data\i.dat (the loader builds every host pack's
 * cache — tp for a preview-listing mod — then the merged index), stage 2
 * opens data\tp.bin. Same evidence shape as overlaydat. */
static int mode_overlaytp(void)
{
    const wchar_t *paths[2] = { L"data\\i.dat", L"data\\tp.bin" };
    const char *tags[2] = { "i.dat", "tp.bin" };
    for (int i = 0; i < 2; i++) {
        unsigned char cfile[0x28];
        cfile_open_fn cfo = (cfile_open_fn)(g_base + RVA_CFILE);
        HANDLE h;
        DWORD lo, hi, rd = 0;
        BYTE *buf;
        int ok;
        memset(cfile, 0, sizeof(cfile));
        int r = cfo(cfile, paths[i], 0, 0);
        if (!r) {
            printf("overlay %s: open FAILED (ret=0)\n", tags[i]);
            continue;
        }
        h = *(HANDLE *)(cfile + 8);
        lo = GetFileSize(h, &hi);
        buf = (BYTE *)malloc(lo ? lo : 1);
        ok = ReadFile(h, buf, lo, &rd, NULL) && rd == lo;
        CloseHandle(h);
        free(buf);
        printf("overlay %s: %lu bytes, handle-read %s\n",
               tags[i], (unsigned long)lo, ok ? "ok" : "SHORT");
    }
    return 0;
}

/* M6 [[loose_asset]]: open game files BY PATH through the (possibly
 * hooked) CFile::Open — an ABSOLUTE path matching the mod's
 * `*.adk\fe01.bin` glob (the per-boot random temp-dir shape), an exact
 * RELATIVE path, and a non-matching path. Evidence: the path the open
 * actually used (g_last_path) + the bytes read, dumped to
 * read_<tag>.bin for an offline byte-compare. */
static int mode_loose(void)
{
    wchar_t cwd[512];
    static wchar_t abs_path[1024];
    const wchar_t *paths[3];
    const char *tags[3] = { "abs", "rel", "plain" };
    if (!GetCurrentDirectoryW(512, cwd)) cwd[0] = 0;
    swprintf(abs_path, 1024, L"%ls\\tmpdir.adk\\fe01.bin", cwd);
    paths[0] = abs_path;
    paths[1] = L"data\\bmf\\fy01.bin";
    paths[2] = L"data\\mtfake.bin";
    for (int i = 0; i < 3; i++) {
        unsigned char cfile[0x28];
        cfile_open_fn cfo = (cfile_open_fn)(g_base + RVA_CFILE);
        HANDLE h;
        DWORD lo, hi, rd = 0;
        BYTE *buf;
        int ok = 0;
        g_last_path[0] = 0;
        memset(cfile, 0, sizeof(cfile));
        int r = cfo(cfile, paths[i], 0, 0);
        if (r) {
            h = *(HANDLE *)(cfile + 8);
            lo = GetFileSize(h, &hi);
            buf = (BYTE *)malloc(lo ? lo : 1);
            ok = ReadFile(h, buf, lo, &rd, NULL) && rd == lo;
            if (ok) {
                char out[64];
                sprintf(out, "read_%s.bin", tags[i]);
                write_whole(out, buf, rd);
            }
            CloseHandle(h);
            free(buf);
            printf("loose %s: used=%ls ret=%d bytes=%lu read=%s\n",
                   tags[i], g_last_path, r, (unsigned long)lo, ok ? "ok" : "SHORT");
        } else {
            printf("loose %s: used=%ls ret=0 (open failed)\n", tags[i], g_last_path);
        }
    }
    return 0;
}

static int mode_probe(void)
{
    if (!g_probe_ok) {
        printf("FAIL: probe anchor pages could not be mapped\n");
        return 1;
    }
    /* the M4 evidence call: run every fetch-probe anchor through its
     * (possibly hooked) entry once — the loader's probe logs the four
     * lines when installed; without the probe these run the fakes */
    static const wchar_t composed_key[] = L"train\\113\\_d_113.dds";
    static const wchar_t mesh_src[] = L"train\\113\\_t_113_a.x";
    static const wchar_t mesh_ref[] = L"./_d_113.dds";
    /* the wstring-OBJECT shape FUN_140306e20's r8 expects ([r8] = data) */
    static const void *name_obj[2];
    static void *dest[64];
    void *bo = NULL;
    unsigned so = 0;
    name_obj[0] = (const void *)composed_key;
    typedef void *(*pack_fn)(void *, void *, const void *);
    typedef void *(*tex_fn)(void *, const wchar_t *, const wchar_t *, unsigned);
    typedef void *(*ccom_fn)(void *, unsigned long long, const void *,
                             unsigned long long, void **, unsigned *);
    typedef void *(*cache_fn)(void *, const wchar_t *);
    printf("host: PACK call about to run\n");
    printf("host: pack fetch ret=%p\n", ((pack_fn)(g_base + RVA_PACK))(NULL, dest, name_obj));
    printf("host: TEX call about to run\n");
    ((tex_fn)(g_base + RVA_TEX))(NULL, mesh_src, mesh_ref, 7);
    printf("host: CCOM call about to run\n");
    ((ccom_fn)(g_base + RVA_CCOM))(NULL, 0, name_obj, 0, &bo, &so);
    printf("host: CACHE call about to run\n");
    ((cache_fn)(g_base + RVA_CACHE))(NULL, composed_key);
    printf("probe host: four fetch calls done\n");
    return 0;
}

/* the M7 fix evidence: with a train_list mod, the loader rewrote the vanilla
 * whitelist scan to call its composed-keep test through the relay cave. The
 * host calls the CAVE directly with a few ids (the patched scan's exact call
 * shape) and reads back the patched site's bytes. */
static int mode_tabfix(void)
{
    mgr_setup();
    if (*(BYTE *)(g_base + RVA_TAB_SCAN) != 0x4D) {
        printf("tabfix: site NOT patched (byte=0x%02x) — skipping the cave call\n",
               *(BYTE *)(g_base + RVA_TAB_SCAN));
        return 1;
    }
    typedef unsigned char (*mem_fn)(unsigned);
    mem_fn mem = (mem_fn)(g_base + RVA_TAB_CAVE);
    unsigned ids[] = { 4, 16, 42, 329, 1, 999 };
    printf("tabfix: cave results:");
    for (unsigned i = 0; i < sizeof(ids) / sizeof(ids[0]); i++) {
        printf(" %u->%u", ids[i], mem(ids[i]));
    }
    printf("\n");
    printf("tabfix: site[0]=0x%02x site[15]=0x%02x site[24]=0x%02x\n",
           *(BYTE *)(g_base + RVA_TAB_SCAN),
           *(BYTE *)(g_base + RVA_TAB_SCAN + 15),
           *(BYTE *)(g_base + RVA_TAB_SCAN + 24));
    printf("tabfix: done\n");
    return 0;
}

/* the M7b custom-train-form fix evidence: check the site was rewritten, call
 * the composed-keep cave for a few ids, and (through the rewritten scan's own
 * two branch markers) read the keep/drop decision end-to-end. */
static int mode_formfix(void)
{
    if (*(BYTE *)(g_base + RVA_FORM_SCAN) != 0x89) {
        printf("formfix: site NOT patched (byte=0x%02x) — skipping the cave call\n",
               *(BYTE *)(g_base + RVA_FORM_SCAN));
        return 1;
    }
    typedef unsigned char (*mem_fn)(unsigned);
    mem_fn mem = (mem_fn)(g_base + RVA_FORM_CAVE);
    unsigned ids[] = { 4, 16, 42, 329, 1, 999 };
    printf("formfix: cave results:");
    for (unsigned i = 0; i < sizeof(ids) / sizeof(ids[0]); i++) {
        printf(" %u->%u", ids[i], mem(ids[i]));
    }
    printf("\n");
    {
        BYTE *s = (BYTE *)g_base + RVA_FORM_SCAN;
        printf("formfix: site bytes:");
        for (int i = 0; i < 20; i++) printf(" %02x", s[i]);
        printf("\n");
    }
    /* end-to-end: run the rewritten scan itself and read its branch decision
     * (1 = the primary admit path was taken, 2 = the not-found fall-through) */
    g_form_scan_ptr = (void *)(g_base + RVA_FORM_SCAN);
    printf("formfix: scan branch: 329->%u 4->%u 1->%u 999->%u (1=kept path, 2=drop path)\n",
           form_scan_call(329), form_scan_call(4), form_scan_call(1), form_scan_call(999));
    printf("formfix: done\n");
    return 0;
}

/* the M7 probe evidence: call the (possibly hooked) window + tab-rebuild
 * entries so the diagnostic dumps the manager lists; shrink +0x48 between
 * calls so the delta line fires. */
static int mode_window(void)
{
    mgr_setup();
    static int tabdesc[11] = { 0, 1, -1, 0, 0, 0, 0, 0, 0, 0, 0 };
    typedef void (*win_fn)(void *);
    typedef void (*tab_fn)(void *, const int *);
    printf("window: call 1 (frame)\n");
    ((win_fn)(g_base + RVA_WINDOW))(NULL);
    printf("window: call 2 (frame; +0x48 shrunk to 3)\n");
    *(void **)((BYTE *)g_base + RVA_MGR + 0x50) = g_vec_flt + 3;
    ((win_fn)(g_base + RVA_WINDOW))(NULL);
    printf("window: tab rebuild call\n");
    ((tab_fn)(g_base + RVA_TAB))((void *)((BYTE *)g_base + RVA_MGR), tabdesc);
    printf("window: done\n");
    return 0;
}

/* the M7-2 save-path probe evidence: call the four (possibly hooked) save-side
 * mmf helpers through in-image caller stubs, so the probe logs the caller RVA
 * + size of each write, the two verbatim-block lines, and the read-side line.
 * The fakes run through their undo thunks either way. */
static int mode_save(void)
{
    static unsigned long ring[64];
    static unsigned char buf[0x2000];
    static unsigned data[8];
    static const unsigned *vec[2];
    for (unsigned i = 0; i < sizeof(buf); i++) buf[i] = (unsigned char)(0xA0 + (i & 0x0f));
    data[0] = 1; data[1] = 2; data[2] = 3; data[3] = 4; data[4] = 5;
    vec[0] = data; vec[1] = data + 5;
    BYTE *stub = (BYTE *)g_base + RVA_SAVE_STUB;
    typedef void (*w3)(void *, const void *, unsigned);
    typedef void (*w2)(unsigned long, const void *);
    printf("save: write 0xf8 (the car-block shape)\n");
    ((w3)(stub + 0x00))(ring, buf, 0xf8);
    printf("save: write 0x124 (the state block)\n");
    ((w3)(stub + 0x00))(ring, buf, 0x124);
    printf("save: bulk 0xbc0 (the mini-block)\n");
    ((w3)(stub + 0x20))(ring, buf, 0xbc0);
    printf("save: u32-vector block (5 elems)\n");
    ((w2)(stub + 0x40))(0, vec);
    printf("save: u32-vector read\n");
    ((w2)(stub + 0x60))(0, vec);
    printf("save: %lu fake-body calls\n", g_save_calls);
    printf("save: done\n");
    return 0;
}

/* the M7-0 clamp evidence: (1) the head clamp — call the head site with a set
 * of ids through the r14 thunk; (2) the car clamp — fill a 0xf8 car block with
 * a mix of in/out-of-range ids, call the car site (which reads through the fake
 * paged reader, then clamps), and read the block back. The manager carries
 * CLAMP_ROWS rows so the target rows are in range. */
static int mode_clamp(void)
{
    unsigned char *rows = (unsigned char *)VirtualAlloc(NULL,
        (SIZE_T)CLAMP_ROWS * 0x140, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!rows) { printf("FAIL: clamp rows alloc\n"); return 2; }
    memset(rows, 0, (SIZE_T)CLAMP_ROWS * 0x140);
    {
        BYTE *m = (BYTE *)g_base + RVA_MGR;
        *(void **)(m + 0x18) = rows;
        *(void **)(m + 0x20) = rows + (SIZE_T)CLAMP_ROWS * 0x140;
    }

    /* ---- head: in-range ids pass through, out-of-range -> 417 ---- */
    g_clamp_head_ptr = (void *)(g_base + RVA_CLAMP_HEAD);
    static const unsigned head_ids[] = { 42, 0, 420, 500, 421, 65535 };
    printf("clamp head:");
    for (unsigned i = 0; i < sizeof(head_ids) / sizeof(head_ids[0]); i++)
        printf(" %u->%u", head_ids[i], clamp_head_call(head_ids[i]));
    printf("\n");

    /* ---- car: the fixed 0xf8 block, 10 rows of [u16 id][u8 aux][u8 f][20B] ---- */
    static unsigned char src[0x100], dst[0x100];
    static const unsigned short car_ids[10] =
        { 42, 9999, 400, 0, 160, 421, 65535, 1, 0, 2000 };
    memset(src, 0, sizeof src);
    for (unsigned i = 0; i < 10; i++) {
        *(unsigned short *)(src + i * 0x18) = car_ids[i];
        src[i * 0x18 + 2] = 1;   /* slot (1-based car position) */
        src[i * 0x18 + 3] = 0;   /* f   */
    }
    /* a save written while the row's per-car block was empty carries slot 0 —
     * the guard must repair it (live crash 2026-10-15) */
    src[2 * 0x18 + 2] = 0;   /* a *real* car (id 400) with slot 0 */
    src[7 * 0x18 + 2] = 0;
    src[0xf5] = 10;              /* the trailer's car count at block+0xf0+5 */
    memset(dst, 0xAA, sizeof dst);
    g_clamp_read_src = src;
    g_clamp_car_ptr = (void *)(g_base + RVA_CLAMP_CAR);
    unsigned long rr = clamp_car_call(NULL, dst, 0xf8);
    printf("clamp car: ret=%lu\n", rr);
    printf("clamp car slots:");
    for (unsigned i = 0; i < 10; i++)
        printf(" %u", (unsigned)dst[i * 0x18 + 2]);
    printf("\n");
    printf("clamp car ids:");
    for (unsigned i = 0; i < 10; i++) {
        unsigned short got = *(unsigned short *)(dst + i * 0x18);
        printf(" %u->%u", car_ids[i], got);
    }
    printf("\n");
    printf("clamp: done\n");
    return 0;
}

/* the M7-1 direct spec-row injection evidence. The host maps the fake manager
 * + spec-table pages, then drives the loader's `a9loader_m7_rows` export:
 *   rows             — op 0 (install the resize hook) then call the resize site;
 *                      the hook must inject AFTER the original re-pin
 *   rows-order       — op 1 (inject) BEFORE the resize; the resize re-pins to
 *                      421, so the pre-resize growth must be silently undone
 *   rows-refuse      — the name ctor returns NULL: the injection must refuse
 *   rows-refuse-tmpl — the template row's pFormName is NULL: must refuse
 *   rows-off         — no export call: the site stays vanilla, count stays 421 */
typedef unsigned long long (*m7rows_fn)(unsigned);

static int mode_rows(const char *scen)
{
    if (!setup_rows_pages()) return 2;
    HMODULE v = GetModuleHandleA("version.dll");
    if (!v) { printf("FAIL: version.dll not loaded\n"); return 1; }
    m7rows_fn op = (m7rows_fn)(void *)GetProcAddress(v, "a9loader_m7_rows");
    if (!op) { printf("FAIL: the a9loader_m7_rows export is missing\n"); return 1; }

    BYTE *m = (BYTE *)g_base + RVA_MGR;
    void *vec = m + 0x18;
    typedef void (*resize_fn)(void *);

    if (!strcmp(scen, "rows-off")) {
        printf("rows: site[0]=0x%02x (unmodified)\n", *(BYTE *)(g_base + RVA_ROWS_RESIZE));
        ((resize_fn)(g_base + RVA_ROWS_RESIZE))(vec);
        printf("rows: count after the vanilla resize = %llu\n", op(2));
        printf("rows: done\n");
        return 0;
    }
    if (!strcmp(scen, "rows-order")) {
        unsigned long long n1 = op(1);
        printf("rows: after a PRE-resize inject count=%llu\n", n1);
        ((resize_fn)(g_base + RVA_ROWS_RESIZE))(vec);
        printf("rows: after the resize count=%llu (the pre-resize growth is undone)\n",
               op(2));
        printf("rows: done\n");
        return 0;
    }
    if (!strcmp(scen, "rows-refuse")) {
        g_rows_ctor_null = 1;
        unsigned long long n = op(1);
        printf("rows: inject with a NULL name returned %llu, count=%llu, realloc calls=%u\n",
               n, op(2), g_rows_realloc_calls);
        printf("rows: done\n");
        return 0;
    }
    if (!strcmp(scen, "rows-refuse-tmpl")) {
        unsigned char *r = g_rows_base + ROWS_TEMPLATE_ID * ROWS_ROW_STRIDE;
        *(void **)(r + ROWS_PFORMNAME) = NULL;
        unsigned long long n = op(1);
        printf("rows: inject from a NULL-named template returned %llu, count=%llu, realloc calls=%u\n",
               n, op(2), g_rows_realloc_calls);
        printf("rows: done\n");
        return 0;
    }

    /* rows: the RIGHT order — install the hook, then call the resize site */
    unsigned long long rc = op(0);
    printf("rows: install rc=%llu site[0]=0x%02x\n", rc, *(BYTE *)(g_base + RVA_ROWS_RESIZE));
    ((resize_fn)(g_base + RVA_ROWS_RESIZE))(vec);
    unsigned long long n = op(2);
    printf("rows: count after the hooked resize = %llu\n", n);

    /* the consumer: the game's OWN accessor resolves the injected id */
    typedef unsigned char *(*acc_fn)(const unsigned char *, unsigned long);
    acc_fn acc = (acc_fn)(g_base + RVA_ROWS_ACCESSOR);
    unsigned char *row = acc((const unsigned char *)vec, ROWS_RESERVED_ID);
    printf("rows: accessor(%u)=%p begin=%p stride=%#x\n", ROWS_RESERVED_ID, (void *)row,
           *(void **)vec, ROWS_ROW_STRIDE);
    printf("rows: injected row bValid=%u carCount=%u marker=%#x\n",
           row[0], row[0x38], *(unsigned int *)(row + ROWS_MARKER_OFF));
    printf("rows: injected names jp=%ls en=%ls form=%ls\n",
           *(wchar_t **)(row + ROWS_NAME_JP),
           *(wchar_t **)(row + ROWS_NAME_EN),
           *(wchar_t **)(row + ROWS_PFORMNAME));
    printf("rows: realloc calls=%u new_count=%lu ctor calls=%u\n",
           g_rows_realloc_calls, g_rows_realloc_new, g_rows_ctor_calls);
    /* the row-walk consumer (mirrors train_list_builder 0x14019d9c0 /
       FUN_14019e760 / FUN_14019e950: walk the LIVE vector, skip row[0]==0) */
    {
        unsigned char *b = *(unsigned char **)vec;
        unsigned char *e = *(unsigned char **)((char *)vec + 8);
        unsigned long cnt = (unsigned long)((e - b) / ROWS_ROW_STRIDE);
        unsigned valid = 0, has = 0;
        for (unsigned long i = 0; i < cnt; i++) {
            unsigned char *rr = b + i * ROWS_ROW_STRIDE;
            if (!rr[0]) continue;
            valid++;
            if (i == ROWS_RESERVED_ID) has = 1;
        }
        printf("rows: walk rows=%lu valid=%u reserved-id-present=%u\n", cnt, valid, has);
    }
    printf("rows: reserved id %u < FORMS_MAX 1024 = %u\n", ROWS_RESERVED_ID,
           ROWS_RESERVED_ID < 1024);
    printf("rows: done\n");
    return 0;
}

/* the M7-2 `[[train]]` host path: the engine's boot already ran (on load) and
 * compiled/queued the scenario's train. Call the `train_list_builder` site to
 * fire the injection hook, read the injected row through the game's OWN
 * accessor, print the composed-filter verdict for its reserved id, open
 * data\csvp.bin through the (hooked) CFile::Open so the overlay merges the
 * derived rows, and dump the merged tables. */
/* M7-2 retry scenario: mark the FORM_AR4 donor row valid with the lane shape
 * the clone fix exercises. A live boot showed the builder is called more than
 * once per boot and the FIRST call happens before the CSV pipeline populates
 * the rows, so the loader must defer and retry — the `train_retry` mode below
 * fires phase 1 with this helper deliberately NOT yet called. */
/* The four undo thunks: each site holds the REAL prologue, so execution arrives
 * here with the prologue's frame built — undo it, restore the register it pushed
 * (saved at rsp+0x20 => entry_rsp-8), then jump to the body. `index_undo` has no
 * frame to undo (that prologue only touches registers). */
__asm__(
    ".text\n"
    ".globl fetch_undo\n"
    "fetch_undo:\n"
    /* The site saved rbx/rsi/rdi into the caller's shadow space and pushed r14; the
     * real epilogue restores all four, so the fake must be entered with them
     * restored too. Skipping that leaves the CALLER's callee-saved registers
     * clobbered - for `model_register` (whose site does `mov rbx,rcx`) that silently
     * redirected the caller's sret pointer and produced garbage results. */
    "  movq 0x30(%rsp), %rbx\n"
    "  movq 0x38(%rsp), %rsi\n"
    "  movq 0x40(%rsp), %rdi\n"
    "  movq 0x20(%rsp), %r14\n"
    "  addq $0x28, %rsp\n"
    "  jmp fake_fetch_body\n"
    ".globl register_undo\n"
    "register_undo:\n"
    "  movq 0x30(%rsp), %rbx\n"
    "  movq 0x20(%rsp), %rdi\n"
    "  addq $0x28, %rsp\n"
    "  jmp fake_register_body\n"
    ".globl index_undo\n"
    "index_undo:\n"
    "  jmp fake_index_body\n"
    ".globl rcstr_undo\n"
    "rcstr_undo:\n"
    "  movq 0x20(%rsp), %rbx\n"
    "  addq $0x28, %rsp\n"
    "  jmp fake_rcstr_body\n");
static void train_populate_donor(void)
{
    int id = form_id_of(L"FORM_AR4");
    if (id >= 0 && id < (int)ROWS_TABLE_N) {
        unsigned char *r = g_rows_base + (size_t)id * ROWS_ROW_STRIDE;
        static wchar_t djp[] = L"DONOR_JP";
        static wchar_t den[] = L"DONOR_EN";
        static wchar_t dfn[] = L"form_ar4";  /* shipped rows carry pFormName lowercase */
        r[0] = 1;
        r[0x38] = 1;
        *(void **)(r + ROWS_NAME_JP) = djp;
        *(void **)(r + ROWS_NAME_EN) = den;
        *(void **)(r + ROWS_NAME_JP + 2 * 8) = djp;
        *(void **)(r + ROWS_NAMEALT) = djp;
        *(void **)(r + ROWS_PFORMNAME) = dfn;
        r[0x39] = 0xA9;  /* the donor's formation block: the car build must NOT touch it */
        r[0x3a] = 0x5C;
        printf("train: donor row %d (FORM_AR4) populated\n", id);
    } else {
        printf("train: WARN: FORM_AR4 not found in the FORM list\n");
    }
}

static int mode_train(const char *scen, int retry)
{
    HMODULE v = GetModuleHandleA("version.dll");
    if (!v) { printf("FAIL: version.dll not loaded\n"); return 1; }
    typedef unsigned long long (*m7kept_fn)(unsigned long long);
    typedef unsigned long long (*m7dump_fn)(void);
    m7kept_fn kept = (m7kept_fn)(void *)GetProcAddress(v, "a9loader_m7_kept");
    m7dump_fn dump = (m7dump_fn)(void *)GetProcAddress(v, "a9loader_m7_dump");
    if (!kept || !dump) { printf("FAIL: the M7-2 exports are missing\n"); return 1; }

    BYTE *m = (BYTE *)g_base + RVA_MGR;
    void *vec = m + 0x18;

    /* fire the injection by calling the site (the loader hooked it) */
    typedef void (*tlb_fn)(void *);
    if (retry) {
        /* the live shape: the first call happens before the rows are filled */
        ((tlb_fn)(g_base + RVA_TLB))((void *)m);
        printf("train: retry phase 1 fired with the donor unpopulated\n");
        train_populate_donor();
        ((tlb_fn)(g_base + RVA_TLB))((void *)m);
        printf("train: retry phase 2 fired with the donor populated\n");
        /* a third call mirrors the live "scenario load" call: the injection is
         * already done, so this one only exercises the probe's post-injection
         * dump of the injected row's name lanes. */
        ((tlb_fn)(g_base + RVA_TLB))((void *)m);
        printf("train: third call (post-injection, probe only)\n");
    } else {
        train_populate_donor();
        ((tlb_fn)(g_base + RVA_TLB))((void *)m);
    }
    printf("train: builder calls=%u clone_ref calls=%u\n", g_tlb_calls, g_clone_calls);

    unsigned char *b = *(unsigned char **)vec;
    unsigned char *e = *(unsigned char **)((char *)vec + 8);
    printf("train: vector rows after injection = %lu\n",
           (unsigned long)((e - b) / ROWS_ROW_STRIDE));
    /* the annotator bound the loader rewrote (its immediate follows the 2-byte opcode) */
    printf("train: annotator bound now = %u (was 417)\n",
           *(unsigned int *)((BYTE *)g_base + RVA_ANNOT_BOUND + 2));

    typedef unsigned char *(*acc_fn)(const unsigned char *, unsigned long);
    acc_fn acc = (acc_fn)(g_base + RVA_ROWS_ACCESSOR);
    if ((e - b) / ROWS_ROW_STRIDE > 421) {
        unsigned char *r = (unsigned char *)acc((const unsigned char *)vec, 421);
        printf("train: car build rcstr=%u register=%u index=%u slot=%u carCount=%u first=%u flags=%u block=%02x%02x\n",
               (unsigned)g_rcstr_calls, (unsigned)g_register_calls, (unsigned)g_index_calls,
               (unsigned)*(unsigned short *)(r + 0x2c), (unsigned)r[0x38],
               (unsigned)*(unsigned short *)(r + 0x14), (unsigned)r[0x9d],
               (unsigned)r[0x39], (unsigned)r[0x3a]);
    }
    unsigned char *row = acc((const unsigned char *)vec, ROWS_RESERVED_ID);
    printf("train: injected row %u bValid=%u special=%u cruise=%u fare=%u carCount=%u\n",
           ROWS_RESERVED_ID, row[0], row[0x9e],
           *(unsigned short *)(row + 0x0c), *(unsigned short *)(row + 0x12), row[0x38]);
    /* the two row invariants the car build must establish (live regressions
     * 2026-10-15): the validity u32 is exactly 1 on every shipped row, and unused
     * model slots carry the pipeline's empty marker (7), never a valid index. */
    printf("train: injected validity u32 = %u\n", *(unsigned int *)row);
    /* +0x2c is the 0x18-stride consist array: per entry a u16 model slot and a
     * 1-based sub-index byte at +0x02 (the donor clone leaves it 0, and sub = 0
     * makes train_row_car_wiring walk aModelIdx[-1]). */
    printf("train: consist entries =");
    for (unsigned k = 0; k < 6; k++) {
        printf(" %u/%u", *(unsigned short *)(row + 0x2c + k * 0x18),
               (unsigned)row[0x2e + k * 0x18]);
    }
    printf("\n");
    /* the per-consist-length records: the leading byte is the car's sub-index, and
     * the vanilla convention (AR4: 01 02 02 ff) writes ff for "the default type" —
     * a 0 there makes the wiring walk aModelIdx[sub - 1] with sub = 0. */
    for (unsigned rec = 0; rec < 10; rec++) {
        const unsigned char *q = row + 0x39 + rec * 10;
        printf("train: car build block record %u = %02x %02x %02x %02x\n",
               rec + 1, q[0], q[1], q[2], q[3]);
    }
    printf("train: injected names jp=%s en=%s form=%s\n",
           whex(*(wchar_t **)(row + ROWS_NAME_JP)),
           whex(*(wchar_t **)(row + ROWS_NAME_EN)),
           whex(*(wchar_t **)(row + ROWS_PFORMNAME)));
    printf("train: reserved id %u kept by the composed filter = %llu\n",
           ROWS_RESERVED_ID, kept(ROWS_RESERVED_ID));

    /* open data\csvp.bin through the hooked CFile::Open -> the overlay merges */
    {
        static const wchar_t csvp[] = L"data\\csvp.bin";
        unsigned char cfile[0x28];
        cfile_open_fn cfo = (cfile_open_fn)(g_base + RVA_CFILE);
        memset(cfile, 0, sizeof(cfile));
        int r = cfo(cfile, csvp, 0, 0);
        printf("train: cfile_open(csvp) -> %d\n", r);
        if (r) CloseHandle(*(HANDLE *)(cfile + 8));
    }
    if (strstr(scen, "nodump")) {
        printf("train: dump skipped\n");
    } else {
        printf("train: dumped %llu merged rows\n", dump());
    }
    printf("train: done\n");
    return 0;
}
/* the M4 inject scenario host path: (1) open the pack through the
 * (possibly hooked) CFile::Open — the injector's capture seam fires
 * INSIDE that open for planned packs; (2) fill the fake pool the
 * way the vanilla loader's refill leaves it at the call site;
 * (3) call the (possibly spliced) site entry — plain C first (rax
 * forwarded = the fake runner's 0x4711), then under the EBX-sentinel
 * thunk (call discipline). */
static int mode_inject(void)
{
    static const wchar_t packpath[] = L"data\\mtfake.bin";
    unsigned char cfile[0x28];
    cfile_open_fn cfo = (cfile_open_fn)(g_base + RVA_CFILE);
    memset(cfile, 0, sizeof(cfile));
    int opened = cfo(cfile, packpath, 0, 0);
    printf("inject host: cfile_open(\"%ls\") -> %d\n", packpath, opened);

    /* the vanilla refill's result: pool object + three filled rows */
    BYTE *pool = (BYTE *)g_base + RVA_POOL;
    *(void **)pool = (void *)((BYTE *)g_base + RVA_POOL_ROWS);
    *(unsigned int *)(pool + 8) = 3;
    *(void **)(pool + 0x10) = (void *)((BYTE *)g_base + RVA_POOL_CHUNK);
    memset((BYTE *)g_base + RVA_POOL_ROWS, 0, 8 * 0x148);
    /* the shared vanilla pointer array every row's +0x38 points at
     * (the loader keeps it on its own stack in-game); two entries, at the
     * stream's cumulative bases */
    BYTE *van_arr = (BYTE *)g_base + RVA_POOL_ROWS + 8 * 0x148;
    *(void **)van_arr = (void *)((BYTE *)g_base + RVA_OUTBUF + 4);
    *(void **)(van_arr + 8) =
        (void *)((BYTE *)g_base + RVA_OUTBUF + 4 + 0x20c + 8);
    for (unsigned i = 0; i < 3; i++) {
        BYTE *r = (BYTE *)g_base + RVA_POOL_ROWS + i * 0x148;
        *(unsigned int *)(r + 0x13c) = i;
        *(unsigned int *)(r + 0x140) = 1;
        *(uintptr_t *)(r + 0x30) = 0x00D1CEB0DE110ULL; /* delegate marker */
        *(void **)(r + 0x38) = van_arr;
        r[0x40] = 0;
        r[0x41] = 0x5A; /* the loader's mode byte */
    }

    /* plain call: the stub returns to site+5 (the pushed E8-continuation),
     * which exits through its `ret` back into this call frame; rax = the
     * runner's forwarded result */
    uintptr_t rax = inject_call_site((uintptr_t)(g_base + RVA_INJ_SITE),
                                     (uintptr_t)pool);
    printf("inject host: runner rax forwarded = %#llx %s\n",
           (unsigned long long)rax,
           rax == 0x4711 ? "(PASS)" : "(FAIL — expected 0x4711)");

    uintptr_t ebx = inject_ebx_call((uintptr_t)(g_base + RVA_INJ_SITE), (uintptr_t)pool);
    printf("inject host: ebx sentinel %s (%#lx)\n",
           ebx == 0x1337 ? "PRESERVED (PASS)" : "CLOBBERED (FAIL)",
           (unsigned long)ebx);
    printf("inject host: site continuation bytes %s\n",
           memcmp((BYTE *)g_base + RVA_INJ_SITE + 5, g_famb_orig + 5, 2) == 0
               ? "untouched (PASS)"
               : "BROKEN (FAIL)");
    return 0;
}

/* the M5 tailgate scenario host path: feed the two spliced decode
 * sites the way the loaders do — a fake in-buffer whose +0x10 carries
 * the declared size, dest = the workspace out the stub checks, and
 * the canned vanilla decoded bytes the fake decoder hands back — then
 * dump the post-decode workspace the harness compares byte-exactly. */
static int mode_tailgate(const char *mode)
{
    static unsigned char vbuf[0x1000], ibuf[0x1000];
    BYTE *out = (BYTE *)g_base + RVA_TG_OUT;
    BYTE *in = (BYTE *)g_base + RVA_TG_IN;
    long vlen = read_whole("vanilla_vp.dec", vbuf, sizeof vbuf);
    long ilen = read_whole("vanilla_idat.dec", ibuf, sizeof ibuf);
    if (vlen <= 0 || ilen <= 0) {
        printf("FAIL: vanilla_vp.dec/vanilla_idat.dec missing (%ld, %ld)\n", vlen, ilen);
        return 2;
    }

    /* ---- i.dat (stage 1): declared = the vanilla decoded length ---- */
    memset(out, 0, RVA_TG_WORK);
    memset(in, 0, 0x40);
    *(unsigned int *)(in + 0x10) = (unsigned int)ilen;
    g_tg_decode_src = ibuf;
    g_tg_decode_force_len = 0;
    g_tg_decode_delta = (!strcmp(mode, "tailgate") || !strcmp(mode, "tailgate-ws")) ? 21u : 0u;
    unsigned cap = 0xC800000u;
    uintptr_t r = tg_call_site((uintptr_t)(g_base + RVA_TG_IDAT_SITE), out, &cap,
                               in + 0x14, 0x100);
    printf("tg host: i.dat site ret=%d cap=%u\n", (int)(r & 0xff), (unsigned)cap);
    write_whole("ws_idat.bin", out, (size_t)ilen + 0x800);

    /* ---- vp (stage 2): declared = the vanilla decoded length ---- */
    memset(out, 0, RVA_TG_WORK);
    memset(in, 0, 0x40);
    *(unsigned int *)(in + 0x10) = (unsigned int)vlen;
    g_tg_decode_src = vbuf;
    g_tg_decode_delta = (!strcmp(mode, "tailgate") || !strcmp(mode, "tailgate-ws")) ? 13u : 0u;
    g_tg_decode_force_len =
        (!strcmp(mode, "tailgate-mismatch")) ? (unsigned int)vlen - 8u : 0;
    cap = 0xC800000u;
    r = tg_call_site((uintptr_t)(g_base + RVA_TG_VP_SITE), out, &cap, in + 0x14, 0x100);
    printf("tg host: vp site ret=%d cap=%u\n", (int)(r & 0xff), (unsigned)cap);
    write_whole("ws_vp.bin", out, (size_t)vlen + 0x100);
    printf("tailgate host: done\n");
    return 0;
}

/* M5-U: the same tailgate host path, but for the `tp` host — stage 1
 * feeds the i.dat site (its declared size + canned decoded bytes), stage
 * 2 the tp site. The mod's preview assets + the rebuilt
 * `previewtexture.dir` listing ride the tp append. */
static int mode_tailgate_tp(void)
{
    static unsigned char tbuf[0x4000], ibuf[0x4000];
    BYTE *out = (BYTE *)g_base + RVA_TG_OUT;
    BYTE *in = (BYTE *)g_base + RVA_TG_IN;
    long tlen = read_whole("vanilla_tp.dec", tbuf, sizeof tbuf);
    long ilen = read_whole("vanilla_idat.dec", ibuf, sizeof ibuf);
    if (tlen <= 0 || ilen <= 0) {
        printf("FAIL: vanilla_tp.dec/vanilla_idat.dec missing (%ld, %ld)\n", tlen, ilen);
        return 2;
    }
    /* ---- i.dat (stage 1) ---- */
    memset(out, 0, RVA_TG_WORK);
    memset(in, 0, 0x40);
    *(unsigned int *)(in + 0x10) = (unsigned int)ilen;
    g_tg_decode_src = ibuf;
    g_tg_decode_force_len = 0;
    g_tg_decode_delta = 21u;
    unsigned cap = 0xC800000u;
    uintptr_t r = tg_call_site((uintptr_t)(g_base + RVA_TG_IDAT_SITE), out, &cap,
                               in + 0x14, 0x100);
    printf("tg host: i.dat site ret=%d cap=%u\n", (int)(r & 0xff), (unsigned)cap);
    write_whole("ws_idat.bin", out, (size_t)ilen + 0x800);
    /* ---- tp (stage 2) ---- */
    memset(out, 0, RVA_TG_WORK);
    memset(in, 0, 0x40);
    *(unsigned int *)(in + 0x10) = (unsigned int)tlen;
    g_tg_decode_src = tbuf;
    g_tg_decode_delta = 13u;
    cap = 0xC800000u;
    r = tg_call_site((uintptr_t)(g_base + RVA_TG_TP_SITE), out, &cap, in + 0x14, 0x100);
    printf("tg host: tp site ret=%d cap=%u\n", (int)(r & 0xff), (unsigned)cap);
    /* dump enough of the tail for several appended payloads + a listing */
    write_whole("ws_tp.bin", out, (size_t)tlen + 0x800);
    printf("tailgate host: done\n");
    return 0;
}

/* the M5-T swap: the loader splices boot_pack_system_driver's alloc
 * tail, so calling the site must hand the workspace over to a bigger
 * buffer allocated through the game's own _aligned_malloc (cap raised,
 * the game's buffer freed), and the shim's tail-jump must reach the
 * pinned callee with RAX flowing back. `workspace-fail` makes the
 * allocator return NULL: nothing may change. */
static int mode_workspace(const char *mode)
{
    /* the fake environment was written BEFORE the DLL loaded (the loader
     * verifies those bytes at boot and splices the site) — here we only
     * reset the bookkeeping and decide what the allocator returns */
    g_ws_malloc_calls = 0;
    g_ws_free_calls = 0;
    g_ws_malloc_size = 0;
    g_ws_freed = NULL;
    g_ws_malloc_ret = strcmp(mode, "workspace-fail") ? (void *)(g_base + RVA_WS_NEWOUT) : NULL;
    BYTE *g = (BYTE *)g_base + RVA_WS_GLOBALS;
    void *old = *(void **)g;
    uintptr_t rax = ws_call_site((uintptr_t)(g_base + RVA_WS_SITE));
    void *now = *(void **)g;
    unsigned cap = *(unsigned *)(g + 0x10);
    printf("ws host: callee rax=%#lx (%s)\n", (unsigned long)rax,
           rax == 0xDEADBEEFu ? "PASS" : "FAIL");
    printf("ws host: malloc calls=%u requested=%u MiB (%s)\n", g_ws_malloc_calls,
           g_ws_malloc_size >> 20, g_ws_malloc_size == 0x10000000u ? "PASS" : "FAIL");
    printf("ws host: out %p -> %p %s\n", old, now,
           now != old ? "(SWAPPED)" : "(unchanged)");
    printf("ws host: cap=%u (%s)\n", cap, cap == 0x10000000u ? "PASS" : "FAIL");
    printf("ws host: free calls=%u freed=%p\n", g_ws_free_calls, g_ws_freed);
    printf("workspace host: done\n");
    return 0;
}

static int mode_table113(void)
{
    DWORD *tbl = (DWORD *)(g_base + RVA_TABLE);
    printf("mode=table113: tbl[6] = %lu %s\n", (unsigned long)tbl[6],
           tbl[6] == 136 ? "(PASS)" : "(FAIL)");
    return 0;
}

static int mode_nop(void)
{
    fn_t fnp = (fn_t)(g_base + RVA_FILTER);
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
static int mode_filter(const char *mode)
{
    fn_t fnp = (fn_t)(g_base + RVA_FILTER);
    static DWORD ids[1024];
    for (int i = 0; i < 1024; i++) ids[i] = (DWORD)i;
    void *out = NULL;
    void *ret = fnp(&out, ids, ids + 1024, NULL);
    DWORD *ne = *(DWORD **)ret;
    printf("mode=%s: kept %ld / 1024 %s\n", mode, (long)(ne - ids),
           ret == &out ? "(ret==out)" : "(RET!=OUT FAIL)");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        printf("usage: host <form_list.bin> <mode>\n");
        return 2;
    }
    setvbuf(stdout, NULL, _IONBF, 0); /* crash-surviving evidence output */
    const char *mode = argv[2];
    g_base = (BYTE *)GetModuleHandleW(NULL);

    /* (a) map every fake page the loader's address book expects */
    if (!setup_filter_pages()) return 3;
    if (!setup_cfile_page()) return 3;
    g_probe_ok = setup_probe_pages();
    setup_tab_pages();
    if (!strcmp(mode, "save")) setup_save_pages();
    setup_clamp_pages();   /* always-on guard: its anchors must exist on every boot */
    setup_inject_pages(mode);
    /* M7-2: the train anchors are verified DURING the boot (on load), so their
     * fake pages must exist before the proxy DLL loads. setup_rows_pages also
     * repoints the spec-table manager at the 421-row fake table (it must run
     * after setup_tab_pages, which installs the 400-row one). */
    if (!strncmp(mode, "train", 5)) {
        if (!setup_rows_pages()) return 3;
        if (!setup_train_pages()) return 3;
        /* the car-build refusal shape: break ONE helper anchor's bytes BEFORE the
         * DLL loads, so the loader's boot-time verify refuses the car build while
         * the injection itself still proceeds (fail-open). */
        if (strstr(mode, "badhelper")) {
            ((BYTE *)g_base + RVA_MODEL_FETCH)[0] ^= 0xFF;
        }
    }

    /* real FORM list bytes -> the mapped region (NUL-terminated UTF-16). Heap,
     * not .bss: a 64K static pushed SizeOfImage past 0x30000, which would make
     * the M7-1 row_name_ctor page (RVA 0x372f0) unmappable (VirtualAlloc's 64K
     * granularity block is partially occupied by the image's tail). */
    FILE *f = fopen(argv[1], "rb");
    if (!f) {
        printf("FAIL: cannot open %s\n", argv[1]);
        return 2;
    }
    BYTE *forms = (BYTE *)VirtualAlloc(NULL, 0x10000, MEM_COMMIT | MEM_RESERVE,
                                       PAGE_READWRITE);
    if (!forms) {
        printf("FAIL: FORM-list buffer alloc failed\n");
        fclose(f);
        return 2;
    }
    size_t n = fread(forms, 1, 0x10000 - 2, f);
    fclose(f);
    forms[n] = 0;
    forms[n + 1] = 0;
    memcpy((BYTE *)g_base + RVA_FORMS, forms, n + 2);
    VirtualFree(forms, 0, MEM_RELEASE);

    /* (b) load the proxy under test (app-dir search hits our version.dll) */
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

    /* (c) dispatch on the mode string to one handler per mode */
    if (!strcmp(mode, "none")) return mode_none();
    if (!strcmp(mode, "overlaypack") || !strcmp(mode, "overlaycsv"))
        return mode_overlaypackcsv(mode);
    if (!strcmp(mode, "overlaydat")) return mode_overlaydat();
    if (!strcmp(mode, "overlaytp")) return mode_overlaytp();
    if (!strcmp(mode, "loose")) return mode_loose();
    if (!strcmp(mode, "probe")) return mode_probe();
    if (!strcmp(mode, "tabfix")) return mode_tabfix();
    if (!strcmp(mode, "formfix")) return mode_formfix();
    if (!strcmp(mode, "window")) return mode_window();
    if (!strcmp(mode, "save")) return mode_save();
    if (!strcmp(mode, "clamp")) return mode_clamp();
    if (!strncmp(mode, "rows", 4)) return mode_rows(mode);
    if (!strcmp(mode, "train") || !strcmp(mode, "train-nodump")) return mode_train(mode, 0);
    if (!strcmp(mode, "train_retry")) return mode_train(mode, 1);
    if (!strcmp(mode, "train_carbuild")) return mode_train(mode, 1);
    if (!strcmp(mode, "train_badhelper")) return mode_train(mode, 1);
    if (!strcmp(mode, "inject")) return mode_inject();
    if (!strcmp(mode, "tailgate") || !strcmp(mode, "tailgate-mismatch")
        || !strcmp(mode, "tailgate-ws"))
        return mode_tailgate(mode);
    if (!strcmp(mode, "tailgate-tp")) return mode_tailgate_tp();
    if (!strcmp(mode, "workspace") || !strcmp(mode, "workspace-fail"))
        return mode_workspace(mode);
    if (!strcmp(mode, "table113")) return mode_table113();
    if (!strcmp(mode, "nop")) return mode_nop();

    /* filter / model / modeltex */
    return mode_filter(mode);
}
