/* Print the process environment a container gives us: seccomp, speculation
   mitigations, cgroup limits, THP mode. Static, for a FROM scratch image. */
#include <stdio.h>
#include <string.h>

static void cat(const char *path, const char *filter) {
    FILE *f = fopen(path, "r");
    char line[512];
    if (!f) { printf("%s: (unreadable)\n", path); return; }
    while (fgets(line, sizeof line, f)) {
        if (!filter || strstr(line, filter)) printf("%s: %s", path, line);
    }
    fclose(f);
}

int main(void) {
    cat("/proc/self/status", "Seccomp");
    cat("/proc/self/status", "Speculation");
    cat("/proc/self/status", "NoNewPrivs");
    cat("/proc/self/status", "Cpus_allowed_list");
    cat("/proc/self/status", "THP");
    cat("/proc/self/cgroup", 0);
    cat("/sys/fs/cgroup/cpu.max", 0);
    cat("/sys/fs/cgroup/memory.max", 0);
    cat("/sys/fs/cgroup/memory.high", 0);
    cat("/sys/fs/cgroup/memory.swap.max", 0);
    cat("/sys/kernel/mm/transparent_hugepage/enabled", 0);
    cat("/sys/kernel/mm/transparent_hugepage/defrag", 0);
    cat("/proc/sys/vm/overcommit_memory", 0);
    return 0;
}
