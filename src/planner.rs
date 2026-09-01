use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike,
    Utc, Weekday,
};
use serde::Serialize;

use crate::config::{Config, Policy, Recursion, ScheduleTimezone};
use crate::model::{SnapshotKind, SnapshotStrategy};
use crate::zfs::{Inventory, Snapshot};

#[derive(Debug, Clone)]
pub struct ResolvedDataset {
    pub name: String,
    pub pool: String,
    pub policy: Policy,
    pub snapshot_strategy: SnapshotStrategy,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotAction {
    pub pool: String,
    pub dataset: String,
    pub recursive: bool,
    pub names: Vec<String>,
    pub kinds: Vec<SnapshotKind>,
    #[serde(skip)]
    pub policy: Policy,
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneAction {
    pub pool: String,
    pub dataset: String,
    pub names: Vec<String>,
    #[serde(skip)]
    pub policy: Policy,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtectedSnapshot {
    pub pool: String,
    pub dataset: String,
    pub name: String,
    pub user_holds: u64,
    pub has_clones: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub snapshots: Vec<SnapshotAction>,
    pub prunes: Vec<PruneAction>,
    pub deferred_prune_datasets: Vec<String>,
    pub protected_snapshots: Vec<ProtectedSnapshot>,
}

impl Plan {
    pub fn pools(&self) -> BTreeSet<&str> {
        self.snapshots
            .iter()
            .map(|action| action.pool.as_str())
            .chain(self.prunes.iter().map(|action| action.pool.as_str()))
            .chain(
                self.protected_snapshots
                    .iter()
                    .map(|snapshot| snapshot.pool.as_str()),
            )
            .collect()
    }

    pub fn snapshot_count(&self) -> usize {
        self.snapshots.iter().map(|action| action.names.len()).sum()
    }

    pub fn prune_count(&self) -> usize {
        self.prunes.iter().map(|action| action.names.len()).sum()
    }

    pub fn protected_snapshot_count(&self) -> usize {
        self.protected_snapshots.len()
    }

    pub fn retain_snapshots_only(&mut self) {
        self.prunes.clear();
        self.deferred_prune_datasets.clear();
        self.protected_snapshots.clear();
    }

    pub fn retain_prunes_only(&mut self) {
        self.snapshots.clear();
    }
}

pub fn resolve_datasets(config: &Config, inventory: &Inventory) -> Result<Vec<ResolvedDataset>> {
    let mut sections: Vec<_> = config.datasets.iter().collect();
    sections.sort_by(|(left, _), (right, _)| {
        dataset_depth(left)
            .cmp(&dataset_depth(right))
            .then_with(|| left.cmp(right))
    });

    let mut resolved: BTreeMap<String, ResolvedDataset> = BTreeMap::new();
    for (root, section) in sections {
        if !inventory.datasets.contains(root) {
            bail!("configured dataset {root:?} does not exist in the ZFS inventory");
        }

        let base = resolved.get(root).map(|entry| &entry.policy);
        let policy = config.effective_policy(root, base)?;
        let prefix = format!("{root}/");
        let descendants: Vec<_> = inventory
            .datasets
            .iter()
            .filter(|name| name.starts_with(&prefix))
            .cloned()
            .collect();

        let mut targets = match section.recursive {
            Recursion::None => vec![root.clone()],
            Recursion::Children | Recursion::Zfs => {
                let mut values = Vec::with_capacity(descendants.len() + 1);
                values.push(root.clone());
                values.extend(descendants);
                values
            }
        };
        if section.process_children_only {
            targets.retain(|name| name != root);
        }

        for target in targets {
            let strategy = match section.recursive {
                Recursion::Zfs if target == *root => SnapshotStrategy::RecursiveRoot,
                Recursion::Zfs => SnapshotStrategy::CoveredByRecursive(root.clone()),
                _ => SnapshotStrategy::Individual,
            };
            resolved.insert(
                target.clone(),
                ResolvedDataset {
                    pool: Inventory::pool_for_dataset(&target).to_owned(),
                    name: target,
                    policy: policy.clone(),
                    snapshot_strategy: strategy,
                },
            );
        }
    }

    Ok(resolved.into_values().collect())
}

pub fn build_plan(
    config: &Config,
    inventory: &Inventory,
    datasets: &[ResolvedDataset],
    now: DateTime<Utc>,
) -> Result<Plan> {
    let mut by_dataset: BTreeMap<&str, Vec<&Snapshot>> = BTreeMap::new();
    for snapshot in &inventory.snapshots {
        by_dataset
            .entry(&snapshot.dataset)
            .or_default()
            .push(snapshot);
    }

    let timestamp = snapshot_timestamp(config.settings.timezone, now);
    let mut plan = Plan::default();
    for dataset in datasets {
        let snapshots = by_dataset
            .get(dataset.name.as_str())
            .cloned()
            .unwrap_or_default();
        if dataset.policy.autosnap
            && !matches!(
                dataset.snapshot_strategy,
                SnapshotStrategy::CoveredByRecursive(_)
            )
        {
            let mut kinds = Vec::new();
            let mut names = Vec::new();
            for kind in SnapshotKind::ALL {
                if dataset.policy.retention(kind) == 0 {
                    continue;
                }
                let newest = snapshots
                    .iter()
                    .filter(|snapshot| {
                        snapshot_kind(&config.settings.snapshot_prefix, &snapshot.name)
                            == Some(kind)
                    })
                    .map(|snapshot| snapshot.created)
                    .max();
                if snapshot_due(kind, &dataset.policy, newest, now, config.settings.timezone)? {
                    kinds.push(kind);
                    names.push(format!(
                        "{}_{}_{}",
                        config.settings.snapshot_prefix,
                        timestamp,
                        kind.as_str()
                    ));
                }
            }
            if !names.is_empty() {
                plan.snapshots.push(SnapshotAction {
                    pool: dataset.pool.clone(),
                    dataset: dataset.name.clone(),
                    recursive: dataset.snapshot_strategy == SnapshotStrategy::RecursiveRoot,
                    names,
                    kinds,
                    policy: dataset.policy.clone(),
                });
            }
        }

        if !dataset.policy.autoprune {
            continue;
        }
        let capacity = inventory
            .pools
            .get(&dataset.pool)
            .with_context(|| {
                format!(
                    "pool {:?} is missing from the zpool inventory",
                    dataset.pool
                )
            })?
            .capacity_percent;
        if dataset.policy.prune_defer > 0 && capacity < dataset.policy.prune_defer {
            plan.deferred_prune_datasets.push(dataset.name.clone());
            continue;
        }

        let mut names = Vec::new();
        for kind in SnapshotKind::ALL {
            let keep = dataset.policy.retention(kind) as usize;
            for snapshot in snapshots.iter().filter(|snapshot| {
                snapshot_kind(&config.settings.snapshot_prefix, &snapshot.name) == Some(kind)
                    && snapshot.managed
                    && snapshot.prune_protected()
            }) {
                plan.protected_snapshots.push(ProtectedSnapshot {
                    pool: dataset.pool.clone(),
                    dataset: dataset.name.clone(),
                    name: snapshot.name.clone(),
                    user_holds: snapshot.user_holds,
                    has_clones: snapshot.has_clones,
                });
            }
            let mut matching: Vec<_> = snapshots
                .iter()
                .filter(|snapshot| {
                    snapshot_kind(&config.settings.snapshot_prefix, &snapshot.name) == Some(kind)
                        && snapshot.managed
                        && !snapshot.prune_protected()
                })
                .copied()
                .collect();
            matching.sort_by_key(|snapshot| (snapshot.created, &snapshot.name));
            let cutoff = now.timestamp()
                - kind.approximate_period_seconds(dataset.policy.frequent_period) * keep as i64;
            let mut remaining = matching.len();
            for snapshot in matching {
                if remaining > keep && snapshot.created < cutoff {
                    names.push(snapshot.name.clone());
                    remaining -= 1;
                }
            }
        }
        names.sort();
        if !names.is_empty() {
            plan.prunes.push(PruneAction {
                pool: dataset.pool.clone(),
                dataset: dataset.name.clone(),
                names,
                policy: dataset.policy.clone(),
            });
        }
    }
    plan.snapshots
        .sort_by(|a, b| (&a.pool, &a.dataset).cmp(&(&b.pool, &b.dataset)));
    plan.prunes
        .sort_by(|a, b| (&a.pool, &a.dataset).cmp(&(&b.pool, &b.dataset)));
    plan.deferred_prune_datasets.sort();
    plan.protected_snapshots.sort_by(|left, right| {
        (&left.pool, &left.dataset, &left.name).cmp(&(&right.pool, &right.dataset, &right.name))
    });
    Ok(plan)
}

fn snapshot_timestamp(timezone: ScheduleTimezone, now: DateTime<Utc>) -> String {
    match timezone {
        ScheduleTimezone::Local => snapshot_timestamp_in(&Local, now),
        ScheduleTimezone::Utc => snapshot_timestamp_in(&Utc, now),
    }
}

fn snapshot_timestamp_in<Tz: TimeZone>(timezone: &Tz, now: DateTime<Utc>) -> String {
    let local = now.with_timezone(timezone);
    let naive = local.naive_local();
    let mut timestamp = naive.format("%Y-%m-%d_%H:%M:%S").to_string();
    if matches!(
        map_local_to_utc(timezone, naive),
        LocalResult::Ambiguous(_, second) if second.timestamp() == now.timestamp()
    ) {
        // Match Sanoid's collision suffix during the repeated daylight-saving hour.
        timestamp.push_str("dst");
    }
    timestamp
}

pub fn snapshot_kind(prefix: &str, name: &str) -> Option<SnapshotKind> {
    if !name.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
    }) {
        return None;
    }
    let remainder = name.strip_prefix(prefix)?.strip_prefix('_')?;
    for kind in SnapshotKind::ALL {
        let Some(timestamp) = remainder.strip_suffix(&format!("_{}", kind.as_str())) else {
            continue;
        };
        // Sanoid can append "dst" directly to the timestamp during the repeated
        // daylight-saving hour. UTC-native zsnap never generates this form, but
        // accepts it when assessing compatible history.
        let timestamp = timestamp
            .strip_suffix("_dst")
            .or_else(|| timestamp.strip_suffix("dst"))
            .unwrap_or(timestamp);
        if NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d_%H:%M:%S").is_ok() {
            return Some(kind);
        }
    }
    None
}

