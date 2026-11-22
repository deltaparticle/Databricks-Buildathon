#!/usr/bin/env python3
"""
Runs the image the way the scorer does (same flags as docker_run.sh), from any OS:
    docker_run.py DATA_DIR OUTPUT_DIR
Env: IMAGE (shopping-sessions:latest), CPUS (8), MEM (24g), CPUSET (optional, e.g. 0-7);
every SS_* and XL_* variable is passed on to the container, and NCPU when set (else CPUS).
Prints the wall time of the container run to stderr.
"""
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def data_mount(arg):
    """A host folder, or `vol:VOLUME/SUBPATH` for a folder inside a named Docker volume."""
    if arg.startswith("vol:"):
        volume, _, sub = arg[4:].partition("/")
        spec = f"type=volume,src={volume},dst=/data,readonly"
        return ["--mount", spec + (f",volume-subpath={sub}" if sub else "")]
    return ["-v", f"{Path(arg).resolve()}:/data:ro"]


def main():
    data, out = sys.argv[-2], Path(sys.argv[-1]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    image = os.environ.get("IMAGE", "shopping-sessions:latest")
    cpus = os.environ.get("CPUS", "8")
    mem = os.environ.get("MEM", "24g")
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as scratch:
        os.chmod(out, 0o777)
        os.chmod(scratch, 0o777)
        cmd = ["docker", "run", "--rm", "--platform", "linux/amd64",
               "--network", "none", "--read-only", "--tmpfs", "/tmp:rw,size=64m,mode=1777",
               "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
               "--pids-limit", "4096", "--cpus", cpus, "--memory", mem, "--memory-swap", mem,
               "--user", "1000:1000",
               *data_mount(data), "-v", f"{out}:/output", "-v", f"{scratch}:/scratch",
               "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/scratch",
               "-e", "HOME=/scratch", "-e", f"NCPU={os.environ.get('NCPU') or cpus.split('.')[0]}", "-e", f"MEM_GB={mem.rstrip('g')}"]
        # Knobs of the pipeline (SS_* ours, XL_* the express-lane ones) reach the container.
        for k, v in sorted(os.environ.items()):
            if k.startswith(("SS_", "XL_")):
                cmd += ["-e", f"{k}={v}"]
        if os.environ.get("CPUSET"):
            cmd[3:3] = ["--cpuset-cpus", os.environ["CPUSET"]]
        cmd.append(image)
        start = time.perf_counter()
        p = subprocess.run(cmd)
        print(f"container wall time {time.perf_counter() - start:.3f}s", file=sys.stderr)
        sys.exit(p.returncode)


if __name__ == "__main__":
    main()
