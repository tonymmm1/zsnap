use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer};

use crate::model::SnapshotKind;

const DEFAULT_PREFIX: &str = "autosnap";
const DEFAULT_LOCK_FILE: &str = "/run/zsnap/zsnap.lock";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub templates: BTreeMap<String, PolicyPatch>,
    pub datasets: BTreeMap<String, DatasetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub snapshot_prefix: String,
    pub max_parallel_pools: usize,
    pub snapshot_batch_size: usize,
    pub prune_batch_size: usize,
    pub lock_file: PathBuf,
    pub zfs_command: PathBuf,
    pub zpool_command: PathBuf,
    pub prune_sanoid_snapshots: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            snapshot_prefix: DEFAULT_PREFIX.to_owned(),
            max_parallel_pools: 0,
            snapshot_batch_size: 128,
            prune_batch_size: 64,
            lock_file: PathBuf::from(DEFAULT_LOCK_FILE),
            zfs_command: PathBuf::from("zfs"),
            zpool_command: PathBuf::from("zpool"),
            prune_sanoid_snapshots: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetConfig {
    #[serde(default, alias = "use_template")]
    pub use_templates: Vec<String>,
    #[serde(default)]
    pub recursive: Recursion,
    #[serde(default)]
    pub process_children_only: bool,
    #[serde(default)]
    pub policy: PolicyPatch,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Recursion {
    #[default]
    None,
    Children,
    Zfs,
}

impl<'de> Deserialize<'de> for Recursion {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Bool(bool),
            String(String),
        }

        match Value::deserialize(deserializer)? {
            Value::Bool(false) => Ok(Self::None),
            Value::Bool(true) => Ok(Self::Children),
            Value::String(value) => match value.to_ascii_lowercase().as_str() {
                "none" | "no" | "false" => Ok(Self::None),
                "children" | "yes" | "true" => Ok(Self::Children),
                "zfs" | "atomic" => Ok(Self::Zfs),
                _ => Err(serde::de::Error::custom(format!(
                    "invalid recursion mode {value:?}; use false, true, \"children\", or \"zfs\""
                ))),
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyPatch {
    pub autosnap: Option<bool>,
    pub autoprune: Option<bool>,
    pub frequently: Option<u32>,
    pub hourly: Option<u32>,
    pub daily: Option<u32>,
    pub weekly: Option<u32>,
    pub monthly: Option<u32>,
    pub yearly: Option<u32>,
    pub frequent_period: Option<u32>,
    pub hourly_min: Option<u32>,
    pub daily_hour: Option<u32>,
    pub daily_min: Option<u32>,
    pub weekly_wday: Option<u32>,
    pub weekly_hour: Option<u32>,
    pub weekly_min: Option<u32>,
    pub monthly_mday: Option<u32>,
    pub monthly_hour: Option<u32>,
    pub monthly_min: Option<u32>,
    pub yearly_mon: Option<u32>,
    pub yearly_mday: Option<u32>,
    pub yearly_hour: Option<u32>,
    pub yearly_min: Option<u32>,
    pub prune_defer: Option<u8>,
    pub pre_snapshot_script: Option<Vec<String>>,
    pub post_snapshot_script: Option<Vec<String>>,
    pub pre_pruning_script: Option<Vec<String>>,
    pub pruning_script: Option<Vec<String>>,
    pub script_timeout: Option<u64>,
    pub no_inconsistent_snapshot: Option<bool>,
    pub force_post_snapshot_script: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub autosnap: bool,
    pub autoprune: bool,
    pub frequently: u32,
    pub hourly: u32,
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
    pub yearly: u32,
    pub frequent_period: u32,
    pub hourly_min: u32,
    pub daily_hour: u32,
    pub daily_min: u32,
    pub weekly_wday: u32,
    pub weekly_hour: u32,
    pub weekly_min: u32,
    pub monthly_mday: u32,
    pub monthly_hour: u32,
    pub monthly_min: u32,
    pub yearly_mon: u32,
    pub yearly_mday: u32,
    pub yearly_hour: u32,
    pub yearly_min: u32,
    pub prune_defer: u8,
    pub pre_snapshot_script: Vec<String>,
    pub post_snapshot_script: Vec<String>,
    pub pre_pruning_script: Vec<String>,
    pub pruning_script: Vec<String>,
    pub script_timeout: u64,
    pub no_inconsistent_snapshot: bool,
    pub force_post_snapshot_script: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            autosnap: true,
            autoprune: true,
            frequently: 0,
            hourly: 48,
            daily: 90,
            weekly: 0,
            monthly: 6,
            yearly: 0,
            frequent_period: 15,
            hourly_min: 0,
            daily_hour: 23,
            daily_min: 59,
            weekly_wday: 1,
            weekly_hour: 23,
            weekly_min: 30,
            monthly_mday: 1,
            monthly_hour: 0,
            monthly_min: 0,
            yearly_mon: 1,
            yearly_mday: 1,
            yearly_hour: 0,
            yearly_min: 0,
            prune_defer: 0,
            pre_snapshot_script: Vec::new(),
            post_snapshot_script: Vec::new(),
            pre_pruning_script: Vec::new(),
            pruning_script: Vec::new(),
            script_timeout: 5,
            no_inconsistent_snapshot: true,
            force_post_snapshot_script: false,
        }
    }
}

impl Policy {
    pub fn apply(&mut self, patch: &PolicyPatch) {
        macro_rules! apply_copy {
            ($($field:ident),+ $(,)?) => {
                $(if let Some(value) = patch.$field { self.$field = value; })+
            };
        }
        macro_rules! apply_clone {
            ($($field:ident),+ $(,)?) => {
                $(if let Some(value) = &patch.$field { self.$field = value.clone(); })+
            };
        }

        apply_copy!(
            autosnap,
            autoprune,
            frequently,
            hourly,
            daily,
            weekly,
            monthly,
            yearly,
            frequent_period,
            hourly_min,
            daily_hour,
            daily_min,
            weekly_wday,
            weekly_hour,
            weekly_min,
            monthly_mday,
            monthly_hour,
            monthly_min,
            yearly_mon,
            yearly_mday,
            yearly_hour,
            yearly_min,
            prune_defer,
            script_timeout,
            no_inconsistent_snapshot,
            force_post_snapshot_script,
        );
        apply_clone!(
            pre_snapshot_script,
            post_snapshot_script,
            pre_pruning_script,
            pruning_script,
        );
    }

    pub const fn retention(&self, kind: SnapshotKind) -> u32 {
        match kind {
            SnapshotKind::Frequently => self.frequently,
            SnapshotKind::Hourly => self.hourly,
            SnapshotKind::Daily => self.daily,
            SnapshotKind::Weekly => self.weekly,
            SnapshotKind::Monthly => self.monthly,
            SnapshotKind::Yearly => self.yearly,
        }
    }

    pub fn has_snapshot_hooks(&self) -> bool {
        !self.pre_snapshot_script.is_empty() || !self.post_snapshot_script.is_empty()
    }

    pub fn has_prune_hooks(&self) -> bool {
        !self.pre_pruning_script.is_empty() || !self.pruning_script.is_empty()
    }

    pub fn validate(&self, context: &str) -> Result<()> {
        ensure!(
            (1..=60).contains(&self.frequent_period) && 60 % self.frequent_period == 0,
            "{context}: frequent_period must be a divisor of 60 between 1 and 60"
        );
        ensure!(self.hourly_min <= 59, "{context}: hourly_min must be 0..59");
        ensure!(self.daily_hour <= 23, "{context}: daily_hour must be 0..23");
        ensure!(self.daily_min <= 59, "{context}: daily_min must be 0..59");
        ensure!(
            (1..=7).contains(&self.weekly_wday),
            "{context}: weekly_wday must be 1 (Monday) through 7 (Sunday)"
        );
        ensure!(
            self.weekly_hour <= 23,
            "{context}: weekly_hour must be 0..23"
        );
        ensure!(self.weekly_min <= 59, "{context}: weekly_min must be 0..59");
        ensure!(
            (1..=28).contains(&self.monthly_mday),
            "{context}: monthly_mday must be 1..28 so it exists in every month"
        );
        ensure!(
            self.monthly_hour <= 23,
            "{context}: monthly_hour must be 0..23"
        );
        ensure!(
            self.monthly_min <= 59,
            "{context}: monthly_min must be 0..59"
        );
        ensure!(
            (1..=12).contains(&self.yearly_mon),
            "{context}: yearly_mon must be 1..12"
        );
        ensure!(
            (1..=31).contains(&self.yearly_mday),
            "{context}: yearly_mday must be 1..31"
        );
        ensure!(
            self.yearly_hour <= 23,
            "{context}: yearly_hour must be 0..23"
        );
        ensure!(self.yearly_min <= 59, "{context}: yearly_min must be 0..59");
        ensure!(
            self.prune_defer <= 100,
            "{context}: prune_defer must be 0..100"
        );
        validate_hook(&self.pre_snapshot_script, context, "pre_snapshot_script")?;
        validate_hook(&self.post_snapshot_script, context, "post_snapshot_script")?;
        validate_hook(&self.pre_pruning_script, context, "pre_pruning_script")?;
        validate_hook(&self.pruning_script, context, "pruning_script")?;
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported configuration version {}; expected 1",
            self.version
        );
        ensure!(
            !self.datasets.is_empty(),
            "configuration must contain at least one dataset"
        );
        ensure!(
            self.settings.snapshot_batch_size > 0,
            "snapshot_batch_size must be greater than zero"
        );
        ensure!(
            self.settings.prune_batch_size > 0,
            "prune_batch_size must be greater than zero"
        );
        validate_prefix(&self.settings.snapshot_prefix)?;

        for (name, patch) in &self.templates {
            ensure!(!name.trim().is_empty(), "template names cannot be empty");
            let mut policy = Policy::default();
            policy.apply(patch);
            policy.validate(&format!("template {name:?}"))?;
        }

        for (dataset, section) in &self.datasets {
            validate_dataset_name(dataset)?;
            if section.process_children_only && section.recursive == Recursion::None {
                bail!("dataset {dataset:?}: process_children_only requires recursive = true");
            }
            if section.process_children_only && section.recursive == Recursion::Zfs {
                bail!(
                    "dataset {dataset:?}: process_children_only cannot be combined with recursive = \"zfs\""
                );
            }
            let mut policy = Policy::default();
            for template in &section.use_templates {
                let patch = self.templates.get(template).with_context(|| {
                    format!("dataset {dataset:?} references unknown template {template:?}")
                })?;
                policy.apply(patch);
            }
            policy.apply(&section.policy);
            policy.validate(&format!("dataset {dataset:?}"))?;
        }

        let atomic_roots: Vec<_> = self
            .datasets
            .iter()
            .filter(|(_, section)| section.recursive == Recursion::Zfs)
            .map(|(name, _)| name)
            .collect();
        for root in atomic_roots {
            let prefix = format!("{root}/");
            if let Some(child) = self.datasets.keys().find(|name| name.starts_with(&prefix)) {
                bail!(
                    "dataset {child:?} is nested beneath atomic recursive dataset {root:?}; use recursive = true on {root:?} to allow child overrides"
                );
            }
        }
        Ok(())
    }

    pub fn effective_policy(&self, dataset: &str, base: Option<&Policy>) -> Result<Policy> {
        let section = self
            .datasets
            .get(dataset)
            .with_context(|| format!("missing dataset section {dataset:?}"))?;
        let mut policy = base.cloned().unwrap_or_default();
        for template in &section.use_templates {
            policy.apply(&self.templates[template]);
        }
        policy.apply(&section.policy);
        Ok(policy)
    }
}

