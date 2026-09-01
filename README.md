<p align="center">
  <img src="assets/zsnap-logo.png" alt="zsnap crab mascot guarding a stack of snapshot disks" width="260">
</p>

<h1 align="center">zsnap</h1>

`zsnap` is a policy-driven ZFS snapshot manager written in Rust. It takes the
useful snapshot lifecycle ideas from [Sanoid](https://github.com/jimsalterjrs/sanoid),
uses typed TOML values with small shorthands for dataset headers and template
references, and compiles to one executable with no Perl or language runtime.
OpenZFS's `zfs` and `zpool` commands remain the only runtime facilities it invokes.
It manages snapshot creation and retention; replication is intentionally outside
its scope.

The initial implementation is usable now, but it deserves normal staging before
being trusted with irreplaceable pools. Start with `zsnap plan`, inspect every
proposed deletion, and test against a disposable ZFS pool.

## What it does

- Creates frequent, hourly, daily, weekly, monthly, and yearly snapshots using the
  host timezone by default, with an explicit UTC mode.
- Applies ordered templates, recursive dataset policies, and per-dataset overrides.
- Supports both individually managed descendants and atomic `zfs snapshot -r` trees.
- Uses Sanoid-compatible names such as
  `autosnap_2026-08-27_14:00:00_hourly`.
- Retains at least the configured number of each snapshot class and only removes
  snapshots older than that class's retention window.
- Runs different zpools concurrently, with a configurable concurrency cap.
- Sends bounded-parallel failure/success notifications to Flock, Discord, and
  Slack incoming webhooks.
- Supports pre/post snapshot and pruning hooks with timeouts and Sanoid-compatible
  environment aliases.
- Provides text or JSON plans, non-mutating dry runs, and a process-wide lock.
- Provides an atomically updated status cache with pool and per-dataset snapshot
  counts, readable through `status` or its `info` alias.
- Converts Sanoid `sanoid.conf` policy into validated zsnap TOML without modifying
  the source configuration or querying ZFS.

## Why it is efficient

`zsnap` discovers datasets and pool capacity once per invocation, and limits its
single snapshot scan to pools referenced by the configuration rather than repeatedly
querying ZFS per dataset or scanning unrelated pools. It then builds the entire plan
in memory. For execution it:

1. assigns one independent worker to each zpool;
2. combines unhooked snapshots from the same pool into bounded, atomic multi-dataset
   `zfs snapshot` calls, with separate rounds when one dataset has several due
   schedule classes;
3. combines snapshot deletions for each dataset with ZFS's comma-list syntax; and
4. serializes commands within a pool by default, avoiding extra queueing and disk
   contention on the same vdevs.

Hooks require dataset-specific ordering, so a dataset with hooks is automatically
removed from batching. `max_parallel_pools = 0` means all pools may run at once;
set a positive number to cap that concurrency.

Webhook delivery has its own bounded worker set and starts only after the ZFS run
lock has been released. HTTPS uses rustls with bundled Mozilla roots, so webhooks
do not introduce an OpenSSL runtime dependency.

### Practical tuning

The shipped values are intended to be useful without tuning. The two parallelism
settings control separate workloads and do not multiply ZFS writes:

| Setting | Default | What to do |
| --- | ---: | --- |
| `settings.max_parallel_pools` | `0` | `0` runs every independent pool concurrently. Keep it for roughly 1-4 pools; use `2` or `4` on hosts with many pools or shared controllers. |
| `settings.snapshot_batch_size` | `128` | Maximum distinct dataset targets in one ZFS command; valid range `1..=256`. OpenZFS requires separate commands for multiple due names on one dataset. It does not add threads. |
| `settings.prune_batch_size` | `64` | Maximum snapshot names destroyed per dataset command; valid range `1..=128`. It does not add threads. |
| `settings.cache_file` | `/etc/zsnap/zsnap.cache` | Reporting-only JSON summary updated after successful mutating runs. It is never used to plan snapshots or pruning. |
| `notifications.max_parallel` | `4` | Maximum simultaneous HTTP requests, unrelated to pool concurrency. It matters only when more than four webhooks are configured. |
| `notifications.timeout_seconds` | `10` | Whole-request deadline for each attempt. Increase only for a known slow internal relay. |
| `notifications.max_attempts` | `3` | Total attempts for transient failures. Keep it low because the systemd job waits for delivery to finish. |
| `notifications.retry_backoff_milliseconds` | `500` | Initial retry delay; later delays grow exponentially. |

The batch settings trade subprocess count against command size, not disk
parallelism. Keep the defaults initially; lowering them is useful for controlled
experiments, while the validated maxima keep command arguments conservative.
Batch size 1 can be useful for benchmarking or isolating failures, but it means
more sequential ZFS processes—not less parallel disk activity. HDD, SSD, and NVMe
presets are therefore not inferred; topology, vdev count, and shared controllers
matter more than the media label.

`prune_defer` is not a concurrency control. `0` applies retention on every run;
for example, `prune_defer = 80` postpones deletion while the pool is below 80%
capacity and resumes retention pruning once it reaches that threshold. The process
lock still permits only one mutating `zsnap` invocation at a time, even when a
timer overlaps a manual run.

### Why channel programs are not the default

OpenZFS channel programs are appealing for very large prune sets: one pool-scoped
Lua invocation can call `zfs.sync.destroy` repeatedly and report failures for each
candidate. They are a possible future opt-in execution backend, after measurement
on pools where subprocess overhead is material.

They are not a better general default here. Channel programs require root, operate
on only one pool, and impose instruction and memory ceilings. Their administrative
operations are isolated from concurrent administration, but a fatal resource error
can leave a program partially applied rather than rolling it back. Snapshotting
would also require separate `snapshot` and ownership-property calls inside Lua,
whereas the current `zfs snapshot -o ... dataset@snap...` command creates the
batched snapshots atomically with their ownership marker. Pool-level concurrency
still has to be orchestrated outside ZFS, which the Rust executor already does.

## Safety model

New snapshots receive the ZFS user property `org.zsnap:managed=yes`. By default,
`zsnap` only deletes snapshots carrying that property. Existing Sanoid-compatible
snapshots still count as recent snapshots when scheduling, preventing duplicates,
but they are never deleted by `zsnap` because they lack its ownership marker.
Managed snapshots with ZFS user holds (including Syncoid holds) or dependent clones
are excluded from pruning and reported as protected; zsnap never uses deferred
destroy to work around those protections.

Every generated `zfs destroy` command is checked again at the execution boundary.
Only strict `dataset@snapshot[,snapshot...]` targets without flags are permitted;
dataset/volume destruction, recursive destruction, bookmarks, and snapshot ranges
are rejected before the configured ZFS executable is started. zsnap never invokes
`zpool checkpoint -d`, so it cannot discard pool checkpoints.
This guarantee covers zsnap's built-in operations; administrator-configured hooks
are arbitrary programs and must be trusted like any other root-run hook.

Additional guardrails:

- `plan` and `--dry-run` never mutate ZFS or execute hooks.
- A locked run covers discovery, planning, snapshotting, and pruning.
- If snapshot creation or a snapshot hook fails for a pool, all pruning for that
  pool is skipped in the same run.
- A retention value is both an approximate age window and a minimum count. For
  example, `daily = 30` only prunes a daily older than 30 days when more than 30
  dailies exist.
- Snapshot names must match the configured prefix and a known period suffix.
- Hook commands are argv arrays. They are executed directly, never through an
  implicit shell.

## Build

Requirements are Rust 1.85 or newer, a C linker/compiler for the bundled TLS crypto
during compilation, and OpenZFS command-line tools at runtime.

```console
cargo build --release
cargo test --all-targets
./target/release/zsnap --help
```

The release profile enables full LTO and strips symbols. To produce a static Linux
binary when a musl toolchain is available:

```console
rustup target add x86_64-unknown-linux-musl
make static
```

The resulting binary is
`target/x86_64-unknown-linux-musl/release/zsnap`. ZFS itself remains an external
system facility: `zsnap` invokes the installed `zfs` and `zpool` tools.

For this checkout, whose Rust toolchain is managed by `mise`, the equivalent local
commands are:

```console
mise exec rust@1.85.0 -- cargo build --release --locked
mise exec rust@1.85.0 -- cargo test --all-targets --locked
# After uncommenting and naming at least one example dataset:
./target/release/zsnap --config ./config.example.toml check
```

## ZFS benchmark

`make benchmark` builds the release binary, asks for root access, creates only
uniquely named disposable sparse-file pools, and compares Sanoid stable, current
Sanoid development, and six zsnap concurrency/batch combinations. Every result
is correctness-checked. The wrapper shallow-clones the two Sanoid refs under a
temporary `/var/tmp` directory and removes them after the run; **no Sanoid source
is tracked or copied into this repository**.

```console
make benchmark
```

Git, working OpenZFS commands, root access, and Sanoid's `Config::IniFiles` and
`Capture::Tiny` Perl modules are benchmark-only prerequisites. zsnap's runtime
remains a single executable.

### Latest synthetic results

This run used ZFS 2.4.4, three sparse 512 MiB pools on btrfs, 39 nested managed
datasets, one warm-up, and five trials per scenario. Sanoid development was
master commit `d39b51a`; both Sanoid checkouts reported version 2.3.0.

| Implementation/settings | Snapshot median | Snapshot speedup vs stable | Prune median | Prune speedup vs stable |
| --- | ---: | ---: | ---: | ---: |
| Sanoid stable v2.3.0 | 1270.590 ms | 1.00× | 5289.418 ms | 1.00× |
| Sanoid development `d39b51a` | 1568.546 ms | 0.81× | 6378.662 ms | 0.83× |
| zsnap auto pools, default batches | **401.958 ms** | **3.16×** | **1245.211 ms** | **4.25×** |

The zsnap prune median broke down as follows:

| Measurement | Median | Share of full prune |
| --- | ---: | ---: |
| Full auto/default prune | 1245.211 ms | 100.0% |
| Warm fresh discovery and plan only | 800.485 ms | 64.3% |
| Snapshot/property inventory alone | 644.042 ms | 51.7% |
| Dataset inventory alone | 151.403 ms | 12.2% |
| Pool-capacity inventory alone | 7.294 ms | 0.6% |
| Full minus plan (mutation/process residual) | 444.726 ms | 35.7% |

The useful optimization target is therefore fresh, scoped snapshot/property
inventory—not a persistent deletion cache. Cached prune candidates must still be
revalidated, while stale data could delete the wrong snapshot. Batching and
independent-pool parallelism already produced most of the measured win. Recursive
destroy and channel-program tradeoffs are discussed in the full report.

The reporting cache used by `zsnap status` contains aggregate counts only. It does
not contain or replace the live snapshot inventory used for retention decisions.

These are control-path measurements on sparse files, not storage benchmarks.
They cannot predict physical HDD, SSD, or NVMe behavior. See the
[`benchmark guide`](benchmarks/README.md),
[`full Markdown report`](benchmarks/results.md),
[`standalone HTML report`](benchmarks/results.html), and
[`raw trial data`](benchmarks/results.tsv).

## Automated build and install

This command installs build prerequisites, bootstraps the pinned Rust toolchain with
official rustup, detects systemd or OpenRC, and enables scheduling only after a
read-only ZFS probe succeeds:

```console
./install.sh --install-deps --bootstrap-rust
```

Run the installer as your normal user; it invokes `sudo` only for system files and
scheduler operations. A fresh installation receives a safe starter configuration
with templates but no active dataset sections. zsnap deliberately never guesses a
pool name. Select exact names from `sudo zfs list -H -o name -t filesystem,volume`,
add them to `/etc/zsnap/zsnap.toml`, validate, and review the plan before enabling
the scheduler.

`--install-deps` supports `apt`, `dnf`/`yum`, `apk`, and `pacman`. It deliberately
does not install ZFS because ZFS packaging and kernel-module choices are
distribution-specific. Install OpenZFS first and ensure both `zfs` and `zpool` are
on root's `PATH`.

- Ubuntu, Debian, Fedora, RHEL-family distributions, CentOS, Rocky Linux, and
  Arch normally select systemd and its 15-minute timer.
- Alpine normally selects OpenRC and `/etc/periodic/15min`.
- `--init systemd`, `--init openrc`, or `--init none` overrides detection.
- `--no-enable` installs without activating a recurring schedule.

Add `--static` to build with Rust's self-contained musl target. The installer adds
the target with rustup and explicitly supplies the host C compiler for the bundled
TLS crypto, so this path does not depend on distro-specific musl compiler package
names. A static binary is the broadest distribution option because it does not
inherit the builder's glibc version. CI builds and tests in Ubuntu, Debian, Fedora,
CentOS Stream, RHEL UBI, Rocky Linux, Alpine, and Arch containers, plus a
static-musl build.

### systemd

The installer builds a release binary, installs it to `/usr/local/sbin/zsnap`,
installs a dataset-neutral starter at `/etc/zsnap/zsnap.toml` only when that file
does not already exist, and installs the systemd units. An existing configuration
is always preserved. It enables the 15-minute timer only when a pre-existing
configuration's datasets pass a read-only ZFS probe; a fresh starter must first be
edited and reviewed:

```console
./install.sh
```

Use `./install.sh --no-enable` to install without starting the timer. Review the
configuration and preview the live plan before enabling it:

```console
sudoedit /etc/zsnap/zsnap.toml
sudo zfs list -H -o name -t filesystem,volume
sudo zsnap check --probe
sudo zsnap plan
sudo make enable
systemctl list-timers zsnap.timer
```

`./install.sh --static` builds and installs a musl-linked static binary and asks
rustup to add the current architecture's musl target.

The equivalent Make targets are:

```console
make release test lint
make verify-static
sudo make install
sudo make enable
```

For Alpine/OpenRC, use `sudo make install-openrc` followed by
`sudo make enable-openrc`. The latter installs the periodic runner and ensures
`crond` starts. `install-none` installs only the binary and configuration.

`PREFIX`, `BINDIR`, `SYSCONFDIR`, `SYSTEMD_UNIT_DIR`, `OPENRC_INIT_DIR`,
`PERIODIC_DIR`, `CONFIG_SOURCE`, and packaging `DESTDIR` are overridable. The
supplied scheduler files assume the default `/usr/local/sbin` and `/etc` locations. `make uninstall`
handles systemd; `make uninstall-openrc` handles OpenRC. Both deliberately preserve
the user-edited configuration, webhook environment file, and reporting cache.

## CI and releases

Every push and pull request runs formatting, Clippy with warnings denied, the full
test suite, a verified static-musl build, and source builds inside Ubuntu, Debian,
Fedora, CentOS Stream, RHEL UBI, Rocky Linux, Alpine, and Arch containers.

Pushing an annotated `vMAJOR.MINOR.PATCH` tag runs the tests again and creates a
GitHub release for x86-64 and ARM64. Each architecture is compiled and smoke-tested
on a matching native GitHub runner. The release contains a portable static archive,
Debian package, RPM package, Alpine package, Arch Linux package, and an individual
SHA-256 file for every artifact. The tag must exactly match the package version in
`Cargo.toml`:

```console
git tag -a v0.1.0 -m "zsnap 0.1.0"
git push origin v0.1.0
```

The release archives contain the executable, example configuration, systemd and
OpenRC scheduling files, README, and license. A downloaded binary can be installed
directly; ZFS command-line tools are still required at runtime:

```console
sha256sum --check zsnap-0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256
tar -xzf zsnap-0.1.0-x86_64-unknown-linux-musl.tar.gz
sudo install -m755 zsnap-0.1.0-x86_64-unknown-linux-musl/zsnap /usr/local/sbin/zsnap
```

Native packages install the binary as `/usr/bin/zsnap`, preserve existing files in
`/etc/zsnap`, and automatically activate the 15-minute systemd timer or Alpine
periodic job. The dataset-neutral starter cannot create or prune anything until
real dataset sections are added. Install the appropriate downloaded package:

```console
sudo apt install ./zsnap_0.1.0_amd64.deb
sudo dnf install ./zsnap-0.1.0-1.x86_64.rpm
sudo apk add --allow-untrusted ./zsnap-0.1.0-x86_64.apk
sudo pacman -U ./zsnap-0.1.0-1-x86_64.pkg.tar.zst
```

The APK is an unsigned standalone release artifact, hence `--allow-untrusted`.
Configure `/etc/zsnap/zsnap.toml`, run `zsnap check --probe`, and review `zsnap plan`
before the first successful scheduled run. Disable automatic scheduling with
`systemctl disable --now zsnap.timer`; on Alpine, remove
`/etc/periodic/15min/zsnap`.

To build all four native package formats locally around the static binary, install
`nFPM` or use the same pinned, checksum-verified helper as CI:

```console
./ci/install-nfpm.sh 2.47.0 x86_64 /tmp/zsnap-nfpm
make packages NFPM=/tmp/zsnap-nfpm/nfpm
```

## Configure

See [`config.example.toml`](config.example.toml) for a fully annotated example.
Its illustrative dataset stanzas are all commented out, so the file cannot manage
anything until an administrator explicitly configures at least one pool or dataset.
Parsing is typed and strict: unknown keys in known sections, unknown webhook
kinds/events, invalid schedules, duplicate webhook names, and unsafe URL choices
are rejected. `zsnap check` validates syntax and semantics without requiring ZFS.
Dataset sections use `[pool]` or `[pool/dataset]`, and template sections use
`[template_name]`. Simple template references can likewise be bare:
`use_templates = [production, archive]`. Quote a template reference if its name
contains other characters. All other tables and values follow TOML. The older
`[datasets."pool/dataset"]` and `[templates.name]` forms, plus quoted template
references, remain accepted for compatibility.

Because a one-component header such as `[tank]` is a valid pool root, any otherwise
unknown top-level header is interpreted as a dataset. Use `check --probe` to catch a
misspelled root pool. A pool named like a reserved configuration table or beginning
with `template_` can still be written with the legacy `[datasets."pool"]` form.
The core shape is:

```toml
version = 1

[settings]
snapshot_prefix = "autosnap"
timezone = "local"
max_parallel_pools = 0
cache_file = "/etc/zsnap/zsnap.cache"

[template_production]
autosnap = true
autoprune = true
frequently = 0
hourly = 36
daily = 30
weekly = 4
monthly = 3
yearly = 0

[tank/data]
use_templates = [production]
recursive = true

[tank/data/vm]
hourly = 12
```

Templates listed in `use_templates` are applied left to right; later values win.
An explicit child starts from its inherited recursive policy, then applies its
templates and policy values written directly in that dataset section.

### Notifications

Notifications are optional. Webhooks default to failure-only; add `"success"`
when routine success messages are wanted. Flock and Slack receive their documented
`text` payload, while Discord receives `content` with mention parsing disabled.

```toml
[notifications]
max_parallel = 4
timeout_seconds = 10
max_attempts = 3
retry_backoff_milliseconds = 500
fail_on_error = false

[[notifications.webhooks]]
name = "storage-flock"
kind = "flock"
url_env = "ZSNAP_FLOCK_WEBHOOK"
events = ["failure"]

[[notifications.webhooks]]
name = "storage-discord"
kind = "discord"
url = "https://discord.com/api/webhooks/REPLACE/REPLACE"
events = ["failure", "success"]

[[notifications.webhooks]]
name = "storage-slack"
kind = "slack"
url = "https://hooks.slack.com/services/REPLACE/REPLACE/REPLACE"
events = ["failure"]
```

Each webhook is independent. `max_parallel` bounds simultaneous requests;
timeouts are per attempt; and transient network errors, HTTP 408/425/429, and 5xx
responses are retried with exponential backoff capped at 30 seconds. Delivery
failures go to stderr but do not mask the ZFS result unless `fail_on_error = true`.

Webhook URLs are bearer secrets. The installer creates the TOML configuration and
`/etc/zsnap/webhooks.env` with mode `0600`. Direct `url` values are supported, but
`url_env` keeps them out of the main configuration. Put simple, quoted assignments
in the environment file:

```sh
ZSNAP_FLOCK_WEBHOOK='https://api.flock.com/hooks/sendMessage/REPLACE'
```

zsnap automatically reads `webhooks.env` next to the selected TOML configuration,
so direct commands and scheduled runs resolve the same secrets. Values already in
the process environment take precedence, allowing one-off overrides. The systemd
unit and OpenRC/Alpine runners also supply the installed file for compatibility.
Webhook URLs must use HTTPS. Errors and debug representations redact configured
URLs.

Test every enabled endpoint through the real delivery path without touching ZFS:

```console
sudo zsnap notify-test --message "storage notifications configured"
sudo zsnap --json notify-test
```

Provider setup details are in the official
[Flock](https://support.flock.com/hc/en-us/articles/360006943354-Incoming-webhooks),
[Discord](https://docs.discord.com/developers/resources/webhook), and
[Slack](https://api.slack.com/messaging/webhooks) incoming-webhook documentation.

### Recursion modes

- `recursive = false` manages only that dataset.
- `recursive = true` expands the policy to every current descendant and snapshots
  each one individually. Explicit child sections are allowed.
- `recursive = "zfs"` takes consistent tree-wide snapshots with `zfs snapshot -r`.
  Explicit child sections beneath that root are rejected because they would imply
  snapshot exclusions ZFS cannot honor atomically.
- `process_children_only = true` is supported with `recursive = true` and leaves
  the named parent unchanged.

### Schedules and retention

Schedules and snapshot-name timestamps use the host timezone by default. Set
`settings.timezone = "utc"` to use UTC instead; `"local"` and `"host"` select the
host timezone. Defaults intentionally track Sanoid's general policy: hourly at
minute 0, daily at 23:59, weekly Monday at 23:30, monthly on day 1, and yearly on
January 1. Days of the week use ISO numbering (`1 = Monday`, `7 = Sunday`). Monthly
days are limited to 1 through 28 so every configured date exists.

During a repeated daylight-saving hour, both real occurrences can become due and
the later occurrence receives Sanoid's `dst` snapshot-name suffix to avoid a name
collision. A configured civil time skipped by a forward clock transition is moved
forward by the size of that gap. Retention ages always use real elapsed time.

Setting a retention class to `0` disables new snapshots of that class and prunes
all owned snapshots in that class when `autoprune = true`. `prune_defer = 70`
defers pruning while pool capacity is below 70 percent.

### Hooks

Hooks are optional arrays containing an executable and arguments:

```toml
[tank/database]
pre_snapshot_script = ["/usr/local/libexec/zsnap/db-freeze", "--timeout", "5"]
post_snapshot_script = ["/usr/local/libexec/zsnap/db-thaw"]
script_timeout = 10
no_inconsistent_snapshot = true
force_post_snapshot_script = true
```

The hook environment contains `ZSNAP_SCRIPT`, `ZSNAP_TARGET`, `ZSNAP_TARGETS`,
`ZSNAP_SNAPNAME`, `ZSNAP_SNAPNAMES`, `ZSNAP_TYPES`, and `ZSNAP_PRE_FAILURE`.
Equivalent `SANOID_*` variables are also set to ease hook migration. Set a hook to
`["/bin/sh", "-c", "..."]` only when shell behavior is explicitly needed.

## Commands

```console
# Convert Sanoid policy into a new validated mode-0600 file; never overwrites.
zsnap migrate-sanoid --input /etc/sanoid/sanoid.conf \
  --output ./zsnap.migrated.toml

# Offline syntax and policy validation; does not require ZFS.
zsnap --config /etc/zsnap/zsnap.toml check

# Validate dataset names and recursive expansion against this host.
sudo zsnap check --probe

# Print all due creates/deletes. Add --json for structured output.
sudo zsnap plan

# Read the fast reporting cache; `info` is an alias. Add -v for pool/dataset detail.
sudo zsnap status
sudo zsnap status -v
sudo zsnap info

# Force a live inventory scan and atomically replace the cache.
sudo zsnap status --refresh

# Exercise the exact executor path without mutation or hooks.
sudo zsnap run --dry-run --verbose

# Run both phases, or isolate one phase.
sudo zsnap run
sudo zsnap snapshot
sudo zsnap prune

# Exercise all enabled notification targets without querying ZFS.
sudo zsnap notify-test
```

Verbose snapshot, prune, and combined runs report snapshot time, pruning time,
and total wall-clock time for each pool, followed by the overall core-run time.
Pool timings are intentionally kept separate because independent pools execute in
parallel and their durations overlap. Dry-run timings measure discovery and command
preparation, not ZFS mutation speed.

### Cached status

`zsnap status` normally reads `/etc/zsnap/zsnap.cache` without invoking `zfs` or
`zpool`. Its compact output reports configured zpools, recursively discovered
datasets, total snapshots, and the subset carrying `org.zsnap:managed=yes`.
`--verbose` adds counts for every pool and dataset; `--json` emits the same cached
data as structured output. `zsnap info` is an alias for `zsnap status`.

Every successful non-dry-run `run`, `snapshot`, or `prune` updates the cache while
holding the normal run lock. This means the installed systemd timer and the
OpenRC/cron runner refresh it automatically without performing a second inventory
scan. A failed operation leaves the previous cache intact. If the cache does not
exist, `status` creates it from a live scan; `status --refresh` always forces a live
scan. Writes use an adjacent mode-0600 temporary file, synchronization, and atomic
rename so readers never observe a partial document.

The cache is deliberately reporting-only and scoped to configured roots plus their
descendants. It stores counts, not a snapshot payload, and is never read by the
planner or pruner. Live ZFS state remains the authority for every mutation.

`check` is the configuration linter. It validates TOML values and the dataset-header
shorthand, rejects unknown keys, and verifies value bounds, template references,
recursion combinations, dataset names, hooks, and webhook settings. Every
operational command loads the file through the same validator before it can query
or mutate ZFS, so an invalid file cannot reach snapshot or prune execution. Add
`--probe` only when the linter should also verify datasets and recursive expansion
against the current host.

## Migrating from Sanoid

Create a new zsnap configuration without changing the Sanoid source file:

```console
zsnap migrate-sanoid \
  --input /etc/sanoid/sanoid.conf \
  --output ./zsnap.migrated.toml
zsnap --config ./zsnap.migrated.toml check
sudo zsnap --config ./zsnap.migrated.toml check --probe
sudo zsnap --config ./zsnap.migrated.toml plan
```

The converter automatically reads `sanoid.defaults.conf` beside the input when it
exists; use `--defaults /path/to/sanoid.defaults.conf` to select another copy.
Omitting `--output` prints the generated TOML for inspection. With `--output`, the
file is created mode 0600 and an existing path is always rejected. The generated
TOML is passed through the same parser and semantic validator used by normal zsnap
runs before it is written. Migration never invokes `zfs` or `zpool`, modifies the
source, disables Sanoid, or installs/enables a service.

Retention, schedules, ordered templates, dataset overrides, `path`, recursion,
`process_children_only`, prune deferral, and hooks are converted. Sanoid hooks are
preserved through explicit `["/bin/sh", "-c", "command"]` argv and produce a review
warning. Migration writes `timezone = "local"`, so Sanoid's configured civil
schedule times and local snapshot-name timestamps are retained. Monitoring-only
keys are reported and omitted because monitoring is outside zsnap's scope. A
setting that cannot be represented without changing snapshot coverage, such as
`skip_children = yes`, stops conversion rather than silently emitting a lossy
policy.

The generated `sanoid_defaults` template is the baseline Sanoid normally applies
implicitly: its built-in defaults, overlaid by `[template_default]` from the
supplied `sanoid.defaults.conf` and the source configuration. The converter applies
it first to every non-inherited dataset; recursive children receive that baseline
through their parent. Making it explicit prevents partially specified Sanoid
policies from changing behavior during migration.

Keep `snapshot_prefix = "autosnap"`. Existing Sanoid snapshots will suppress
unnecessary duplicate snapshots, but `zsnap` will never prune them because they
lack `org.zsnap:managed=yes`. After reviewing the converted file and plan, disable
Sanoid before enabling the zsnap timer. Remove old Sanoid snapshots manually under
your existing migration policy when they are no longer needed.

## Project scope and provenance

This is an independent Rust implementation informed by Sanoid's public behavior
and documentation. It does not contain Sanoid's Perl source. Snapshot replication,
Nagios-compatible health checks, bookmarks, holds, and send/receive orchestration
are not part of version 0.1.0.

Licensed under MIT. See [`LICENSE`](LICENSE).
