use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Settings;
use crate::process::output_with_timeout;

pub const MANAGED_PROPERTY: &str = "org.zsnap:managed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub dataset: String,
    pub name: String,
    pub created: i64,
    pub managed: bool,
    pub user_holds: u64,
    pub has_clones: bool,
}

impl Snapshot {
    pub const fn prune_protected(&self) -> bool {
        self.user_holds > 0 || self.has_clones
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub name: String,
    pub capacity_percent: u8,
}

#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub datasets: BTreeSet<String>,
    pub snapshots: Vec<Snapshot>,
    pub pools: BTreeMap<String, Pool>,
}

impl Inventory {
    pub fn discover<'a>(
        settings: &Settings,
        configured_datasets: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self> {
        let configured_datasets: BTreeSet<_> =
            configured_datasets.into_iter().map(str::to_owned).collect();
        let configured_pools: BTreeSet<_> = configured_datasets
            .iter()
            .map(|dataset| Self::pool_for_dataset(dataset))
            .map(str::to_owned)
            .collect();
        // `zfs list -r` already covers descendants, so passing a nested section as well
        // would make ZFS traverse that subtree a second time.
        let roots: Vec<&String> = configured_datasets
            .iter()
            .filter(|dataset| {
                !configured_datasets
                    .iter()
                    .any(|ancestor| is_descendant(dataset, ancestor))
            })
            .collect();
        let mut dataset_args: Vec<OsString> = vec![
            "list".into(),
            "-H".into(),
            "-p".into(),
            "-r".into(),
            "-t".into(),
            "filesystem,volume".into(),
            "-o".into(),
            format!("name,{MANAGED_PROPERTY}").into(),
        ];
        dataset_args.extend(roots.iter().map(OsString::from));
        let timeout = settings.zfs_timeout();
        let datasets_output =
            run_dataset_list_checked(&settings.zfs_command, &dataset_args, timeout)?;
        let mut snapshot_args: Vec<OsString> = vec![
            "list".into(),
            "-H".into(),
            "-p".into(),
            "-r".into(),
            "-t".into(),
            "snapshot".into(),
            "-o".into(),
            format!("name,creation,{MANAGED_PROPERTY},userrefs,clones").into(),
        ];
        snapshot_args.extend(roots.iter().map(OsString::from));
        let snapshots_output = run_checked(&settings.zfs_command, &snapshot_args, timeout)?;
        let mut pool_args: Vec<OsString> = vec![
            "list".into(),
            "-H".into(),
            "-p".into(),
            "-o".into(),
            "name,capacity".into(),
        ];
        pool_args.extend(configured_pools.into_iter().map(OsString::from));
        let pools_output = run_checked(&settings.zpool_command, &pool_args, timeout)?;

        let datasets = parse_datasets(&String::from_utf8_lossy(&datasets_output.stdout))?;
        // Nested sections are not passed to ZFS, so it cannot report them missing.
        let missing: BTreeSet<String> = configured_datasets
            .iter()
            .filter(|dataset| !datasets.contains(*dataset))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(missing_datasets_error(missing));
        }

        Ok(Self {
            datasets,
            snapshots: parse_snapshots(&String::from_utf8_lossy(&snapshots_output.stdout))?,
            pools: parse_pools(&String::from_utf8_lossy(&pools_output.stdout))?,
        })
    }

    pub fn pool_for_dataset(dataset: &str) -> &str {
        dataset.split('/').next().unwrap_or(dataset)
    }
}

fn run_checked(program: &Path, args: &[OsString], timeout: Option<Duration>) -> Result<Output> {
    let output = run_output(program, args, timeout)?;
    check_output(program, args, output)
}

fn run_dataset_list_checked(
    program: &Path,
    args: &[OsString],
    timeout: Option<Duration>,
) -> Result<Output> {
    let output = run_output(program, args, timeout)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(missing) = parse_missing_datasets(&stderr) {
            return Err(missing_datasets_error(missing));
        }
    }
    check_output(program, args, output)
}

