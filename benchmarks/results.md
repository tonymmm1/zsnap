# zsnap ZFS benchmark

> **Synthetic-result warning:** These pools use sparse files on `btrfs`, not physical vdevs. Results primarily measure process spawning, ZFS control-path work, discovery, and policy planning. They do **not** predict HDD, SSD, or NVMe throughput, latency, contention, durability, or production scaling.

Generated 2026-09-01 19:12:31 UTC from a working tree based on commit `58943ca`.

## Environment

| Item | Value |
| --- | --- |
| OS | Omarchy |
| Kernel | `Linux 7.1.9-arch1-2 x86_64 GNU/Linux` |
| CPU | AMD Ryzen Threadripper PRO 5975WX 32-Cores |
| ZFS | `zfs-2.4.4-1;zfs-kmod-2.4.4-1` |
| zsnap | `zsnap 0.1.0` |
| Sanoid stable | `sanoid version 2.3.0` |
| Sanoid development | `sanoid version 2.3.0`, master `d39b51a` |
| Topology | 3 pools × 13 managed datasets (39 total), 3 branches × 3 leaves |
| Sparse vdev | `512M` per pool on `btrfs` |
| Prune workload | 6 snapshots/dataset (234 total) |
| Sampling | 1 warm-up + 5 trials/scenario; alternating forward/reverse order |

## Results

Times are end-to-end wall-clock durations. Mutation calls count only the `zfs snapshot` or `zfs destroy` processes implied by the verified workload; discovery is included in time but not that column.

| Phase | Scenario | Max pools | Snapshot batch | Prune batch | Mutation calls | Median | Mean | Min–max | vs Sanoid stable |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| snapshot | Sanoid stable | n/a | n/a | n/a | 39 | 1270.590 ms | 1274.327 ms | 1259.477–1289.640 ms | 1.00× |
| snapshot | Sanoid development | n/a | n/a | n/a | 39 | 1568.546 ms | 1562.602 ms | 1547.169–1575.056 ms | 0.81× |
| snapshot | zsnap serial, unbatched | 1 | 1 | 1 | 39 | 945.238 ms | 951.681 ms | 941.658–969.324 ms | 1.34× |
| snapshot | zsnap serial, default batches | 1 | 128 | 64 | 3 | 508.969 ms | 508.091 ms | 504.118–511.754 ms | 2.50× |
| snapshot | zsnap 2 pools, default batches | 2 | 128 | 64 | 3 | 451.706 ms | 452.315 ms | 449.901–455.481 ms | 2.81× |
| snapshot | zsnap auto pools, unbatched | 0 (auto) | 1 | 1 | 39 | 546.044 ms | 550.582 ms | 536.682–576.810 ms | 2.33× |
| snapshot | zsnap auto pools, small batches | 0 (auto) | 4 | 3 | 12 | 441.819 ms | 444.949 ms | 437.102–460.697 ms | 2.88× |
| snapshot | zsnap auto pools, defaults | 0 (auto) | 128 | 64 | 3 | 401.958 ms | 400.612 ms | 395.056–404.703 ms | 3.16× |
| prune | Sanoid stable | n/a | n/a | n/a | 234 | 5289.418 ms | 5303.045 ms | 5186.027–5445.699 ms | 1.00× |
| prune | Sanoid development | n/a | n/a | n/a | 234 | 6378.662 ms | 6388.533 ms | 6362.269–6421.343 ms | 0.83× |
| prune | zsnap serial, unbatched | 1 | 1 | 1 | 234 | 4929.588 ms | 4906.750 ms | 4783.667–4982.205 ms | 1.07× |
| prune | zsnap serial, default batches | 1 | 128 | 64 | 39 | 2064.229 ms | 2051.913 ms | 2021.875–2069.009 ms | 2.56× |
| prune | zsnap 2 pools, default batches | 2 | 128 | 64 | 39 | 1608.119 ms | 1614.459 ms | 1604.363–1633.294 ms | 3.29× |
| prune | zsnap auto pools, unbatched | 0 (auto) | 1 | 1 | 234 | 2283.518 ms | 2244.844 ms | 2114.342–2330.661 ms | 2.32× |
| prune | zsnap auto pools, small batches | 0 (auto) | 4 | 3 | 78 | 1452.039 ms | 1458.920 ms | 1409.298–1503.347 ms | 3.64× |
| prune | zsnap auto pools, defaults | 0 (auto) | 128 | 64 | 39 | 1245.211 ms | 1237.723 ms | 1211.816–1253.180 ms | 4.25× |

