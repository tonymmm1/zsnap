# ZFS benchmark

This benchmark exercises the real Sanoid stable, Sanoid development, and zsnap
command paths against uniquely named, disposable sparse-file zpools. It creates
nested datasets, measures snapshot creation and retention pruning, verifies every
result, and emits Markdown, standalone HTML, and raw TSV reports.

The harness never imports, exports, or destroys an existing pool. It only destroys
pools created by the current run whose names match its private
`zsnapbench_<timestamp>_<pid>_p<number>` pattern. Exit and interrupt traps clean up
those pools.

## Requirements

- A loaded, working OpenZFS installation with `zfs`, `zpool`, and sparse-file vdev
  support.
- Root access for pool creation and mutation. `make` builds zsnap as the invoking
  user before the harness asks `sudo` to re-execute only the ZFS workload.
- Git and network access for the default temporary Sanoid checkouts.
- Perl with Sanoid's `Config::IniFiles` and `Capture::Tiny` modules. Verify them
  with `perl -MConfig::IniFiles -MCapture::Tiny -e 1`.
- Bash, GNU `date`, `awk`, `sed`, `sort`, `truncate`, and standard core utilities.

Run the default comparison with:

```console
make benchmark
```

The wrapper shallow-clones stable tag `v2.3.0` and the current `master` branch of
the upstream Sanoid repository beneath a randomly named `/var/tmp` directory. It
records the development commit in the report and deletes both checkouts on exit.
Sanoid is a benchmark-only dependency: no Sanoid source is copied into or tracked
by this repository, and zsnap's runtime remains a single Rust executable.

To compare offline local checkouts or pin exact revisions, provide both binaries
and defaults files:

```console
make benchmark \
  BENCHMARK_SANOID_STABLE=/path/to/sanoid-stable/sanoid \
  BENCHMARK_SANOID_STABLE_DEFAULTS=/path/to/sanoid-stable/sanoid.defaults.conf \
  BENCHMARK_SANOID_DEVELOPMENT=/path/to/sanoid-development/sanoid \
  BENCHMARK_SANOID_DEVELOPMENT_DEFAULTS=/path/to/sanoid-development/sanoid.defaults.conf \
  BENCHMARK_SANOID_DEVELOPMENT_REVISION=d39b51a \
  BENCHMARK_SANOID_PERL5LIB=/path/to/local/perl5/lib/perl5
```

The backwards-compatible `BENCHMARK_SANOID` and
`BENCHMARK_SANOID_DEFAULTS` variables select the stable executable only. The
fetch wrapper also accepts `SANOID_REPOSITORY`, `SANOID_STABLE_REF`, and
`SANOID_DEVELOPMENT_REF` through the environment.

Generated files:

- [`results.md`](results.md): full tables, method, caveats, and findings;
- [`results.html`](results.html): standalone styled report;
- [`results.tsv`](results.tsv): raw per-trial timings for independent analysis.

## Workload and settings

The default topology is three 512 MiB sparse-file pools. Each contains one
managed root, three branch datasets, and three leaf datasets below each branch:
13 managed datasets per pool and 39 total. Snapshot trials create one hourly
snapshot per dataset. Prune trials remove six expired snapshots per dataset.

Eight scenarios isolate the relevant controls:

| Scenario | `max_parallel_pools` | Snapshot batch | Prune batch |
| --- | ---: | ---: | ---: |
| Sanoid stable | n/a | n/a | n/a |
| Sanoid development | n/a | n/a | n/a |
| zsnap serial, unbatched | 1 | 1 | 1 |
| zsnap serial, default batches | 1 | 128 | 64 |
| zsnap 2 pools, default batches | 2 | 128 | 64 |
| zsnap auto pools, unbatched | 0 | 1 | 1 |
| zsnap auto pools, small batches | 0 | 4 | 3 |
| zsnap auto pools, defaults | 0 | 128 | 64 |

Every scenario gets one warm-up. Five measured trials alternate forward and
reverse scenario order to reduce ordering bias. Timed regions include normal
inventory, configuration, planning, and mutation work. Pool creation, snapshot
seeding, correctness checks, and cleanup are excluded.

After the prune trials, the harness separately times a warm non-mutating zsnap
prune plan and its three native inventory calls. This distinguishes fresh ZFS
enumeration from policy work and mutation/process overhead. It is diagnostic,
not a claim that independently timed commands add perfectly to the end-to-end
measurement.

Environment variables make the workload adjustable without editing the script:

```console
BENCHMARK_TRIALS=3 \
BENCHMARK_POOLS=3 \
BENCHMARK_BRANCHES=2 \
BENCHMARK_LEAVES=4 \
BENCHMARK_PRUNE_SNAPSHOTS=8 \
BENCHMARK_SPARSE_SIZE=1G \
make benchmark
```

Equivalent flags can be supplied through `BENCHMARK_ARGS`, for example
`make benchmark BENCHMARK_ARGS='--trials 3 --sparse-size 1G'`. Set
`BENCHMARK_KEEP_WORKDIR=1` to retain generated configurations and command logs;
the disposable pools are still destroyed.

## Interpreting pruning results

The current portable path uses comma-separated native `zfs destroy` batches per
dataset. OpenZFS also supports recursive destruction for matching snapshot names,
but a recursive call is safe only when every affected descendant was independently
validated as a prune candidate. See
[`zfs-destroy(8)`](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-destroy.8.html).

Channel programs can reduce command-launch overhead, but they are root-only,
operate on one pool, block concurrent administrative operations while running,
have resource limits, and do not roll back already-applied operations after every
failure. They remain a possible opt-in for unusually large real workloads, not
the simple default. See
[`zfs-program(8)`](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-program.8.html).

Persistent caching is also intentionally not the default for deletion candidates.
A safe prune still needs a fresh inventory/revalidation pass, and stale candidates
are more dangerous than the time saved. The benchmark instead led to scoping
dataset, snapshot, and capacity queries to configured roots and pools.

## Limitations

Sparse-file vdevs are cheap and repeatable but unrealistic. Results are useful for
finding control-path regressions and comparing process spawning, inventory,
planning, batching, and independent-pool workers. They are not storage benchmarks
and cannot establish the right concurrency for HDD, SSD, or NVMe pools. Repeat the
workload on representative physical pools before changing production defaults.
