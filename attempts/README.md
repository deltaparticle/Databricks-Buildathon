# Earlier attempts

Everything we built or submitted before the current shipped pipeline (`lane/`) is kept
in this repository. Each scored version can also be rebuilt exactly from its commit
(`git checkout <commit>` and build the Dockerfile at the root).

| Attempt | Where the code is | Commit | Board score |
|---|---|---|---|
| Our own Rust pipeline (exact native implementation of `rules.md`) | `src/` (also the reference CI diffs against) | first submitted around `6510e95`, last around `ba90900` | 1.27 s, then 1.16 s, then 1.13 s |
| C++ pipeline adapted from Dremix10/shopping-sessions-fast `8452f3a` | history only | `5355eaa` | 0.66 s |
| C++ pipeline, upstream Dremix10 `954c08d` with our build | `attempts/cpp-1bacb58/` (exact snapshot, with its Dockerfile) | `1bacb58` | 0.64 s |
| C++ dense-index experiment (event/click IDs decoded to generation indices; exact, but not faster) | `cpp/` | `95cb0e0` | not submitted |
| Team 67's Express Lane `612962f`, unused code removed | history only | `d3fb904` | 0.2821 s |
| Same, smaller user buckets and R3 off (CI 1.7-2.2% faster) | history only | `565cc3b` | 0.2873 s (worse; reverted) |
| Team 67's Express Lane `078e760` (CI 2.2-2.3% faster than `612962f`) | history only | `8e6fe53` | not submitted |
| Team 67's Express Lane `3493fdd` (same speed as `078e760`, plus two parse fixes) | `lane/` (shipped) | current `main` | pending |

The commits of the three Rust submissions are approximate: the scores were reported in
chat without the exact hashes. `1bacb58` and `5355eaa` are exact (see the commit
message of `9f0953e`).
