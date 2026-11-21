// Parse-speed floor: how fast can event and click CSV rows be decoded into
// fixed records with AVX-512 + SWAR, with nothing downstream?  Decodes every
// field the pipeline needs (base-36 IDs, all common timestamp forms, type,
// product, price), strips quotes, writes one 32-byte record per row into a
// per-thread buffer, and checksums.  Rare forms go to a counted slow path.
// Usage: parsefloor DATA_DIR NTHREADS [MODE]   MODE: full (default) | scan
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <immintrin.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

typedef struct { char path[600]; size_t size; int kind; uint8_t *map; } File;  // kind 0 events, 1 clicks
static File files[1024];
static int nfiles;
#define CHUNK (4u << 20)
typedef struct { int f; size_t off, len; } Job;
static Job jobs[1 << 16];
static int njobs;
static atomic_int next_job;
static int scan_only;

typedef struct { uint64_t a, b, c, d; } Rec;

static double now_ms(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }

static void add_dir(const char *root, const char *sub, int kind) {
  char d[512]; snprintf(d, sizeof d, "%s/%s", root, sub);
  DIR *dir = opendir(d); if (!dir) return;
  struct dirent *e;
  while ((e = readdir(dir))) {
    size_t n = strlen(e->d_name);
    if (n < 4 || strcmp(e->d_name + n - 4, ".csv")) continue;
    File *f = &files[nfiles++];
    snprintf(f->path, sizeof f->path, "%s/%s", d, e->d_name);
    struct stat st; stat(f->path, &st); f->size = st.st_size; f->kind = kind;
  }
  closedir(dir);
}

// ---- field decoders (SWAR) ----
static inline uint64_t ld8(const uint8_t *p) { uint64_t v; memcpy(&v, p, 8); return v; }

// base-36 value of len (1..8) chars at p
static inline uint64_t b36(const uint8_t *p, unsigned len) {
  uint64_t x = ld8(p) << (8 * (8 - len));
  uint64_t nz = ~0ull << (8 * (8 - len));
  const uint64_t O = 0x0101010101010101ull;
  uint64_t let = ((x | ~nz) + O * (0x80 - 'a')) & O * 0x80;  // byte >= 'a'
  uint64_t d = x - (nz & O) * '0' - (let >> 7) * 39;
  uint64_t p2 = (d & 0x00FF00FF00FF00FFull) * 36 + ((d >> 8) & 0x00FF00FF00FF00FFull);
  uint64_t p4 = (p2 & 0x0000FFFF0000FFFFull) * 1296 + ((p2 >> 16) & 0x0000FFFF0000FFFFull);
  return (p4 & 0xFFFFFFFFull) * 1679616ull + (p4 >> 32);
}

// decimal value of len (1..8) digits at p
static inline uint64_t dec8(const uint8_t *p, unsigned len) {
  uint64_t x = (ld8(p) << (8 * (8 - len))) - 0x3030303030303030ull * 1;
  x &= ~0ull << (8 * (8 - len));
  x = (x & 0x0F0F0F0F0F0F0F0Full);
  x = (x * 10 + (x >> 8)) & 0x00FF00FF00FF00FFull;
  x = (x * 100 + (x >> 16)) & 0x0000FFFF0000FFFFull;
  return (x * 10000 + (x >> 32)) & 0xFFFFFFFFull;
}

static inline int64_t days_from_civil(int64_t y, int64_t m, int64_t d) {
  y -= m <= 2; int64_t era = (y >= 0 ? y : y - 399) / 400; int64_t yoe = y - era * 400;
  int64_t doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1;
  return era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
}

typedef struct { uint64_t key; int64_t ms; } DateMemo;

// "YYYY-MM-DDTHH:MM:SS" at p -> epoch ms (date part memoized)
static inline int64_t iso(const uint8_t *p, DateMemo *dm) {
  uint64_t key = ld8(p) ^ ((uint64_t)p[8] << 56) ^ ((uint64_t)p[9] << 48);
  if (key != dm->key) {
    int64_t y = dec8(p, 4), mo = dec8(p + 5, 2), d = dec8(p + 8, 2);
    dm->key = key; dm->ms = days_from_civil(y, mo, d) * 86400000ll;
  }
  uint64_t t = ld8(p + 11) - 0x3030303030303030ull;  // HH:MM:SS
  int64_t hh = (t & 0xFF) * 10 + ((t >> 8) & 0xFF), mm = ((t >> 24) & 0xFF) * 10 + ((t >> 32) & 0xFF), ss = ((t >> 48) & 0xFF) * 10 + ((t >> 56) & 0xFF);
  return dm->ms + (hh * 3600 + mm * 60 + ss) * 1000;
}