## Prune path diagnostic

A separate warm, non-mutating `zsnap plan --scope prune` pass used the same 234 managed snapshots to isolate fresh discovery, configuration, and policy planning from deletion.

| Measurement | Median | Interpretation |
| --- | ---: | --- |
| Full zsnap auto/default prune | 1245.211 ms | Discovery, planning, and 39 batched destroy processes |
| Warm prune plan only | 800.485 ms | Fresh discovery and policy work; no mutation |
| ↳ Configured-root dataset list | 151.403 ms | One scoped `zfs list` process |
| ↳ Managed-root snapshot/property list | 644.042 ms | One scoped `zfs list` process |
| ↳ Configured-pool capacity list | 7.294 ms | One scoped `zpool list` process |
| ↳ Other plan work (approximate) | 0.000 ms | Locking, TOML, parsing, policy, rendering, and median subtraction noise |
| Full minus plan (approximate) | 444.726 ms | Mutation/process residual; not independently timed |


## What this run says

- Sanoid development took **1.23×** the stable snapshot time and **1.21×** the stable prune time on this isolated topology.
- Warm fresh discovery and planning were **64.3%** of the fastest full prune median. That is the maximum share a perfect plan cache could remove, but deletion candidates still require fresh revalidation for safety.
- The fresh snapshot/property scan alone was **80.5%** of plan time and **51.7%** of full prune time, making that native query the first optimization target.
- Fastest zsnap snapshot scenario: **zsnap auto pools, defaults (401.958 ms, 3.16× Sanoid stable)**.
- Fastest zsnap prune scenario: **zsnap auto pools, defaults (1245.211 ms, 4.25× Sanoid stable)**.
- At one pool worker, default batching changed snapshot time by **1.86×** and prune time by **2.39×** versus batch size 1.
- With default batches, auto pool workers changed snapshot time by **1.27×** and prune time by **1.66×** versus one worker.
- Auto defaults versus small batches (4 snapshot / 3 prune) changed snapshot time by **1.10×** and prune time by **1.17×**. Once a batch fits all eligible targets, a larger cap cannot reduce command count further.

Ratios are first duration ÷ second duration, so values above 1 favor the second setting. Differences near 1 are synthetic noise.

## Improvement guidance

- Keep batching simple. Defaults already collapse this workload to one snapshot command per pool and one destroy command per dataset; media presets would not reduce those counts.
- Keep same-pool mutations serialized and overlap independent pools. Validate `max_parallel_pools = 0` on real hardware; cap it when pools share controllers, CPU, or memory bandwidth.
- Keep inventory fresh for deletion safety. zsnap now scopes dataset and snapshot scans to configured roots and capacity queries to configured pools; optimize those native reads before considering a stale cross-run cache.
- A recursive `zfs destroy -r root@snap1,snap2` can reduce this uniform tree to one call per pool, but is unsafe unless every matching descendant/name is an explicitly validated prune candidate and no dataset has an override or hook.
- A channel program can destroy exact candidates in one root-only invocation per pool, but adds Lua, instruction/memory ceilings, blocks concurrent administrative changes during execution, and can stop after partial application. Keep it a future opt-in unless real enormous prune sets justify it.
- The current portable default already uses OpenZFS comma-list deletion per dataset and preserves granular errors and hooks.
- Repeat on physical HDD, SSD, and NVMe pools before changing production settings. Sparse files are useful for regressions, not device tuning.

## Method

Each snapshot trial starts empty and must create one hourly snapshot per dataset. Each prune trial seeds 6 expired Sanoid-compatible snapshots per dataset; zsnap seeds carry `org.zsnap:managed=yes`, while Sanoid seeds are unmarked. A successful prune returns to zero.

Both Sanoid versions and zsnap manage the same recursive roots and policy. Sanoid uses `--force-update` because the harness changes state externally. Setup, seeding, correctness checks, and cleanup are outside timed regions. The warm plan diagnostic and its three native inventory commands are timed separately on one seeded inventory. No Sanoid source is copied into this repository.

Raw timings: [`results.tsv`](results.tsv). Reproduce with `make benchmark`; see [`README.md`](README.md).