fn missing_datasets_error(missing: BTreeSet<String>) -> anyhow::Error {
    let list = missing
        .into_iter()
        .map(|dataset| format!("  - {dataset}"))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::anyhow!(
        "configured ZFS dataset sections reference missing names:\n{list}\ncorrect or remove these sections, then rerun `zsnap check --probe`"
    )
}

fn is_descendant(dataset: &str, ancestor: &str) -> bool {
    dataset
        .strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn run_output(program: &Path, args: &[OsString], timeout: Option<Duration>) -> Result<Output> {
    let output = output_with_timeout(Command::new(program).args(args).env("LC_ALL", "C"), timeout)
        .with_context(|| format!("failed to execute {}", program.display()))?;
    output.with_context(|| {
        format!(
            "{} {} timed out after {}s (settings.zfs_timeout)",
            program.display(),
            render_args(args),
            timeout.unwrap_or_default().as_secs()
        )
    })
}

fn render_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn check_output(program: &Path, args: &[OsString], output: Output) -> Result<Output> {
    if !output.status.success() {
        let rendered_args = render_args(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {} failed with {}: {}",
            program.display(),
            rendered_args,
            output.status,
            stderr.trim()
        );
    }
    Ok(output)
}

fn parse_missing_datasets(stderr: &str) -> Option<BTreeSet<String>> {
    let mut missing = BTreeSet::new();
    for line in stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let rest = line.strip_prefix("cannot open '")?;
        let (dataset, reason) = rest.split_once("': ")?;
        if !matches!(reason, "dataset does not exist" | "no such pool") || dataset.is_empty() {
            return None;
        }
        missing.insert(dataset.to_owned());
    }
    (!missing.is_empty()).then_some(missing)
}

/// Parses `zfs list -o name,org.zsnap:managed` for filesystems and volumes.
///
/// Snapshots inherit user properties from their dataset, so a marker set on a dataset
/// would make every unmarked snapshot beneath it look zsnap-managed. That is refused
/// outright rather than risk pruning snapshots zsnap did not create.
pub fn parse_datasets(output: &str) -> Result<BTreeSet<String>> {
    let mut datasets = BTreeSet::new();
    let mut marked: Vec<&str> = Vec::new();
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, marker)) = line.split_once('\t') else {
            bail!(
                "invalid dataset listing on line {}: expected 2 tab-separated fields: {line:?}",
                index + 1
            );
        };
        if name.is_empty() || name.contains('@') || marker.contains('\t') {
            bail!("invalid dataset listing on line {}: {line:?}", index + 1);
        }
        // Report only the topmost marked datasets; the listing is parent-first.
        if is_managed_value(marker) && !marked.iter().any(|parent| is_descendant(name, parent)) {
            marked.push(name);
        }
        datasets.insert(name.to_owned());
    }
    if !marked.is_empty() {
        let list = marked
            .iter()
            .map(|dataset| format!("  - {dataset}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "the snapshot ownership marker {MANAGED_PROPERTY} is set on or inherited by dataset(s):\n{list}\nsnapshots inherit dataset properties, so unmarked snapshots there would look zsnap-managed and could be pruned; find where it is set with `zfs get -r -s local {MANAGED_PROPERTY} <pool>` and clear it with `zfs inherit {MANAGED_PROPERTY} <dataset>`"
        );
    }
    Ok(datasets)
}

fn is_managed_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "yes" | "true" | "on"
    )
}

