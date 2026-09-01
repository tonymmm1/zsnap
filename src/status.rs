use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::planner::Plan;
use crate::zfs::Inventory;

pub const STATUS_CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusTotals {
    pub pools: usize,
    pub datasets: usize,
    pub snapshots: usize,
    pub managed_snapshots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolStatus {
    pub name: String,
    pub capacity_percent: Option<u8>,
    pub datasets: usize,
    pub snapshots: usize,
    pub managed_snapshots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatasetStatus {
    pub name: String,
    pub pool: String,
    pub snapshots: usize,
    pub managed_snapshots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusCache {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub inventory_scan_milliseconds: u64,
    pub scope_roots: Vec<String>,
    pub totals: StatusTotals,
    pub pools: Vec<PoolStatus>,
    pub datasets: Vec<DatasetStatus>,
}

impl StatusCache {
    pub fn from_inventory(
        inventory: &Inventory,
        scope_roots: impl IntoIterator<Item = String>,
        inventory_scan: Duration,
        completed_plan: Option<&Plan>,
    ) -> Self {
        let mut dataset_names = inventory.datasets.clone();
        let mut snapshot_counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for snapshot in &inventory.snapshots {
            dataset_names.insert(snapshot.dataset.clone());
            let counts = snapshot_counts.entry(snapshot.dataset.clone()).or_default();
            counts.0 += 1;
            counts.1 += usize::from(snapshot.managed);
        }

        if let Some(plan) = completed_plan {
            for action in &plan.snapshots {
                dataset_names.insert(action.dataset.clone());
                let descendant_prefix = format!("{}/", action.dataset);
                let targets = dataset_names
                    .iter()
                    .filter(|dataset| {
                        **dataset == action.dataset
                            || (action.recursive && dataset.starts_with(&descendant_prefix))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                for dataset in targets {
                    let counts = snapshot_counts.entry(dataset).or_default();
                    counts.0 += action.names.len();
                    counts.1 += action.names.len();
                }
            }
            for action in &plan.prunes {
                dataset_names.insert(action.dataset.clone());
                let counts = snapshot_counts.entry(action.dataset.clone()).or_default();
                counts.0 = counts.0.saturating_sub(action.names.len());
                counts.1 = counts.1.saturating_sub(action.names.len());
            }
        }

        let datasets = dataset_names
            .into_iter()
            .map(|name| {
                let (snapshot_count, managed_snapshots) =
                    snapshot_counts.get(&name).copied().unwrap_or_default();
                DatasetStatus {
                    pool: Inventory::pool_for_dataset(&name).to_owned(),
                    name,
                    snapshots: snapshot_count,
                    managed_snapshots,
                }
            })
            .collect::<Vec<_>>();

        let mut pools = inventory
            .pools
            .values()
            .map(|pool| {
                (
                    pool.name.clone(),
                    PoolStatus {
                        name: pool.name.clone(),
                        capacity_percent: Some(pool.capacity_percent),
                        datasets: 0,
                        snapshots: 0,
                        managed_snapshots: 0,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for dataset in &datasets {
            let pool = pools
                .entry(dataset.pool.clone())
                .or_insert_with(|| PoolStatus {
                    name: dataset.pool.clone(),
                    capacity_percent: None,
                    datasets: 0,
                    snapshots: 0,
                    managed_snapshots: 0,
                });
            pool.datasets += 1;
            pool.snapshots += dataset.snapshots;
            pool.managed_snapshots += dataset.managed_snapshots;
        }
        let pools = pools.into_values().collect::<Vec<_>>();
        let snapshots = datasets.iter().map(|dataset| dataset.snapshots).sum();
        let managed_snapshots = datasets
            .iter()
            .map(|dataset| dataset.managed_snapshots)
            .sum();
        let scan_milliseconds = inventory_scan.as_millis().min(u128::from(u64::MAX)) as u64;
        let mut scope_roots = scope_roots.into_iter().collect::<Vec<_>>();
        scope_roots.sort();
        scope_roots.dedup();

        Self {
            version: STATUS_CACHE_VERSION,
            generated_at: Utc::now(),
            inventory_scan_milliseconds: scan_milliseconds,
            scope_roots,
            totals: StatusTotals {
                pools: pools.len(),
                datasets: datasets.len(),
                snapshots,
                managed_snapshots,
            },
            pools,
            datasets,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read(path)
            .with_context(|| format!("failed to read status cache {}", path.display()))?;
        let cache: Self = serde_json::from_slice(&contents)
            .with_context(|| format!("failed to parse status cache {}", path.display()))?;
        cache
            .validate()
            .with_context(|| format!("invalid status cache {}", path.display()))?;
        Ok(cache)
    }

    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent_existed = parent.exists();
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create status cache directory {}",
                parent.display()
            )
        })?;
        if !parent_existed {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o750)).with_context(|| {
                format!(
                    "failed to set status cache directory permissions on {}",
                    parent.display()
                )
            })?;
        }

        let file_name = path
            .file_name()
            .context("status cache path must name a file")?
            .to_string_lossy();
        let nonce = Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let temporary = parent.join(format!(".{file_name}.tmp.{}.{nonce}", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| {
                    format!(
                        "failed to create temporary status cache {}",
                        temporary.display()
                    )
                })?;
            serde_json::to_writer_pretty(&mut file, self).with_context(|| {
                format!(
                    "failed to serialize temporary status cache {}",
                    temporary.display()
                )
            })?;
            file.write_all(b"\n")?;
            file.sync_all().with_context(|| {
                format!(
                    "failed to synchronize temporary status cache {}",
                    temporary.display()
                )
            })?;
            fs::rename(&temporary, path).with_context(|| {
                format!(
                    "failed to atomically replace status cache {}",
                    path.display()
                )
            })?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| {
                    format!(
                        "failed to synchronize status cache directory {}",
                        parent.display()
                    )
                })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == STATUS_CACHE_VERSION,
            "unsupported status cache version {}; expected {}",
            self.version,
            STATUS_CACHE_VERSION
        );
        let mut pool_names = BTreeSet::new();
        for pool in &self.pools {
            ensure!(
                !pool.name.is_empty(),
                "status cache contains an empty pool name"
            );
            ensure!(
                pool_names.insert(pool.name.as_str()),
                "status cache contains duplicate pool {:?}",
                pool.name
            );
            ensure!(
                pool.managed_snapshots <= pool.snapshots,
                "status cache pool {:?} has more managed snapshots than snapshots",
                pool.name
            );
            ensure!(
                pool.capacity_percent.is_none_or(|capacity| capacity <= 100),
                "status cache pool {:?} has an invalid capacity",
                pool.name
            );
        }
        let mut dataset_names = BTreeSet::new();
        for dataset in &self.datasets {
            ensure!(
                !dataset.name.is_empty(),
                "status cache contains an empty dataset name"
            );
            ensure!(
                dataset_names.insert(dataset.name.as_str()),
                "status cache contains duplicate dataset {:?}",
                dataset.name
            );
            ensure!(
                pool_names.contains(dataset.pool.as_str()),
                "status cache dataset {:?} references missing pool {:?}",
                dataset.name,
                dataset.pool
            );
            ensure!(
                Inventory::pool_for_dataset(&dataset.name) == dataset.pool,
                "status cache dataset {:?} has inconsistent pool {:?}",
                dataset.name,
                dataset.pool
            );
            ensure!(
                dataset.managed_snapshots <= dataset.snapshots,
                "status cache dataset {:?} has more managed snapshots than snapshots",
                dataset.name
            );
        }
        let snapshots = self.datasets.iter().map(|dataset| dataset.snapshots).sum();
        let managed_snapshots = self
            .datasets
            .iter()
            .map(|dataset| dataset.managed_snapshots)
            .sum();
        ensure!(
            self.totals
                == (StatusTotals {
                    pools: self.pools.len(),
                    datasets: self.datasets.len(),
                    snapshots,
                    managed_snapshots,
                }),
            "status cache totals do not match its pool and dataset records"
        );
        for pool in &self.pools {
            let datasets = self
                .datasets
                .iter()
                .filter(|dataset| dataset.pool == pool.name)
                .collect::<Vec<_>>();
            ensure!(
                pool.datasets == datasets.len()
                    && pool.snapshots
                        == datasets
                            .iter()
                            .map(|dataset| dataset.snapshots)
                            .sum::<usize>()
                    && pool.managed_snapshots
                        == datasets
                            .iter()
                            .map(|dataset| dataset.managed_snapshots)
                            .sum::<usize>(),
                "status cache pool totals do not match dataset records for {:?}",
                pool.name
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Policy;
    use crate::planner::{PruneAction, SnapshotAction};
    use crate::zfs::{Pool, Snapshot};
    use tempfile::tempdir;

    #[test]
    fn summarizes_inventory_and_applies_a_completed_plan() {
        let inventory = Inventory {
            datasets: ["tank", "tank/a", "tank/b", "backup"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            snapshots: vec![
                snapshot("tank", "manual", false),
                snapshot("tank/a", "old", true),
                snapshot("tank/a", "manual", false),
                snapshot("backup", "keep", true),
            ],
            pools: [pool("tank", 75), pool("backup", 40)]
                .into_iter()
                .map(|pool| (pool.name.clone(), pool))
                .collect(),
        };
        let policy = Policy::default();
        let plan = Plan {
            snapshots: vec![SnapshotAction {
                pool: "tank".to_owned(),
                dataset: "tank".to_owned(),
                recursive: true,
                names: vec!["new".to_owned()],
                kinds: vec![],
                policy: policy.clone(),
            }],
            prunes: vec![PruneAction {
                pool: "tank".to_owned(),
                dataset: "tank/a".to_owned(),
                names: vec!["old".to_owned()],
                policy,
            }],
            ..Plan::default()
        };

        let cache = StatusCache::from_inventory(
            &inventory,
            ["tank".to_owned(), "backup".to_owned()],
            Duration::from_millis(123),
            Some(&plan),
        );

        assert_eq!(cache.inventory_scan_milliseconds, 123);
        assert_eq!(cache.totals.pools, 2);
        assert_eq!(cache.totals.datasets, 4);
        assert_eq!(cache.totals.snapshots, 6);
        assert_eq!(cache.totals.managed_snapshots, 4);
        let tank = cache.pools.iter().find(|pool| pool.name == "tank").unwrap();
        assert_eq!(tank.datasets, 3);
        assert_eq!(tank.snapshots, 5);
        assert_eq!(tank.managed_snapshots, 3);
        cache.validate().unwrap();
    }

    #[test]
    fn atomically_round_trips_with_restricted_permissions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("zsnap.cache");
        let cache =
            StatusCache::from_inventory(&Inventory::default(), Vec::new(), Duration::ZERO, None);

        cache.write_atomic(&path).unwrap();

        assert_eq!(StatusCache::load(&path).unwrap(), cache);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn snapshot(dataset: &str, name: &str, managed: bool) -> Snapshot {
        Snapshot {
            dataset: dataset.to_owned(),
            name: name.to_owned(),
            created: 0,
            managed,
        }
    }

    fn pool(name: &str, capacity_percent: u8) -> Pool {
        Pool {
            name: name.to_owned(),
            capacity_percent,
        }
    }
}
