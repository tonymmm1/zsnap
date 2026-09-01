#!/usr/bin/env bash
# Compare Sanoid and zsnap on uniquely named, disposable sparse-file zpools.

set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd -- "$SCRIPT_DIR/.." && pwd -P)

ZSNAP_BIN=${ZSNAP_BIN:-$REPO_DIR/target/release/zsnap}
SANOID_STABLE_BIN=${SANOID_STABLE_BIN:-${SANOID_BIN:-sanoid}}
SANOID_STABLE_DEFAULTS=${SANOID_STABLE_DEFAULTS:-${SANOID_DEFAULTS:-}}
SANOID_DEVELOPMENT_BIN=${SANOID_DEVELOPMENT_BIN:-}
SANOID_DEVELOPMENT_DEFAULTS=${SANOID_DEVELOPMENT_DEFAULTS:-}
SANOID_DEVELOPMENT_REVISION=${SANOID_DEVELOPMENT_REVISION:-}
SANOID_PERL5LIB=${SANOID_PERL5LIB:-}
TRIALS=${BENCHMARK_TRIALS:-5}
POOL_COUNT=${BENCHMARK_POOLS:-3}
BRANCH_COUNT=${BENCHMARK_BRANCHES:-3}
LEAF_COUNT=${BENCHMARK_LEAVES:-3}
PRUNE_SNAPSHOTS=${BENCHMARK_PRUNE_SNAPSHOTS:-6}
SPARSE_SIZE=${BENCHMARK_SPARSE_SIZE:-512M}
KEEP_WORKDIR=${BENCHMARK_KEEP_WORKDIR:-0}
OWNER=${BENCHMARK_OWNER:-}
AS_ROOT=${BENCHMARK_AS_ROOT:-0}
REPORT_MD=$SCRIPT_DIR/results.md
REPORT_HTML=$SCRIPT_DIR/results.html
REPORT_TSV=$SCRIPT_DIR/results.tsv

usage() {
    cat <<'EOF'
Usage: benchmarks/zfs-benchmark.sh [options]

  --trials N             measured trials per scenario (default: 5)
  --pools N              disposable pools, at least 2 (default: 3)
  --branches N           branch datasets per pool (default: 3)
  --leaves N             leaves per branch (default: 3)
  --prune-snapshots N    expired snapshots per dataset (default: 6)
  --sparse-size SIZE     sparse backing file per pool (default: 512M)
  --keep-workdir         retain generated configs/logs, but destroy pools
  -h, --help             show help

Set SANOID_STABLE_BIN and SANOID_DEVELOPMENT_BIN to compare an installed stable
release with a separate development checkout. Sanoid source is never copied into
this repository. SANOID_BIN remains a backwards-compatible stable alias.
EOF
}

while (($#)); do
    case $1 in
        --trials) TRIALS=$2; shift 2 ;;
        --pools) POOL_COUNT=$2; shift 2 ;;
        --branches) BRANCH_COUNT=$2; shift 2 ;;
        --leaves) LEAF_COUNT=$2; shift 2 ;;
        --prune-snapshots) PRUNE_SNAPSHOTS=$2; shift 2 ;;
        --sparse-size) SPARSE_SIZE=$2; shift 2 ;;
        --keep-workdir) KEEP_WORKDIR=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

die() { printf 'benchmark error: %s\n' "$*" >&2; exit 1; }

require_uint() {
    local name=$1 value=$2 minimum=$3 maximum=$4
    [[ $value =~ ^[0-9]+$ ]] || die "$name must be an integer"
    ((value >= minimum && value <= maximum)) ||
        die "$name must be between $minimum and $maximum"
}