pub fn parse_snapshots(output: &str) -> Result<Vec<Snapshot>> {
    let mut snapshots = BTreeMap::new();
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 5 {
            bail!(
                "invalid snapshot listing on line {}: expected 5 tab-separated fields, got {}",
                index + 1,
                fields.len()
            );
        }
        let (dataset, name) = fields[0].split_once('@').with_context(|| {
            format!(
                "invalid snapshot name {:?} on line {}",
                fields[0],
                index + 1
            )
        })?;
        let created = fields[1].parse::<i64>().with_context(|| {
            format!(
                "invalid snapshot creation time {:?} on line {}",
                fields[1],
                index + 1
            )
        })?;
        let managed = is_managed_value(fields[2]);
        let user_holds = fields[3].parse::<u64>().with_context(|| {
            format!(
                "invalid snapshot userrefs count {:?} on line {}",
                fields[3],
                index + 1
            )
        })?;
        let has_clones = !matches!(fields[4].trim(), "" | "-");
        snapshots.insert(
            fields[0].to_owned(),
            Snapshot {
                dataset: dataset.to_owned(),
                name: name.to_owned(),
                created,
                managed,
                user_holds,
                has_clones,
            },
        );
    }
    Ok(snapshots.into_values().collect())
}

pub fn parse_pools(output: &str) -> Result<BTreeMap<String, Pool>> {
    let mut pools = BTreeMap::new();
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 2 {
            bail!(
                "invalid zpool listing on line {}: expected 2 tab-separated fields, got {}",
                index + 1,
                fields.len()
            );
        }
        let capacity = fields[1]
            .trim()
            .trim_end_matches('%')
            .parse::<u8>()
            .with_context(|| {
                format!(
                    "invalid pool capacity {:?} on line {}",
                    fields[1],
                    index + 1
                )
            })?;
        if capacity > 100 {
            bail!("invalid pool capacity {capacity} on line {}", index + 1);
        }
        pools.insert(
            fields[0].to_owned(),
            Pool {
                name: fields[0].to_owned(),
                capacity_percent: capacity,
            },
        );
    }
    Ok(pools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inventory_outputs() {
        let datasets = parse_datasets("tank\t-\ntank/data\t-\nbackup\t-\n").unwrap();
        assert!(datasets.contains("tank/data"));
        assert!(parse_datasets("tank\ntank/data\n").is_err());

        let snapshots = parse_snapshots(
            "tank/data@autosnap_2026-08-27_12:00:00_hourly\t1787832000\tyes\t1\ttank/clone-a,tank/clone-b\n\
             tank/data@manual\t1787831000\t-\t0\t-\n",
        )
        .unwrap();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots[0].managed);
        assert_eq!(snapshots[0].user_holds, 1);
        assert!(snapshots[0].has_clones);
        assert!(snapshots[0].prune_protected());
        assert!(!snapshots[1].managed);
        assert!(!snapshots[1].prune_protected());

        let pools = parse_pools("tank\t42%\nbackup\t7%\n").unwrap();
        assert_eq!(pools["tank"].capacity_percent, 42);
    }

    #[test]
    fn refuses_an_ownership_marker_inherited_from_a_dataset() {
        let error = parse_datasets(
            "tank\t-\ntank/data\tyes\ntank/data/db\tyes\ntank/database\t-\ntank/vm\ton\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("  - tank/data\n  - tank/vm\n"), "{error}");
        assert!(!error.contains("tank/data/db"), "{error}");
        assert!(error.contains("zfs inherit org.zsnap:managed"), "{error}");
        // A non-marker value on a dataset cannot make snapshots look managed.
        assert!(parse_datasets("tank\tno\n").is_ok());
    }

    #[test]
    fn recognizes_only_clean_missing_dataset_diagnostics() {
        let missing = parse_missing_datasets(
            "cannot open 'pool/missing-b': dataset does not exist\n\
             cannot open 'pool/missing-a': dataset does not exist\n",
        )
        .unwrap();
        assert_eq!(
            missing.into_iter().collect::<Vec<_>>(),
            ["pool/missing-a", "pool/missing-b"]
        );
        assert!(parse_missing_datasets("cannot open 'pool/data': permission denied\n").is_none());
        assert!(
            parse_missing_datasets(
                "cannot open 'missingpool': no such pool\nunrelated diagnostic\n"
            )
            .is_none()
        );
    }
}
