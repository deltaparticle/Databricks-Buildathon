// Lower bounds for any pipeline that reads its whole input.
// Usage: floor MODE DATA_DIR NTHREADS
//   MODE=touch: mmap every CSV and read one byte per 4 KB page (page-fault cost)
//   MODE=sum:   mmap every CSV and read every byte (memory bandwidth)
//   MODE=pread: pread every CSV in 256 KB chunks into per-thread buffers and sum
// Prints wall milliseconds and a checksum.
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
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

typedef struct { char path[512]; size_t size; } File;
static File files[1024];
static int nfiles;
static int mode;
static atomic_size_t next_chunk;
static size_t total_chunks;
#define CHUNK (1u << 20)
static size_t chunk_file[1 << 16], chunk_off[1 << 16];
static uint8_t *maps[1024];

static double now_ms(void) {
  struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void add_dir(const char *root, const char *sub) {
  char d[512]; snprintf(d, sizeof d, "%s/%s", root, sub);
  DIR *dir = opendir(d); if (!dir) return;
  struct dirent *e;
  while ((e = readdir(dir))) {
    size_t n = strlen(e->d_name);
    if (n < 4 || strcmp(e->d_name + n - 4, ".csv")) continue;
    File *f = &files[nfiles++];
    snprintf(f->path, sizeof f->path, "%s/%s", d, e->d_name);
    struct stat st; stat(f->path, &st); f->size = st.st_size;
  }
  closedir(dir);
}

static void *worker(void *arg) {
  uint64_t sum = 0;
  uint8_t *buf = mode == 2 ? aligned_alloc(4096, 256 << 10) : NULL;
  int fds[1024]; if (mode == 2) for (int i = 0; i < nfiles; i++) fds[i] = open(files[i].path, O_RDONLY);
  for (;;) {
    size_t c = atomic_fetch_add(&next_chunk, 1);
    if (c >= total_chunks) break;
    size_t fi = chunk_file[c], off = chunk_off[c];
    size_t len = files[fi].size - off < CHUNK ? files[fi].size - off : CHUNK;
    if (mode == 0) {
      const volatile uint8_t *p = maps[fi] + off;
      for (size_t i = 0; i < len; i += 4096) sum += p[i];
    } else if (mode == 1) {
      const uint64_t *p = (const uint64_t *)(maps[fi] + off);
      for (size_t i = 0; i < len / 8; i++) sum += p[i];
    } else {
      for (size_t o = 0; o < len; o += 256 << 10) {
        ssize_t r = pread(fds[fi], buf, 256 << 10, off + o);
        const uint64_t *p = (const uint64_t *)buf;
        for (ssize_t i = 0; i < r / 8; i++) sum += p[i];
      }
    }
  }
  return (void *)(uintptr_t)sum;
}

int main(int argc, char **argv) {
  double t0 = now_ms();
  mode = !strcmp(argv[1], "touch") ? 0 : !strcmp(argv[1], "sum") ? 1 : 2;
  int nt = atoi(argv[3]);
  add_dir(argv[2], "events"); add_dir(argv[2], "clicks"); add_dir(argv[2], "links");
  size_t bytes = 0;
  for (int i = 0; i < nfiles; i++) {
    bytes += files[i].size;
    if (mode != 2 && files[i].size) {
      int fd = open(files[i].path, O_RDONLY);
      maps[i] = mmap(NULL, files[i].size, PROT_READ, MAP_PRIVATE, fd, 0);
      close(fd);
    }
    for (size_t o = 0; o < files[i].size; o += CHUNK) { chunk_file[total_chunks] = i; chunk_off[total_chunks++] = o; }
  }
  pthread_t th[64]; uint64_t sum = 0;
  for (int i = 0; i < nt; i++) pthread_create(&th[i], NULL, worker, NULL);
  for (int i = 0; i < nt; i++) { void *r; pthread_join(th[i], &r); sum += (uintptr_t)r; }
  printf("%s: %d files, %.0f MB, %d threads, %.1f ms (sum %llu)\n", argv[1], nfiles, bytes / 1e6, nt, now_ms() - t0, (unsigned long long)sum);
  return 0;
}