resolve_executable() {
    local value=$1 resolved
    if [[ $value == */* ]]; then
        [[ -x $value ]] || die "executable is not runnable: $value"
        (cd -- "$(dirname -- "$value")" && printf '%s/%s\n' "$PWD" "$(basename -- "$value")")
    else
        resolved=$(command -v -- "$value" 2>/dev/null) || die "cannot find '$value' in PATH"
        printf '%s\n' "$resolved"
    fi
}

resolve_sanoid_defaults() {
    local binary=$1 requested=$2 label=$3 allow_system=$4 candidate
    if [[ -n $requested ]]; then
        candidate=$requested
    elif [[ -r $(dirname -- "$binary")/sanoid.defaults.conf ]]; then
        candidate=$(dirname -- "$binary")/sanoid.defaults.conf
    elif [[ $allow_system == 1 && -r /etc/sanoid/sanoid.defaults.conf ]]; then
        candidate=/etc/sanoid/sanoid.defaults.conf
    else
        die "cannot find defaults for $label; set its *_DEFAULTS variable"
    fi
    [[ -r $candidate ]] || die "cannot read $label defaults: $candidate"
    (cd -- "$(dirname -- "$candidate")" && printf '%s/%s\n' "$PWD" "$(basename -- "$candidate")")
}

sanoid_version() {
    local binary=$1 value
    if [[ -n $SANOID_PERL5LIB ]]; then value=$(PERL5LIB=$SANOID_PERL5LIB "$binary" --version 2>&1 | head -n 1)
    else value=$("$binary" --version 2>&1 | head -n 1); fi
    printf '%s\n' "${value##*/}"
}

require_uint trials "$TRIALS" 1 50
require_uint pools "$POOL_COUNT" 2 16
require_uint branches "$BRANCH_COUNT" 1 20
require_uint leaves "$LEAF_COUNT" 1 50
require_uint prune-snapshots "$PRUNE_SNAPSHOTS" 1 30
[[ $SPARSE_SIZE =~ ^[1-9][0-9]*[KMGTP]?$ ]] || die "sparse-size must look like 512M or 2G"
[[ $KEEP_WORKDIR == 0 || $KEEP_WORKDIR == 1 ]] || die "BENCHMARK_KEEP_WORKDIR must be 0 or 1"

ZSNAP_BIN=$(resolve_executable "$ZSNAP_BIN")
SANOID_STABLE_BIN=$(resolve_executable "$SANOID_STABLE_BIN")
[[ -n $SANOID_DEVELOPMENT_BIN ]] || die "set SANOID_DEVELOPMENT_BIN to a separate development checkout"
SANOID_DEVELOPMENT_BIN=$(resolve_executable "$SANOID_DEVELOPMENT_BIN")
SANOID_STABLE_DEFAULTS=$(resolve_sanoid_defaults "$SANOID_STABLE_BIN" "$SANOID_STABLE_DEFAULTS" "Sanoid stable" 1)
SANOID_DEVELOPMENT_DEFAULTS=$(resolve_sanoid_defaults "$SANOID_DEVELOPMENT_BIN" "$SANOID_DEVELOPMENT_DEFAULTS" "Sanoid development" 0)

if ((EUID != 0)); then
    command -v sudo >/dev/null || die "sudo is required to create disposable zpools"
    printf 'Requesting root only for disposable ZFS pool creation and mutation...\n'
    exec sudo -- env \
        ZSNAP_BIN="$ZSNAP_BIN" \
        SANOID_STABLE_BIN="$SANOID_STABLE_BIN" \
        SANOID_STABLE_DEFAULTS="$SANOID_STABLE_DEFAULTS" \
        SANOID_DEVELOPMENT_BIN="$SANOID_DEVELOPMENT_BIN" \
        SANOID_DEVELOPMENT_DEFAULTS="$SANOID_DEVELOPMENT_DEFAULTS" \
        SANOID_DEVELOPMENT_REVISION="$SANOID_DEVELOPMENT_REVISION" \
        SANOID_PERL5LIB="$SANOID_PERL5LIB" \
        BENCHMARK_TRIALS="$TRIALS" \
        BENCHMARK_POOLS="$POOL_COUNT" \
        BENCHMARK_BRANCHES="$BRANCH_COUNT" \
        BENCHMARK_LEAVES="$LEAF_COUNT" \
        BENCHMARK_PRUNE_SNAPSHOTS="$PRUNE_SNAPSHOTS" \
        BENCHMARK_SPARSE_SIZE="$SPARSE_SIZE" \
        BENCHMARK_KEEP_WORKDIR="$KEEP_WORKDIR" \
        BENCHMARK_OWNER="$(id -u):$(id -g)" \
        BENCHMARK_AS_ROOT=1 \
        "$0"
fi
((EUID == 0)) || die "root privileges are required"
[[ $AS_ROOT == 0 || $AS_ROOT == 1 ]] || die "invalid internal privilege state"

ZFS_BIN=$(resolve_executable zfs)
ZPOOL_BIN=$(resolve_executable zpool)
TRUNCATE_BIN=$(resolve_executable truncate)
for utility in awk date df git grep hostname install mktemp paste sed sort; do
    command -v "$utility" >/dev/null || die "required utility is missing: $utility"
done
[[ $(date +%s%N) =~ ^[0-9]{15,}$ ]] || die "GNU date with nanosecond timestamps is required"
if [[ -n $SANOID_PERL5LIB ]]; then
    [[ -d $SANOID_PERL5LIB ]] || die "SANOID_PERL5LIB does not exist: $SANOID_PERL5LIB"
    PERL5LIB=$SANOID_PERL5LIB perl -MConfig::IniFiles -MCapture::Tiny -e 1 ||
        die "Sanoid Perl modules are not loadable from $SANOID_PERL5LIB"
else
    perl -MConfig::IniFiles -MCapture::Tiny -e 1 ||
        die "Sanoid needs Config::IniFiles and Capture::Tiny; see benchmarks/README.md"
fi

WORK_DIR=$(mktemp -d /var/tmp/zsnap-benchmark.XXXXXX)
RUN_TOKEN="$(date +%s)_$$"
RAW_TSV=$WORK_DIR/results.tsv
SUMMARY_TSV=$WORK_DIR/summary.tsv
SUCCESS=0
declare -a POOLS=() ROOT_DATASETS=() ALL_DATASETS=()

is_benchmark_pool_name() { [[ $1 =~ ^zsnapbench_[0-9]+_[0-9]+_p[0-9]+$ ]]; }

cleanup() {
    local exit_code=$? pool
    trap - EXIT INT TERM
    set +e
    for pool in "${POOLS[@]}"; do
        if ! is_benchmark_pool_name "$pool"; then
            printf 'SAFETY: refusing to destroy unexpected pool %q\n' "$pool" >&2
            continue
        fi
        if [[ $($ZPOOL_BIN list -H -o name "$pool" 2>/dev/null) == "$pool" ]]; then
            "$ZPOOL_BIN" destroy -f "$pool" || printf 'warning: could not destroy %s\n' "$pool" >&2
        fi
    done
    if [[ $KEEP_WORKDIR == 1 ]]; then
        [[ -z $OWNER ]] || chown -R "$OWNER" "$WORK_DIR" 2>/dev/null
        printf 'Retained work directory: %s\n' "$WORK_DIR"
    elif [[ $WORK_DIR == /var/tmp/zsnap-benchmark.* && -d $WORK_DIR ]]; then
        rm -rf -- "$WORK_DIR"
    else
        printf 'SAFETY: refusing to remove unexpected path %q\n' "$WORK_DIR" >&2
    fi
    ((SUCCESS == 0)) || printf 'Destroyed all disposable benchmark pools.\n'
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

scenario_ids=(
    sanoid-stable
    sanoid-development
    zsnap-serial-unbatched
    zsnap-serial-batched
    zsnap-two-batched
    zsnap-auto-unbatched
    zsnap-auto-small-batches
    zsnap-auto-defaults
)
scenario_labels=(
    "Sanoid stable"
    "Sanoid development"
    "zsnap serial, unbatched"
    "zsnap serial, default batches"
    "zsnap 2 pools, default batches"
    "zsnap auto pools, unbatched"
    "zsnap auto pools, small batches"
    "zsnap auto pools, defaults"
)
scenario_tools=(sanoid_stable sanoid_development zsnap zsnap zsnap zsnap zsnap zsnap)
scenario_max=(- - 1 1 2 0 0 0)
scenario_snap_batch=(- - 1 128 128 1 4 128)
scenario_prune_batch=(- - 1 64 64 1 3 64)

write_zsnap_config() {
    local phase=$1 index=$2 autosnap=false autoprune=true hourly=0 root
    [[ $phase == snapshot ]] && autosnap=true && autoprune=false && hourly=1
    {
        printf 'version = 1\n\n[settings]\n'
        printf 'snapshot_prefix = "autosnap"\n'
        printf 'max_parallel_pools = %s\n' "${scenario_max[$index]}"
        printf 'snapshot_batch_size = %s\n' "${scenario_snap_batch[$index]}"
        printf 'prune_batch_size = %s\n' "${scenario_prune_batch[$index]}"
        printf 'lock_file = "%s/zsnap.lock"\n' "$WORK_DIR"
        printf 'cache_file = "%s/zsnap.cache"\n' "$WORK_DIR"
        printf 'zfs_command = "%s"\nzpool_command = "%s"\n\n' "$ZFS_BIN" "$ZPOOL_BIN"
        printf '[notifications]\nenabled = false\n\n[templates.benchmark]\n'
        printf 'autosnap = %s\nautoprune = %s\n' "$autosnap" "$autoprune"
        printf 'frequently = 0\nhourly = %s\ndaily = 0\nweekly = 0\nmonthly = 0\nyearly = 0\nprune_defer = 0\n\n' "$hourly"
        for root in "${ROOT_DATASETS[@]}"; do
            printf '[datasets."%s"]\nuse_templates = ["benchmark"]\nrecursive = true\n\n' "$root"
        done
    } >"$WORK_DIR/zsnap-$phase-${scenario_ids[$index]}.toml"
}

write_sanoid_config() {
    local phase=$1 variant=$2 defaults=$3
    local dir=$WORK_DIR/sanoid-$variant-$phase autosnap=no autoprune=yes hourly=0 root
    [[ $phase == snapshot ]] && autosnap=yes && autoprune=no && hourly=1
    install -d -m 755 "$dir"
    install -m 644 "$defaults" "$dir/sanoid.defaults.conf"
    {
        for root in "${ROOT_DATASETS[@]}"; do
            printf '[%s]\nuse_template = benchmark\nrecursive = yes\n\n' "$root"
        done
        printf '[template_benchmark]\nautosnap = %s\nautoprune = %s\nmonitor = no\n' "$autosnap" "$autoprune"
        printf 'frequently = 0\nhourly = %s\ndaily = 0\nweekly = 0\nmonthly = 0\nyearly = 0\nprune_defer = 0\n' "$hourly"
    } >"$dir/sanoid.conf"
}

run_sanoid() {
    local phase=$1 variant=$2 log=$3 action=--take-snapshots binary
    [[ $phase == prune ]] && action=--prune-snapshots
    case $variant in
        stable) binary=$SANOID_STABLE_BIN ;;
        development) binary=$SANOID_DEVELOPMENT_BIN ;;
        *) die "unknown Sanoid variant: $variant" ;;
    esac
    local -a command=("$binary" "--configdir=$WORK_DIR/sanoid-$variant-$phase"
        "--cache-dir=$WORK_DIR/sanoid-cache-$variant-$phase" "--run-dir=$WORK_DIR/sanoid-run-$variant-$phase"
        --force-update --quiet "$action")
    if [[ -n $SANOID_PERL5LIB ]]; then
        TZ=UTC PERL5LIB=$SANOID_PERL5LIB "${command[@]}" >"$log" 2>&1
    else
        TZ=UTC "${command[@]}" >"$log" 2>&1
    fi
}