static inline int64_t ts_field(const uint8_t *p, unsigned len, DateMemo *dm, uint64_t *slow) {
  if (len == 20 && p[19] == 'Z') return iso(p, dm);
  if (len == 13) return (int64_t)dec8(p, 5) * 100000000ll + dec8(p + 5, 8);
  if (len == 25 && (p[19] == '+' || p[19] == '-')) {
    int64_t off = (int64_t)(dec8(p + 20, 2) * 60 + dec8(p + 23, 2)) * 60000;
    return iso(p, dm) - (p[19] == '+' ? off : -off);
  }
  (*slow)++;
  return 0;
}

typedef struct { uint64_t sum, rows, slow, quoted; Rec *out; size_t nout; char pad[64]; } TState;

// one line [s, e) with the comma positions of up to 8 fields
static inline void event_line(const uint8_t *s, const uint8_t *e, const uint32_t *cpos, int nf, TState *t, DateMemo *dm) {
  if (nf != 7) { t->slow++; return; }
  const uint8_t *fs[7]; unsigned fl[7];
  uint32_t prev = 0;
  for (int i = 0; i < 7; i++) {
    uint32_t end = i < 6 ? cpos[i] : (uint32_t)(e - s);
    const uint8_t *a = s + prev; unsigned n = end - prev;
    if (n >= 2 && a[0] == '"') { a++; n -= 2; t->quoted++; }
    fs[i] = a; fl[i] = n; prev = end + 1;
  }
  uint64_t eid = fl[0] >= 2 && fl[0] <= 9 ? b36(fs[0] + 1, fl[0] - 1) : (t->slow++, 0);
  uint64_t uid = fl[1] >= 2 && fl[1] <= 9 ? b36(fs[1] + 1, fl[1] - 1) : (t->slow++, 0);
  int64_t ts = ts_field(fs[2], fl[2], dm, &t->slow);
  int64_t ing = ts_field(fs[3], fl[3], dm, &t->slow);
  unsigned type = fs[4][0];
  uint64_t prod = fl[5] >= 2 && fl[5] <= 9 ? dec8(fs[5] + 1, fl[5] - 1) : 0;
  uint64_t price = fl[6] && fl[6] <= 8 ? dec8(fs[6], fl[6]) : 0;
  Rec r = { eid ^ uid << 40, (uint64_t)ts, (uint64_t)ing, prod | price << 20 | (uint64_t)type << 56 };
  t->out[t->nout++ & ((1u << 16) - 1)] = r;
  t->sum += r.a ^ r.b ^ r.c ^ r.d;
  t->rows++;
}

static inline void click_line(const uint8_t *s, const uint8_t *e, const uint32_t *cpos, int nf, TState *t, DateMemo *dm) {
  if (nf != 4) { t->slow++; return; }
  const uint8_t *fs[4]; unsigned fl[4];
  uint32_t prev = 0;
  for (int i = 0; i < 4; i++) {
    uint32_t end = i < 3 ? cpos[i] : (uint32_t)(e - s);
    const uint8_t *a = s + prev; unsigned n = end - prev;
    if (n >= 2 && a[0] == '"') { a++; n -= 2; t->quoted++; }
    fs[i] = a; fl[i] = n; prev = end + 1;
  }
  uint64_t cid = fl[0] >= 2 && fl[0] <= 9 ? b36(fs[0] + 1, fl[0] - 1) : 0;
  uint64_t uid = fl[1] >= 2 && fl[1] <= 9 ? b36(fs[1] + 1, fl[1] - 1) : 0;
  uint64_t camp = fl[2] >= 3 && fl[2] <= 10 ? dec8(fs[2] + 2, fl[2] - 2) : 0;
  int64_t ts = ts_field(fs[3], fl[3], dm, &t->slow);
  Rec r = { cid, uid, (uint64_t)ts, camp };
  t->out[t->nout++ & ((1u << 16) - 1)] = r;
  t->sum += r.a ^ r.b ^ r.c ^ r.d;
  t->rows++;
}

