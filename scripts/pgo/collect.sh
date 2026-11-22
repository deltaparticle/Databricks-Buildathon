#!/bin/bash
# PGO step 2 (local): run the instrumented pipeline on training datasets (never-scored
# generator seeds) and merge the raw profiles into express/pgo/merged.profdata.
# Usage: scripts/pgo/collect.sh RUN_IMAGE DATA_DIR...
#   RUN_IMAGE runs /pipeline built with --features pgo-gen (on a CPU without AVX-512,
#   an image that runs it under Intel SDE). The merge uses the image ss-express:pgotools
#   (scripts/pgo/Dockerfile.gen --target tools), whose LLVM matches the scored build's.
set -eo pipefail
export MSYS_NO_PATHCONV=1  # Git Bash: keep /data etc. as container paths
cd "$(dirname "$0")/../.."
image=$1; shift
prof=$(mktemp -d)
chmod 777 "$prof"
for data in "$@"; do
    out=$(mktemp -d); chmod 777 "$out"
    docker run --rm --network none --user 1000:1000 -e NCPU=8 \
        -e DATA_DIR=/data -e OUTPUT_DIR=/output -e LLVM_PROFILE_FILE=/prof/$(basename "$data")-%p.profraw \
        -v "$(cygpath -w "$data" 2>/dev/null || echo "$data"):/data:ro" -v "$(cygpath -w "$out" 2>/dev/null || echo "$out"):/output" \
        -v "$(cygpath -w "$prof" 2>/dev/null || echo "$prof"):/prof" "$image"
    rm -rf "$out"
    echo "profiled $data"
done
ls -la "$prof"
mkdir -p express/pgo
docker run --rm -v "$(cygpath -w "$prof" 2>/dev/null || echo "$prof"):/prof" ss-express:pgotools \
    sh -c 'llvm-profdata merge -o /prof/merged.profdata /prof/*.profraw && llvm-profdata show /prof/merged.profdata | tail -4'
cp "$prof/merged.profdata" express/pgo/merged.profdata
rm -rf "$prof"
ls -la express/pgo/merged.profdata