run_variant() {
    local phase=$1 index=$2 log=$3
    case ${scenario_tools[$index]} in
        sanoid_stable) run_sanoid "$phase" stable "$log" ;;
        sanoid_development) run_sanoid "$phase" development "$log" ;;
        zsnap) "$ZSNAP_BIN" --config "$WORK_DIR/zsnap-$phase-${scenario_ids[$index]}.toml" "$phase" >"$log" 2>&1 ;;
        *) die "unknown benchmark tool: ${scenario_tools[$index]}" ;;
    esac
}

snapshot_count() {
    local root value count=0
    for root in "${ROOT_DATASETS[@]}"; do
        value=$($ZFS_BIN list -H -r -t snapshot -o name "$root" 2>/dev/null |
            awk 'NF { n++ } END { print n + 0 }')
        ((count += value))
    done
    printf '%s\n' "$count"
}

clear_snapshots() {
    local root name full
    local -a names leftovers
    for root in "${ROOT_DATASETS[@]}"; do
        mapfile -t names < <($ZFS_BIN list -H -r -t snapshot -o name "$root" 2>/dev/null |
            awk -F@ -v root="$root" '$1 == root { print $2 }')
        for name in "${names[@]}"; do
            [[ -z $name ]] || "$ZFS_BIN" destroy -r "$root@$name"
        done
        mapfile -t leftovers < <($ZFS_BIN list -H -r -t snapshot -o name "$root" 2>/dev/null || true)
        for full in "${leftovers[@]}"; do
            [[ -z $full ]] || "$ZFS_BIN" destroy "$full"
        done
    done
    [[ $(snapshot_count) == 0 ]] || die "snapshot cleanup did not reach zero"
}