static void run_job(const Job *j, TState *t) {
  const File *f = &files[j->f];
  const uint8_t *base = f->map;
  size_t off = j->off, end = j->off + j->len;
  // align start to the line after the first newline (except at file start; skip header there)
  if (off != 0) { while (off < f->size && base[off - 1] != '\n') off++; }
  else { while (off < f->size && base[off] != '\n') off++; off++; }
  // extend end to the end of its line
  while (end < f->size && base[end - 1] != '\n') end++;
  const __m512i nl = _mm512_set1_epi8('\n'), cm = _mm512_set1_epi8(',');
  DateMemo dm = { ~0ull, 0 };
  size_t ls = off;  // line start
  uint32_t cpos[16]; int nf = 0;
  for (size_t p = off; p < end; p += 64) {
    __m512i v = _mm512_loadu_si512(base + p);
    uint64_t mn = _mm512_cmpeq_epi8_mask(v, nl), mc = _mm512_cmpeq_epi8_mask(v, cm);
    if (p + 64 > end) { uint64_t keep = end - p >= 64 ? ~0ull : (1ull << (end - p)) - 1; mn &= keep; mc &= keep; }
    if (scan_only) { t->sum += __builtin_popcountll(mn) + __builtin_popcountll(mc); t->rows += __builtin_popcountll(mn); continue; }
    uint64_t m = mn | mc;
    while (m) {
      unsigned i = __builtin_ctzll(m); m &= m - 1;
      size_t at = p + i;
      if ((mn >> i) & 1) {
        if (f->kind == 0) event_line(base + ls, base + at, cpos, nf + 1, t, &dm);
        else click_line(base + ls, base + at, cpos, nf + 1, t, &dm);
        ls = at + 1; nf = 0;
      } else if (nf < 15) {
        cpos[nf++] = (uint32_t)(at - ls);
      } else nf++;
    }
  }
}

static TState ts_[64];
static void *worker(void *arg) {
  TState *t = arg;
  t->out = aligned_alloc(64, sizeof(Rec) << 16);
  for (;;) {
    int j = atomic_fetch_add(&next_job, 1);
    if (j >= njobs) break;
    run_job(&jobs[j], t);
  }
  return NULL;
}

int main(int argc, char **argv) {
  int nt = atoi(argv[2]);
  scan_only = argc > 3 && !strcmp(argv[3], "scan");
  double t0 = now_ms();
  add_dir(argv[1], "events", 0); add_dir(argv[1], "clicks", 1);
  size_t bytes = 0;
  for (int i = 0; i < nfiles; i++) {
    File *f = &files[i]; bytes += f->size;
    if (!f->size) continue;
    int fd = open(f->path, O_RDONLY);
    // reserve file size + one zero page, then map the file over the front, so
    // loads up to 64 bytes past the end read zeros instead of faulting
    uint8_t *r = mmap(NULL, f->size + 8192, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    f->map = mmap(r, f->size, PROT_READ, MAP_PRIVATE | MAP_FIXED | MAP_POPULATE, fd, 0);
    close(fd);
    for (size_t o = 0; o < f->size; o += CHUNK) { jobs[njobs].f = i; jobs[njobs].off = o; jobs[njobs].len = f->size - o < CHUNK ? f->size - o : CHUNK; njobs++; }
  }
  double t1 = now_ms();
  pthread_t th[64];
  for (int i = 0; i < nt; i++) pthread_create(&th[i], NULL, worker, &ts_[i]);
  for (int i = 0; i < nt; i++) pthread_join(th[i], NULL);
  double t2 = now_ms();
  uint64_t sum = 0, rows = 0, slow = 0, quoted = 0;
  for (int i = 0; i < nt; i++) { sum += ts_[i].sum; rows += ts_[i].rows; slow += ts_[i].slow; quoted += ts_[i].quoted; }
  printf("%s %d threads: map %.1f ms, parse %.1f ms (%.2f GB/s), rows %llu, slow %llu, quoted fields %llu, sum %llx\n",
         scan_only ? "scan" : "full", nt, t1 - t0, t2 - t1, bytes / ((t2 - t1) * 1e6), (unsigned long long)rows,
         (unsigned long long)slow, (unsigned long long)quoted, (unsigned long long)sum);
  return 0;
}