fn snapshot_due(
    kind: SnapshotKind,
    policy: &Policy,
    newest: Option<i64>,
    now: DateTime<Utc>,
    timezone: ScheduleTimezone,
) -> Result<bool> {
    let preferred = most_recent_preferred(kind, policy, now, timezone)?;
    Ok(newest.is_none_or(|created| created < preferred.timestamp()))
}

fn most_recent_preferred(
    kind: SnapshotKind,
    policy: &Policy,
    now: DateTime<Utc>,
    timezone: ScheduleTimezone,
) -> Result<DateTime<Utc>> {
    match timezone {
        ScheduleTimezone::Local => most_recent_preferred_in(kind, policy, now, &Local),
        ScheduleTimezone::Utc => most_recent_preferred_in(kind, policy, now, &Utc),
    }
}

fn most_recent_preferred_in<Tz: TimeZone>(
    kind: SnapshotKind,
    policy: &Policy,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> Result<DateTime<Utc>> {
    let civil_now = now.with_timezone(timezone).naive_local();
    let mut candidate = match kind {
        SnapshotKind::Frequently => {
            let minute = civil_now.minute() / policy.frequent_period * policy.frequent_period;
            naive_datetime(civil_now.date(), civil_now.hour(), minute)?
        }
        SnapshotKind::Hourly => {
            naive_datetime(civil_now.date(), civil_now.hour(), policy.hourly_min)?
        }
        SnapshotKind::Daily => {
            naive_datetime(civil_now.date(), policy.daily_hour, policy.daily_min)?
        }
        SnapshotKind::Weekly => {
            let wanted = weekday_from_iso(policy.weekly_wday)?;
            let current = civil_now.weekday().num_days_from_monday() as i64;
            let target = wanted.num_days_from_monday() as i64;
            let days_back = (current - target).rem_euclid(7);
            let date = civil_now.date() - Duration::days(days_back);
            naive_datetime(date, policy.weekly_hour, policy.weekly_min)?
        }
        SnapshotKind::Monthly => {
            let date =
                NaiveDate::from_ymd_opt(civil_now.year(), civil_now.month(), policy.monthly_mday)
                    .context("invalid monthly schedule date")?;
            naive_datetime(date, policy.monthly_hour, policy.monthly_min)?
        }
        SnapshotKind::Yearly => {
            let date = valid_yearly_date(civil_now.year(), policy.yearly_mon, policy.yearly_mday)?;
            naive_datetime(date, policy.yearly_hour, policy.yearly_min)?
        }
    };

    for _ in 0..4 {
        if let Some(preferred) = resolve_boundary(timezone, candidate, now)? {
            return Ok(preferred);
        }
        candidate = previous_preferred(kind, candidate, policy)?;
    }
    bail!("could not resolve a recent preferred {kind} boundary in the configured timezone")
}

fn previous_preferred(
    kind: SnapshotKind,
    candidate: NaiveDateTime,
    policy: &Policy,
) -> Result<NaiveDateTime> {
    match kind {
        SnapshotKind::Frequently => {
            Ok(candidate - Duration::minutes(policy.frequent_period.into()))
        }
        SnapshotKind::Hourly => Ok(candidate - Duration::hours(1)),
        SnapshotKind::Daily => Ok(candidate - Duration::days(1)),
        SnapshotKind::Weekly => Ok(candidate - Duration::weeks(1)),
        SnapshotKind::Monthly => {
            let (year, month) = previous_month(candidate.year(), candidate.month());
            let date = NaiveDate::from_ymd_opt(year, month, policy.monthly_mday)
                .context("invalid previous monthly schedule date")?;
            naive_datetime(date, policy.monthly_hour, policy.monthly_min)
        }
        SnapshotKind::Yearly => {
            let date =
                valid_yearly_date(candidate.year() - 1, policy.yearly_mon, policy.yearly_mday)?;
            naive_datetime(date, policy.yearly_hour, policy.yearly_min)
        }
    }
}

fn naive_datetime(date: NaiveDate, hour: u32, minute: u32) -> Result<NaiveDateTime> {
    date.and_hms_opt(hour, minute, 0)
        .context("invalid schedule time")
}

fn resolve_boundary<Tz: TimeZone>(
    timezone: &Tz,
    candidate: NaiveDateTime,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let mut mapped = map_local_to_utc(timezone, candidate);
    if matches!(mapped, LocalResult::None) {
        let normalized = normalize_nonexistent_time(timezone, candidate)?;
        mapped = map_local_to_utc(timezone, normalized);
    }
    Ok(latest_not_after(mapped, now))
}

fn map_local_to_utc<Tz: TimeZone>(
    timezone: &Tz,
    candidate: NaiveDateTime,
) -> LocalResult<DateTime<Utc>> {
    match timezone.from_local_datetime(&candidate) {
        LocalResult::Single(value) => LocalResult::Single(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, second) => {
            LocalResult::Ambiguous(first.with_timezone(&Utc), second.with_timezone(&Utc))
        }
        LocalResult::None => LocalResult::None,
    }
}

fn latest_not_after(
    mapped: LocalResult<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match mapped {
        LocalResult::Single(value) => (value <= now).then_some(value),
        LocalResult::Ambiguous(first, second) => [first, second]
            .into_iter()
            .filter(|value| *value <= now)
            .max(),
        LocalResult::None => None,
    }
}

fn normalize_nonexistent_time<Tz: TimeZone>(
    timezone: &Tz,
    candidate: NaiveDateTime,
) -> Result<NaiveDateTime> {
    let mut before = None;
    let mut after = None;
    for minutes in 1..=180 {
        let distance = Duration::minutes(minutes);
        if before.is_none() {
            let value = candidate - distance;
            if !matches!(map_local_to_utc(timezone, value), LocalResult::None) {
                before = Some(value);
            }
        }
        if after.is_none() {
            let value = candidate + distance;
            if !matches!(map_local_to_utc(timezone, value), LocalResult::None) {
                after = Some(value);
            }
        }
        if before.is_some() && after.is_some() {
            break;
        }
    }
    let before =
        before.context("could not find a valid local time before a timezone transition")?;
    let after = after.context("could not find a valid local time after a timezone transition")?;
    let gap = after - before - Duration::minutes(1);
    if gap <= Duration::zero() {
        bail!("could not determine the local timezone transition gap")
    }
    Ok(candidate + gap)
}

fn weekday_from_iso(day: u32) -> Result<Weekday> {
    match day {
        1 => Ok(Weekday::Mon),
        2 => Ok(Weekday::Tue),
        3 => Ok(Weekday::Wed),
        4 => Ok(Weekday::Thu),
        5 => Ok(Weekday::Fri),
        6 => Ok(Weekday::Sat),
        7 => Ok(Weekday::Sun),
        _ => bail!("weekly_wday must be 1..7"),
    }
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn valid_yearly_date(year: i32, month: u32, day: u32) -> Result<NaiveDate> {
    if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
        return Ok(date);
    }
    if month == 2 && day == 29 {
        return NaiveDate::from_ymd_opt(year, 2, 28).context("invalid yearly schedule date");
    }
    bail!("invalid yearly schedule date {year:04}-{month:02}-{day:02}")
}

fn dataset_depth(name: &str) -> usize {
    name.bytes().filter(|byte| *byte == b'/').count()
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone};

    use super::*;
    use crate::config::{DatasetConfig, Notifications, PolicyPatch, Recursion, Settings};
    use crate::zfs::Pool;

    fn test_config() -> Config {
        let mut templates = BTreeMap::new();
        templates.insert(
            "production".to_owned(),
            PolicyPatch {
                frequently: Some(0),
                hourly: Some(2),
                daily: Some(0),
                weekly: Some(0),
                monthly: Some(0),
                yearly: Some(0),
                ..PolicyPatch::default()
            },
        );
        let mut datasets = BTreeMap::new();
        datasets.insert(
            "tank/data".to_owned(),
            DatasetConfig {
                use_templates: vec!["production".to_owned()],
                recursive: Recursion::Children,
                process_children_only: false,
                policy: PolicyPatch::default(),
            },
        );
        Config {
            version: 1,
            settings: Settings {
                timezone: ScheduleTimezone::Utc,
                ..Settings::default()
            },
            notifications: Notifications::default(),
            templates,
            datasets,
        }
    }

    fn test_inventory() -> Inventory {
        Inventory {
            datasets: ["tank", "tank/data", "tank/data/db"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            snapshots: Vec::new(),
            pools: [(
                "tank".to_owned(),
                Pool {
                    name: "tank".to_owned(),
                    capacity_percent: 50,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn recursive_policy_expands_to_children() {
        let resolved = resolve_datasets(&test_config(), &test_inventory()).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|entry| entry.policy.hourly == 2));
        assert!(
            resolved
                .iter()
                .all(|entry| entry.snapshot_strategy == SnapshotStrategy::Individual)
        );
    }

    #[test]
    fn schedules_only_after_preferred_boundary() {
        let config = test_config();
        let mut inventory = test_inventory();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 10, 0).unwrap();
        inventory.snapshots.push(Snapshot {
            dataset: "tank/data".to_owned(),
            name: "autosnap_2026-08-27_12:05:00_hourly".to_owned(),
            created: Utc
                .with_ymd_and_hms(2026, 8, 27, 12, 5, 0)
                .unwrap()
                .timestamp(),
            managed: true,
            user_holds: 0,
            has_clones: false,
        });
        let resolved = resolve_datasets(&config, &inventory).unwrap();
        let plan = build_plan(&config, &inventory, &resolved, now).unwrap();
        assert_eq!(plan.snapshot_count(), 1); // Child is missing an hourly; parent is current.
        assert_eq!(plan.snapshots[0].dataset, "tank/data/db");
        assert_eq!(
            plan.snapshots[0].names[0],
            "autosnap_2026-08-27_12:10:00_hourly"
        );
    }

    #[test]
    fn civil_schedule_and_names_follow_the_selected_offset() {
        let timezone = FixedOffset::west_opt(4 * 60 * 60).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 10, 0).unwrap();
        let policy = Policy {
            hourly_min: 5,
            ..Policy::default()
        };
        let preferred =
            most_recent_preferred_in(SnapshotKind::Hourly, &policy, now, &timezone).unwrap();
        assert_eq!(
            preferred,
            Utc.with_ymd_and_hms(2026, 8, 27, 12, 5, 0).unwrap()
        );
        assert_eq!(snapshot_timestamp_in(&timezone, now), "2026-08-27_08:10:00");
    }

    #[test]
    fn ambiguous_boundaries_select_the_latest_occurrence_not_after_now() {
        let first = Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap();
        let second = Utc.with_ymd_and_hms(2026, 11, 1, 6, 30, 0).unwrap();
        assert_eq!(
            latest_not_after(
                LocalResult::Ambiguous(first, second),
                Utc.with_ymd_and_hms(2026, 11, 1, 5, 45, 0).unwrap(),
            ),
            Some(first)
        );
        assert_eq!(
            latest_not_after(
                LocalResult::Ambiguous(first, second),
                Utc.with_ymd_and_hms(2026, 11, 1, 6, 45, 0).unwrap(),
            ),
            Some(second)
        );
    }

    #[test]
    fn prune_requires_both_age_and_more_than_minimum() {
        let config = test_config();
        let mut inventory = test_inventory();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 10, 0).unwrap();
        for (hours_ago, hour) in [(5, 5), (4, 6), (3, 7), (1, 9)] {
            inventory.snapshots.push(Snapshot {
                dataset: "tank/data".to_owned(),
                name: format!("autosnap_2026-08-27_{hour:02}:00:00_hourly"),
                created: (now - Duration::hours(hours_ago)).timestamp(),
                managed: true,
                user_holds: 0,
                has_clones: false,
            });
        }
        let resolved = resolve_datasets(&config, &inventory).unwrap();
        let plan = build_plan(&config, &inventory, &resolved, now).unwrap();
        assert_eq!(plan.prune_count(), 2);
    }

    #[test]
    fn unmanaged_compatible_snapshots_are_never_pruned() {
        let config = test_config();
        let mut inventory = test_inventory();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 10, 0).unwrap();
        for (hours_ago, hour) in [(10, 2), (9, 3), (8, 4)] {
            inventory.snapshots.push(Snapshot {
                dataset: "tank/data".to_owned(),
                name: format!("autosnap_2026-08-27_{hour:02}:00:00_hourly"),
                created: (now - Duration::hours(hours_ago)).timestamp(),
                managed: false,
                user_holds: 0,
                has_clones: false,
            });
        }
        let resolved = resolve_datasets(&config, &inventory).unwrap();
        let plan = build_plan(&config, &inventory, &resolved, now).unwrap();
        assert_eq!(plan.prune_count(), 0);
    }

    #[test]
    fn held_and_cloned_snapshots_are_never_pruned() {
        let config = test_config();
        let mut inventory = test_inventory();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 10, 0).unwrap();
        for (hours_ago, hour) in [(8, 4), (7, 5), (1, 11)] {
            inventory.snapshots.push(Snapshot {
                dataset: "tank/data".to_owned(),
                name: format!("autosnap_2026-08-27_{hour:02}:00:00_hourly"),
                created: (now - Duration::hours(hours_ago)).timestamp(),
                managed: true,
                user_holds: 0,
                has_clones: false,
            });
        }
        inventory.snapshots.push(Snapshot {
            dataset: "tank/data".to_owned(),
            name: "autosnap_2026-08-27_02:00:00_hourly".to_owned(),
            created: (now - Duration::hours(10)).timestamp(),
            managed: true,
            user_holds: 1,
            has_clones: false,
        });
        inventory.snapshots.push(Snapshot {
            dataset: "tank/data".to_owned(),
            name: "autosnap_2026-08-27_03:00:00_hourly".to_owned(),
            created: (now - Duration::hours(9)).timestamp(),
            managed: true,
            user_holds: 0,
            has_clones: true,
        });

        let resolved = resolve_datasets(&config, &inventory).unwrap();
        let plan = build_plan(&config, &inventory, &resolved, now).unwrap();

        assert_eq!(plan.prune_count(), 1);
        assert_eq!(plan.protected_snapshot_count(), 2);
        assert_eq!(
            plan.prunes[0].names,
            ["autosnap_2026-08-27_04:00:00_hourly"]
        );
        assert!(plan.protected_snapshots[0].user_holds > 0);
        assert!(plan.protected_snapshots[1].has_clones);
    }

    #[test]
    fn rejects_unsafe_or_lookalike_snapshot_names() {
        assert_eq!(
            snapshot_kind("autosnap", "autosnap_2026-08-27_12:00:00_hourly"),
            Some(SnapshotKind::Hourly)
        );
        assert_eq!(
            snapshot_kind("autosnap", "autosnap_2026-08-27_01:00:00dst_hourly"),
            Some(SnapshotKind::Hourly)
        );
        assert_eq!(snapshot_kind("autosnap", "autosnap_latest_hourly"), None);
        assert_eq!(
            snapshot_kind("autosnap", "autosnap_2026-08-27_12:00:00,manual_hourly"),
            None
        );
    }
}