fn validate_prefix(prefix: &str) -> Result<()> {
    ensure!(!prefix.is_empty(), "snapshot_prefix cannot be empty");
    ensure!(
        prefix.chars().all(is_safe_name_character),
        "snapshot_prefix may contain only ASCII letters, numbers, '.', '-', '_', and ':'"
    );
    Ok(())
}

fn validate_dataset_name(name: &str) -> Result<()> {
    ensure!(
        !name.starts_with('/') && !name.ends_with('/'),
        "invalid ZFS dataset name {name:?}"
    );
    ensure!(
        name.contains('/') || !name.is_empty(),
        "invalid ZFS dataset name {name:?}"
    );
    ensure!(
        name.split('/')
            .all(|part| !part.is_empty() && part.chars().all(is_safe_name_character)),
        "dataset {name:?} contains unsupported characters"
    );
    Ok(())
}

fn is_safe_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
}

fn validate_hook(command: &[String], context: &str, field: &str) -> Result<()> {
    if command.is_empty() {
        return Ok(());
    }
    ensure!(
        !command[0].trim().is_empty(),
        "{context}: {field} executable cannot be empty"
    );
    ensure!(
        command.iter().all(|part| !part.contains('\0')),
        "{context}: {field} cannot contain NUL bytes"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_toml_and_merges_templates_in_order() {
        let config: Config = toml::from_str(
            r#"
version = 1

[templates.base]
hourly = 24
daily = 7

[templates.archive]
hourly = 0
monthly = 12

[datasets."tank/data"]
use_templates = ["base", "archive"]
recursive = true

[datasets."tank/data".policy]
daily = 30
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let policy = config.effective_policy("tank/data", None).unwrap();
        assert_eq!(policy.hourly, 0);
        assert_eq!(policy.daily, 30);
        assert_eq!(policy.monthly, 12);
        assert_eq!(config.datasets["tank/data"].recursive, Recursion::Children);
    }

    #[test]
    fn rejects_child_override_under_atomic_recursion() {
        let config: Config = toml::from_str(
            r#"
version = 1
[datasets."tank/data"]
recursive = "zfs"
[datasets."tank/data/db"]
"#,
        )
        .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("atomic recursive")
        );
    }
}