seed_prune_snapshots() {
    local tool=$1 index second pool root dataset target
    local -a targets
    for ((index = 1; index <= PRUNE_SNAPSHOTS; index++)); do
        printf -v second '%02d' "$index"
        for pool_index in "${!POOLS[@]}"; do
            pool=${POOLS[$pool_index]}
            root=${ROOT_DATASETS[$pool_index]}
            targets=()
            for dataset in "${ALL_DATASETS[@]}"; do
                [[ $dataset == "$pool"/* ]] || continue
                target="$dataset@autosnap_2000-01-01_00:00:${second}_hourly"
                targets+=("$target")
            done
            if [[ $tool == zsnap ]]; then
                "$ZFS_BIN" snapshot -o org.zsnap:managed=yes "${targets[@]}"
            else
                "$ZFS_BIN" snapshot "${targets[@]}"
            fi
        done
    done
    local expected=$(( ${#ALL_DATASETS[@]} * PRUNE_SNAPSHOTS )) seed_second
    [[ $(snapshot_count) == "$expected" ]] || die "prune seed count mismatch"
    seed_second=$(date +%s)
    while (($(date +%s) <= seed_second)); do sleep 0.05; done
}

prepare_variant() {
    local phase=$1 index=$2
    clear_snapshots
    [[ $phase != prune ]] || seed_prune_snapshots "${scenario_tools[$index]}"
}

verify_variant() {
    local phase=$1 index=$2 count expected=${#ALL_DATASETS[@]}
    count=$(snapshot_count)
    if [[ $phase == snapshot && $count != "$expected" ]]; then
        sed -n '1,240p' "$WORK_DIR/last-command.log" >&2
        die "${scenario_labels[$index]} created $count snapshots; expected $expected"
    elif [[ $phase == prune && $count != 0 ]]; then
        sed -n '1,240p' "$WORK_DIR/last-command.log" >&2
        die "${scenario_labels[$index]} left $count snapshots after prune"
    fi
}

timed_variant() {
    local phase=$1 index=$2 trial=$3 start end elapsed ms
    prepare_variant "$phase" "$index"
    start=$(date +%s%N)
    if ! run_variant "$phase" "$index" "$WORK_DIR/last-command.log"; then
        sed -n '1,260p' "$WORK_DIR/last-command.log" >&2
        die "${scenario_labels[$index]} $phase command failed"
    fi
    end=$(date +%s%N)
    elapsed=$((end - start))
    ms=$(awk -v ns="$elapsed" 'BEGIN { printf "%.3f", ns / 1000000 }')
    verify_variant "$phase" "$index"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$phase" "${scenario_ids[$index]}" \
        "${scenario_tools[$index]}" "${scenario_max[$index]}" "${scenario_snap_batch[$index]}" \
        "${scenario_prune_batch[$index]}" "$trial" "$ms" "${scenario_labels[$index]}" >>"$RAW_TSV"
    printf '%9s ms\n' "$ms"
}

measure_inventory_command() {
    local id=$1 label=$2
    local trial start end elapsed ms
    shift 2
    for ((trial = 1; trial <= TRIALS; trial++)); do
        printf '  trial %d/%d  %-38s' "$trial" "$TRIALS" "$label"
        start=$(date +%s%N)
        if ! "$@" >"$WORK_DIR/last-command.log" 2>&1; then
            sed -n '1,260p' "$WORK_DIR/last-command.log" >&2
            die "$label command failed"
        fi
        end=$(date +%s%N)
        elapsed=$((end - start))
        ms=$(awk -v ns="$elapsed" 'BEGIN { printf "%.3f", ns / 1000000 }')
        printf 'inventory\t%s\tzfs\t-\t-\t-\t%s\t%s\t%s\n' \
            "$id" "$trial" "$ms" "$label" >>"$RAW_TSV"
        printf '%9s ms\n' "$ms"
    done
}

measure_prune_plan() {
    local config=$WORK_DIR/zsnap-prune-zsnap-auto-defaults.toml
    local expected=$(( ${#ALL_DATASETS[@]} * PRUNE_SNAPSHOTS ))
    local trial start end elapsed ms

    printf '\nDiagnostic: warm prune planning only\n'
    clear_snapshots
    seed_prune_snapshots zsnap
    if ! "$ZSNAP_BIN" --config "$config" plan --scope prune >"$WORK_DIR/last-command.log" 2>&1; then
        sed -n '1,260p' "$WORK_DIR/last-command.log" >&2
        die "prune plan warm-up failed"
    fi
    if ! grep -Fq "plan: create 0 snapshot(s), prune $expected snapshot(s), $POOL_COUNT pool(s)" "$WORK_DIR/last-command.log"; then
        sed -n '1,80p' "$WORK_DIR/last-command.log" >&2
        die "prune plan count mismatch"
    fi
    printf '  warm-up                               ok\n'
    for ((trial = 1; trial <= TRIALS; trial++)); do
        printf '  trial %d/%d  %-38s' "$trial" "$TRIALS" "zsnap prune plan only"
        start=$(date +%s%N)
        if ! "$ZSNAP_BIN" --config "$config" plan --scope prune >"$WORK_DIR/last-command.log" 2>&1; then
            sed -n '1,260p' "$WORK_DIR/last-command.log" >&2
            die "prune plan command failed"
        fi
        end=$(date +%s%N)
        elapsed=$((end - start))
        ms=$(awk -v ns="$elapsed" 'BEGIN { printf "%.3f", ns / 1000000 }')
        printf 'plan\tzsnap-prune-plan\tzsnap\t0\t128\t64\t%s\t%s\tzsnap prune plan only\n' \
            "$trial" "$ms" >>"$RAW_TSV"
        printf '%9s ms\n' "$ms"
    done
    printf '\nDiagnostic: warm inventory command components\n'
    measure_inventory_command dataset-list "configured-root dataset list" \
        "$ZFS_BIN" list -H -p -r -t filesystem,volume -o name "${ROOT_DATASETS[@]}"
    measure_inventory_command snapshot-list "managed-root snapshot/property list" \
        "$ZFS_BIN" list -H -p -r -t snapshot -o "name,creation,org.zsnap:managed" "${ROOT_DATASETS[@]}"
    measure_inventory_command pool-list "configured-pool capacity list" \
        "$ZPOOL_BIN" list -H -p -o name,capacity "${POOLS[@]}"
    clear_snapshots
}

mutation_calls() {
    local phase=$1 index=$2 total=${#ALL_DATASETS[@]}
    local per_pool=$((1 + BRANCH_COUNT + BRANCH_COUNT * LEAF_COUNT)) batch
    if [[ ${scenario_tools[$index]} != zsnap ]]; then
        [[ $phase == snapshot ]] && printf '%s\n' "$total" || printf '%s\n' "$((total * PRUNE_SNAPSHOTS))"
    elif [[ $phase == snapshot ]]; then
        batch=${scenario_snap_batch[$index]}
        printf '%s\n' "$((POOL_COUNT * ((per_pool + batch - 1) / batch)))"
    else
        batch=${scenario_prune_batch[$index]}
        printf '%s\n' "$((total * ((PRUNE_SNAPSHOTS + batch - 1) / batch)))"
    fi
}

aggregate_results() {
    local phase index id count middle median mean minimum maximum baseline speed calls
    local -a values
    printf 'phase\tid\tscenario\ttool\tmax_parallel_pools\tsnapshot_batch_size\tprune_batch_size\tmutation_calls\ttrials\tmedian_ms\tmean_ms\tmin_ms\tmax_ms\tspeedup_vs_sanoid_stable\n' >"$SUMMARY_TSV"
    for phase in snapshot prune; do
        mapfile -t values < <(awk -F '\t' -v p="$phase" '$1 == p && $2 == "sanoid-stable" { print $8 }' "$RAW_TSV" | sort -n)
        count=${#values[@]}; middle=$((count / 2))
        if ((count % 2)); then baseline=${values[$middle]};
        else baseline=$(awk -v a="${values[$((middle - 1))]}" -v b="${values[$middle]}" 'BEGIN { printf "%.3f", (a+b)/2 }'); fi
        for index in "${!scenario_ids[@]}"; do
            id=${scenario_ids[$index]}
            mapfile -t values < <(awk -F '\t' -v p="$phase" -v id="$id" '$1 == p && $2 == id { print $8 }' "$RAW_TSV" | sort -n)
            count=${#values[@]}; ((count == TRIALS)) || die "$phase/$id result count mismatch"
            middle=$((count / 2)); minimum=${values[0]}; maximum=${values[$((count - 1))]}
            if ((count % 2)); then median=${values[$middle]};
            else median=$(awk -v a="${values[$((middle - 1))]}" -v b="${values[$middle]}" 'BEGIN { printf "%.3f", (a+b)/2 }'); fi
            mean=$(printf '%s\n' "${values[@]}" | awk '{ sum += $1 } END { printf "%.3f", sum/NR }')
            speed=$(awk -v base="$baseline" -v value="$median" 'BEGIN { printf "%.2f", base/value }')
            calls=$(mutation_calls "$phase" "$index")
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$phase" "$id" "${scenario_labels[$index]}" "${scenario_tools[$index]}" \
                "${scenario_max[$index]}" "${scenario_snap_batch[$index]}" "${scenario_prune_batch[$index]}" \
                "$calls" "$count" "$median" "$mean" "$minimum" "$maximum" "$speed" >>"$SUMMARY_TSV"
        done
    done
}

summary_value() {
    awk -F '\t' -v p="$1" -v id="$2" -v column="$3" '$1 == p && $2 == id { print $column; exit }' "$SUMMARY_TSV"
}
raw_median() {
    local phase=$1 id=$2 count middle
    local -a values
    mapfile -t values < <(awk -F '\t' -v p="$phase" -v id="$id" '$1 == p && $2 == id { print $8 }' "$RAW_TSV" | sort -n)
    count=${#values[@]}
    ((count == TRIALS)) || die "$phase/$id result count mismatch"
    middle=$((count / 2))
    if ((count % 2)); then
        printf '%s\n' "${values[$middle]}"
    else
        awk -v a="${values[$((middle - 1))]}" -v b="${values[$middle]}" 'BEGIN { printf "%.3f", (a+b)/2 }'
    fi
}

ratio() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a/b }'; }
display_max() { [[ $1 == - ]] && printf n/a || { [[ $1 == 0 ]] && printf '0 (auto)' || printf '%s' "$1"; }; }
display_batch() { [[ $1 == - ]] && printf n/a || printf '%s' "$1"; }
html_escape() { sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g'; }

markdown_table() {
    local phase id label tool max snap_batch prune_batch calls trials median mean minimum maximum speed
    printf '| Phase | Scenario | Max pools | Snapshot batch | Prune batch | Mutation calls | Median | Mean | Min–max | vs Sanoid stable |\n'
    printf '| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n'
    while IFS=$'\t' read -r phase id label tool max snap_batch prune_batch calls trials median mean minimum maximum speed; do
        [[ $phase == phase ]] && continue
        printf '| %s | %s | %s | %s | %s | %s | %s ms | %s ms | %s–%s ms | %s× |\n' \
            "$phase" "$label" "$(display_max "$max")" "$(display_batch "$snap_batch")" \
            "$(display_batch "$prune_batch")" "$calls" "$median" "$mean" "$minimum" "$maximum" "$speed"
    done <"$SUMMARY_TSV"
}

html_table() {
    local phase id label tool max snap_batch prune_batch calls trials median mean minimum maximum speed
    printf '<table><thead><tr><th>Phase</th><th>Scenario</th><th>Max pools</th><th>Snapshot batch</th><th>Prune batch</th><th>Mutation calls</th><th>Median</th><th>Mean</th><th>Min–max</th><th>vs Sanoid stable</th></tr></thead><tbody>\n'
    while IFS=$'\t' read -r phase id label tool max snap_batch prune_batch calls trials median mean minimum maximum speed; do
        [[ $phase == phase ]] && continue
        printf '<tr><td>%s</td><td>%s</td><td>%s</td><td>%s</td><td>%s</td><td class="n">%s</td><td class="n">%s ms</td><td class="n">%s ms</td><td class="n">%s–%s ms</td><td class="n">%s×</td></tr>\n' \
            "$phase" "$(printf '%s' "$label" | html_escape)" "$(display_max "$max")" \
            "$(display_batch "$snap_batch")" "$(display_batch "$prune_batch")" "$calls" \
            "$median" "$mean" "$minimum" "$maximum" "$speed"
    done <"$SUMMARY_TSV"
    printf '</tbody></table>\n'
}

render_reports() {
    local generated os kernel zfs_version zsnap_version stable_version development_version development_revision cpu fs commit source_markdown source_html
    local per_pool total seeded csu csb cad cas psu psb pad pas c_batch c_pool p_batch p_pool c_small p_small fastest_c fastest_p stable_create dev_create stable_prune dev_prune dev_create_ratio dev_prune_ratio plan_only prune_execution plan_share dataset_list snapshot_list pool_list plan_residual snapshot_plan_share snapshot_full_share
    generated=$(date -u '+%Y-%m-%d %H:%M:%S UTC')
    os=$(awk -F= '$1 == "PRETTY_NAME" { v=$2; gsub(/^"|"$/, "", v); print v; exit }' /etc/os-release)
    kernel=$(uname -srmo)
    zfs_version=$($ZFS_BIN version 2>&1 | paste -sd ';' -)
    zsnap_version=$($ZSNAP_BIN --version 2>&1 | head -n 1)
    stable_version=$(sanoid_version "$SANOID_STABLE_BIN")
    development_version=$(sanoid_version "$SANOID_DEVELOPMENT_BIN")
    development_revision=$SANOID_DEVELOPMENT_REVISION
    if [[ -z $development_revision ]]; then
        development_revision=$(git -c safe.directory="$(dirname -- "$SANOID_DEVELOPMENT_BIN")" -C "$(dirname -- "$SANOID_DEVELOPMENT_BIN")" rev-parse --short HEAD 2>/dev/null || printf unknown)
    fi
    cpu=$(awk -F: '/model name|Hardware|Processor/ { sub(/^[ \t]+/, "", $2); print $2; exit }' /proc/cpuinfo)
    fs=$(df -T "$WORK_DIR" | awk 'NR == 2 { print $2 }')
    commit=$(git -c safe.directory="$REPO_DIR" -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || printf unknown)
    source_markdown="commit \`$commit\`"
    source_html="commit <code>$commit</code>"
    if [[ -n $(git -c safe.directory="$REPO_DIR" -C "$REPO_DIR" status --porcelain 2>/dev/null) ]]; then
        source_markdown="a working tree based on commit \`$commit\`"
        source_html="a working tree based on commit <code>$commit</code>"
    fi
    per_pool=$((1 + BRANCH_COUNT + BRANCH_COUNT * LEAF_COUNT)); total=${#ALL_DATASETS[@]}; seeded=$((total * PRUNE_SNAPSHOTS))
    csu=$(summary_value snapshot zsnap-serial-unbatched 10); csb=$(summary_value snapshot zsnap-serial-batched 10)
    cad=$(summary_value snapshot zsnap-auto-defaults 10); cas=$(summary_value snapshot zsnap-auto-small-batches 10)
    psu=$(summary_value prune zsnap-serial-unbatched 10); psb=$(summary_value prune zsnap-serial-batched 10)
    pad=$(summary_value prune zsnap-auto-defaults 10); pas=$(summary_value prune zsnap-auto-small-batches 10)
    stable_create=$(summary_value snapshot sanoid-stable 10); dev_create=$(summary_value snapshot sanoid-development 10)
    stable_prune=$(summary_value prune sanoid-stable 10); dev_prune=$(summary_value prune sanoid-development 10)
    dev_create_ratio=$(ratio "$dev_create" "$stable_create"); dev_prune_ratio=$(ratio "$dev_prune" "$stable_prune")
    plan_only=$(raw_median plan zsnap-prune-plan)
    prune_execution=$(awk -v full="$pad" -v plan="$plan_only" 'BEGIN { printf "%.3f", full-plan }')
    plan_share=$(awk -v full="$pad" -v plan="$plan_only" 'BEGIN { printf "%.1f", 100*plan/full }')
    dataset_list=$(raw_median inventory dataset-list)
    snapshot_list=$(raw_median inventory snapshot-list)
    pool_list=$(raw_median inventory pool-list)
    plan_residual=$(awk -v plan="$plan_only" -v datasets="$dataset_list" -v snapshots="$snapshot_list" -v pools="$pool_list" 'BEGIN { value=plan-datasets-snapshots-pools; if (value<0) value=0; printf "%.3f", value }')
    snapshot_plan_share=$(awk -v plan="$plan_only" -v snapshots="$snapshot_list" 'BEGIN { printf "%.1f", 100*snapshots/plan }')
    snapshot_full_share=$(awk -v full="$pad" -v snapshots="$snapshot_list" 'BEGIN { printf "%.1f", 100*snapshots/full }')
    c_batch=$(ratio "$csu" "$csb"); c_pool=$(ratio "$csb" "$cad"); p_batch=$(ratio "$psu" "$psb"); p_pool=$(ratio "$psb" "$pad")
    c_small=$(ratio "$cas" "$cad"); p_small=$(ratio "$pas" "$pad")
    fastest_c=$(awk -F '\t' '$1=="snapshot" && $4=="zsnap" { if (!s || $10<b) { s=1;b=$10;l=$3;x=$14 } } END { printf "%s (%s ms, %s× Sanoid stable)",l,b,x }' "$SUMMARY_TSV")
    fastest_p=$(awk -F '\t' '$1=="prune" && $4=="zsnap" { if (!s || $10<b) { s=1;b=$10;l=$3;x=$14 } } END { printf "%s (%s ms, %s× Sanoid stable)",l,b,x }' "$SUMMARY_TSV")

    {
        printf '# zsnap ZFS benchmark\n\n'
        printf '> **Synthetic-result warning:** These pools use sparse files on `%s`, not physical vdevs. Results primarily measure process spawning, ZFS control-path work, discovery, and policy planning. They do **not** predict HDD, SSD, or NVMe throughput, latency, contention, durability, or production scaling.\n\n' "$fs"
        printf 'Generated %s from %s.\n\n## Environment\n\n' "$generated" "$source_markdown"
        printf '| Item | Value |\n| --- | --- |\n'
        printf '| OS | %s |\n| Kernel | `%s` |\n| CPU | %s |\n' "$os" "$kernel" "$cpu"
        printf '| ZFS | `%s` |\n| zsnap | `%s` |\n' "$zfs_version" "$zsnap_version"
        printf '| Sanoid stable | `%s` |\n| Sanoid development | `%s`, master `%s` |\n' "$stable_version" "$development_version" "$development_revision"
        printf '| Topology | %s pools × %s managed datasets (%s total), %s branches × %s leaves |\n' "$POOL_COUNT" "$per_pool" "$total" "$BRANCH_COUNT" "$LEAF_COUNT"
        printf '| Sparse vdev | `%s` per pool on `%s` |\n| Prune workload | %s snapshots/dataset (%s total) |\n' "$SPARSE_SIZE" "$fs" "$PRUNE_SNAPSHOTS" "$seeded"
        printf '| Sampling | 1 warm-up + %s trials/scenario; alternating forward/reverse order |\n\n' "$TRIALS"
        printf '## Results\n\nTimes are end-to-end wall-clock durations. Mutation calls count only the `zfs snapshot` or `zfs destroy` processes implied by the verified workload; discovery is included in time but not that column.\n\n'
        markdown_table
        printf '\n## Prune path diagnostic\n\n'
        printf 'A separate warm, non-mutating `zsnap plan --scope prune` pass used the same %s managed snapshots to isolate fresh discovery, configuration, and policy planning from deletion.\n\n' "$seeded"
        printf '| Measurement | Median | Interpretation |\n| --- | ---: | --- |\n'
        printf '| Full zsnap auto/default prune | %s ms | Discovery, planning, and %s batched destroy processes |\n' "$pad" "$total"
        printf '| Warm prune plan only | %s ms | Fresh discovery and policy work; no mutation |\n' "$plan_only"
        printf '| ↳ Configured-root dataset list | %s ms | One scoped `zfs list` process |\n' "$dataset_list"
        printf '| ↳ Managed-root snapshot/property list | %s ms | One scoped `zfs list` process |\n' "$snapshot_list"
        printf '| ↳ Configured-pool capacity list | %s ms | One scoped `zpool list` process |\n' "$pool_list"
        printf '| ↳ Other plan work (approximate) | %s ms | Locking, TOML, parsing, policy, rendering, and median subtraction noise |\n' "$plan_residual"
        printf '| Full minus plan (approximate) | %s ms | Mutation/process residual; not independently timed |\n\n' "$prune_execution"
        printf '\n## What this run says\n\n'
        printf -- '- Sanoid development took **%s×** the stable snapshot time and **%s×** the stable prune time on this isolated topology.\n' "$dev_create_ratio" "$dev_prune_ratio"
        printf -- '- Warm fresh discovery and planning were **%s%%** of the fastest full prune median. That is the maximum share a perfect plan cache could remove, but deletion candidates still require fresh revalidation for safety.\n' "$plan_share"
        printf -- '- The fresh snapshot/property scan alone was **%s%%** of plan time and **%s%%** of full prune time, making that native query the first optimization target.\n' "$snapshot_plan_share" "$snapshot_full_share"
        printf -- '- Fastest zsnap snapshot scenario: **%s**.\n- Fastest zsnap prune scenario: **%s**.\n' "$fastest_c" "$fastest_p"
        printf -- '- At one pool worker, default batching changed snapshot time by **%s×** and prune time by **%s×** versus batch size 1.\n' "$c_batch" "$p_batch"
        printf -- '- With default batches, auto pool workers changed snapshot time by **%s×** and prune time by **%s×** versus one worker.\n' "$c_pool" "$p_pool"
        printf -- '- Auto defaults versus small batches (4 snapshot / 3 prune) changed snapshot time by **%s×** and prune time by **%s×**. Once a batch fits all eligible targets, a larger cap cannot reduce command count further.\n\n' "$c_small" "$p_small"
        printf 'Ratios are first duration ÷ second duration, so values above 1 favor the second setting. Differences near 1 are synthetic noise.\n\n'
        printf '## Improvement guidance\n\n'
        printf -- '- Keep batching simple. Defaults already collapse this workload to one snapshot command per pool and one destroy command per dataset; media presets would not reduce those counts.\n'
        printf -- '- Keep same-pool mutations serialized and overlap independent pools. Validate `max_parallel_pools = 0` on real hardware; cap it when pools share controllers, CPU, or memory bandwidth.\n'
        printf -- '- Keep inventory fresh for deletion safety. zsnap now scopes dataset and snapshot scans to configured roots and capacity queries to configured pools; optimize those native reads before considering a stale cross-run cache.\n'
        printf -- '- A recursive `zfs destroy -r root@snap1,snap2` can reduce this uniform tree to one call per pool, but is unsafe unless every matching descendant/name is an explicitly validated prune candidate and no dataset has an override or hook.\n'
        printf -- '- A channel program can destroy exact candidates in one root-only invocation per pool, but adds Lua, instruction/memory ceilings, blocks concurrent administrative changes during execution, and can stop after partial application. Keep it a future opt-in unless real enormous prune sets justify it.\n'
        printf -- '- The current portable default already uses OpenZFS comma-list deletion per dataset and preserves granular errors and hooks.\n'
        printf -- '- Repeat on physical HDD, SSD, and NVMe pools before changing production settings. Sparse files are useful for regressions, not device tuning.\n\n'
        printf '## Method\n\nEach snapshot trial starts empty and must create one hourly snapshot per dataset. Each prune trial seeds %s expired Sanoid-compatible snapshots per dataset; zsnap seeds carry `org.zsnap:managed=yes`, while Sanoid seeds are unmarked. A successful prune returns to zero.\n\n' "$PRUNE_SNAPSHOTS"
        printf 'Both Sanoid versions and zsnap manage the same recursive roots and policy. Sanoid uses `--force-update` because the harness changes state externally. Setup, seeding, correctness checks, and cleanup are outside timed regions. The warm plan diagnostic and its three native inventory commands are timed separately on one seeded inventory. No Sanoid source is copied into this repository.\n\nRaw timings: [`results.tsv`](results.tsv). Reproduce with `make benchmark`; see [`README.md`](README.md).\n'
    } >"$WORK_DIR/results.md"

    {
        cat <<'EOF'
<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>zsnap ZFS benchmark</title>
<style>:root{color-scheme:dark;--bg:#0b1220;--panel:#111c2e;--text:#e7eef9;--muted:#a9b8cc;--accent:#63d6c5;--warn:#ffd166;--line:#2a3b53}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:15px/1.55 system-ui,sans-serif}main{max-width:1180px;margin:auto;padding:32px 20px 64px}h1,h2{line-height:1.2}h2{margin-top:2rem;color:var(--accent)}code{background:#18263b;padding:.12rem .35rem;border-radius:4px}.warning{border-left:5px solid var(--warn);background:var(--panel);padding:14px 18px}.wrap{overflow-x:auto;border:1px solid var(--line);border-radius:8px}table{border-collapse:collapse;width:100%;background:var(--panel)}th,td{padding:9px 11px;border-bottom:1px solid var(--line);text-align:left;white-space:nowrap}th{color:var(--accent);font-size:.82rem;text-transform:uppercase}td.n{text-align:right;font-variant-numeric:tabular-nums}.muted{color:var(--muted)}a{color:var(--accent)}</style></head><body><main>
EOF
        printf '<h1>zsnap ZFS benchmark</h1><p class="warning"><strong>Synthetic-result warning:</strong> sparse files on <code>%s</code> measure control-path overhead, not physical-device performance or durability.</p>' "$fs"
        printf '<p class="muted">Generated %s from %s.</p><h2>Environment</h2><div class="wrap"><table><tbody>' "$generated" "$source_html"
        printf '<tr><th>OS</th><td>%s</td></tr><tr><th>Kernel</th><td><code>%s</code></td></tr><tr><th>CPU</th><td>%s</td></tr>' "$(printf '%s' "$os"|html_escape)" "$(printf '%s' "$kernel"|html_escape)" "$(printf '%s' "$cpu"|html_escape)"
        printf '<tr><th>ZFS</th><td><code>%s</code></td></tr><tr><th>zsnap</th><td><code>%s</code></td></tr>' "$(printf '%s' "$zfs_version"|html_escape)" "$(printf '%s' "$zsnap_version"|html_escape)"
        printf '<tr><th>Sanoid stable</th><td><code>%s</code></td></tr><tr><th>Sanoid development</th><td><code>%s, master %s</code></td></tr>' "$(printf '%s' "$stable_version"|html_escape)" "$(printf '%s' "$development_version"|html_escape)" "$development_revision"
        printf '<tr><th>Topology</th><td>%s pools × %s datasets (%s total)</td></tr><tr><th>Prune workload</th><td>%s/dataset (%s total)</td></tr><tr><th>Sampling</th><td>1 warm-up + %s trials</td></tr></tbody></table></div>' "$POOL_COUNT" "$per_pool" "$total" "$PRUNE_SNAPSHOTS" "$seeded" "$TRIALS"
        printf '<h2>Results</h2><p>End-to-end time; mutation-call count excludes discovery calls.</p><div class="wrap">'; html_table; printf '</div>'
        printf '<h2>Prune path diagnostic</h2><div class="wrap"><table><thead><tr><th>Measurement</th><th>Median</th></tr></thead><tbody><tr><td>Full zsnap auto/default prune</td><td class="n">%s ms</td></tr><tr><td>Warm prune plan only</td><td class="n">%s ms</td></tr><tr><td>Configured-root dataset list</td><td class="n">%s ms</td></tr><tr><td>Managed-root snapshot/property list</td><td class="n">%s ms</td></tr><tr><td>Configured-pool capacity list</td><td class="n">%s ms</td></tr><tr><td>Other plan work (approximate)</td><td class="n">%s ms</td></tr><tr><td>Full minus plan (approximate)</td><td class="n">%s ms</td></tr></tbody></table></div>' "$pad" "$plan_only" "$dataset_list" "$snapshot_list" "$pool_list" "$plan_residual" "$prune_execution"
        printf '<h2>What this run says</h2><ul><li>Fastest snapshot: <strong>%s</strong>.</li><li>Fastest prune: <strong>%s</strong>.</li>' "$(printf '%s' "$fastest_c"|html_escape)" "$(printf '%s' "$fastest_p"|html_escape)"
        printf '<li>Sanoid development took %s× stable snapshot time and %s× stable prune time.</li>' "$dev_create_ratio" "$dev_prune_ratio"
        printf '<li>Warm fresh discovery and planning were %s%% of the fastest full prune median; a deletion cache would still need fresh revalidation.</li>' "$plan_share"
        printf '<li>The snapshot/property scan alone was %s%% of plan time and %s%% of full prune time.</li>' "$snapshot_plan_share" "$snapshot_full_share"
        printf '<li>Serial batching ratios: %s× snapshot, %s× prune.</li><li>Auto-pool ratios with default batches: %s× snapshot, %s× prune.</li><li>Defaults versus small batches: %s× snapshot, %s× prune.</li></ul>' "$c_batch" "$p_batch" "$c_pool" "$p_pool" "$c_small" "$p_small"
        printf '<h2>Improvement guidance</h2><ul><li>Keep default batching and overlap only independent pools.</li><li>Keep fresh inventory scoped to configured roots and pools; optimize native reads before adding a cache.</li><li>Recursive destroy needs proof that every descendant candidate is safe.</li><li>Keep channel programs optional unless enormous real prune sets justify root-only Lua and partial-application complexity.</li><li>Retest on physical pools before device tuning.</li></ul>'
        printf '<h2>Method</h2><p>Snapshot runs create one snapshot per dataset. Prune runs seed %s expired snapshots per dataset; zsnap seeds carry <code>org.zsnap:managed=yes</code>. All outcomes are verified. A separate warm plan-only pass and its three native inventory commands are timed on the same seeded state.</p><p><a href="results.tsv">Raw timings</a> · <a href="README.md">Benchmark guide</a></p></main></body></html>\n' "$PRUNE_SNAPSHOTS"
    } >"$WORK_DIR/results.html"

    install -m 644 "$RAW_TSV" "$REPORT_TSV"
    install -m 644 "$WORK_DIR/results.md" "$REPORT_MD"
    install -m 644 "$WORK_DIR/results.html" "$REPORT_HTML"
    [[ -z $OWNER ]] || chown "$OWNER" "$REPORT_TSV" "$REPORT_MD" "$REPORT_HTML"
}

printf 'Creating %s disposable sparse-file pools under %s...\n' "$POOL_COUNT" "$WORK_DIR"
for ((pool_index = 1; pool_index <= POOL_COUNT; pool_index++)); do
    pool="zsnapbench_${RUN_TOKEN}_p${pool_index}"
    is_benchmark_pool_name "$pool" || die "generated unsafe pool name: $pool"
    POOLS+=("$pool")
    vdev="$WORK_DIR/vdev-p${pool_index}.img"
    "$TRUNCATE_BIN" -s "$SPARSE_SIZE" "$vdev"
    "$ZPOOL_BIN" create -f -o cachefile=none -O atime=off -O compression=off -O mountpoint=none "$pool" "$vdev"
    root="$pool/bench"; ROOT_DATASETS+=("$root"); "$ZFS_BIN" create "$root"; ALL_DATASETS+=("$root")
    for ((branch_index = 1; branch_index <= BRANCH_COUNT; branch_index++)); do
        branch="$root/branch${branch_index}"; "$ZFS_BIN" create "$branch"; ALL_DATASETS+=("$branch")
        for ((leaf_index = 1; leaf_index <= LEAF_COUNT; leaf_index++)); do
            leaf="$branch/leaf${leaf_index}"; "$ZFS_BIN" create "$leaf"; ALL_DATASETS+=("$leaf")
        done
    done
done
expected=$((POOL_COUNT * (1 + BRANCH_COUNT + BRANCH_COUNT * LEAF_COUNT)))
[[ ${#ALL_DATASETS[@]} == "$expected" ]] || die "dataset topology mismatch"

install -d -m 755 "$WORK_DIR/sanoid-cache-stable-snapshot" "$WORK_DIR/sanoid-run-stable-snapshot" "$WORK_DIR/sanoid-cache-stable-prune" "$WORK_DIR/sanoid-run-stable-prune"
install -d -m 755 "$WORK_DIR/sanoid-cache-development-snapshot" "$WORK_DIR/sanoid-run-development-snapshot" "$WORK_DIR/sanoid-cache-development-prune" "$WORK_DIR/sanoid-run-development-prune"
write_sanoid_config snapshot stable "$SANOID_STABLE_DEFAULTS"; write_sanoid_config prune stable "$SANOID_STABLE_DEFAULTS"
write_sanoid_config snapshot development "$SANOID_DEVELOPMENT_DEFAULTS"; write_sanoid_config prune development "$SANOID_DEVELOPMENT_DEFAULTS"
for index in "${!scenario_ids[@]}"; do
    [[ ${scenario_tools[$index]} != zsnap ]] || { write_zsnap_config snapshot "$index"; write_zsnap_config prune "$index"; }
done
printf 'phase\tscenario\ttool\tmax_parallel_pools\tsnapshot_batch_size\tprune_batch_size\ttrial\telapsed_ms\tlabel\n' >"$RAW_TSV"

for phase in snapshot prune; do
    printf '\nWarm-up: %s\n' "$phase"
    for index in "${!scenario_ids[@]}"; do
        printf '  %-38s' "${scenario_labels[$index]}"
        prepare_variant "$phase" "$index"
        if ! run_variant "$phase" "$index" "$WORK_DIR/last-command.log"; then
            sed -n '1,260p' "$WORK_DIR/last-command.log" >&2; die "warm-up failed"
        fi
        verify_variant "$phase" "$index"; printf 'ok\n'
    done
    printf 'Measured: %s\n' "$phase"
    for ((trial = 1; trial <= TRIALS; trial++)); do
        if ((trial % 2)); then
            for index in "${!scenario_ids[@]}"; do
                printf '  trial %d/%d  %-38s' "$trial" "$TRIALS" "${scenario_labels[$index]}"
                timed_variant "$phase" "$index" "$trial"
            done
        else
            for ((index = ${#scenario_ids[@]} - 1; index >= 0; index--)); do
                printf '  trial %d/%d  %-38s' "$trial" "$TRIALS" "${scenario_labels[$index]}"
                timed_variant "$phase" "$index" "$trial"
            done
        fi
    done
done

measure_prune_plan
clear_snapshots; aggregate_results; render_reports; SUCCESS=1
printf '\nBenchmark complete.\n  Markdown: %s\n  HTML:     %s\n  Raw TSV:  %s\n' "$REPORT_MD" "$REPORT_HTML" "$REPORT_TSV"
