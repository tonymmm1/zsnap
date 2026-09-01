use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::SnapshotKind;

const DEFAULT_PREFIX: &str = "autosnap";
const DEFAULT_LOCK_FILE: &str = "/run/zsnap/zsnap.lock";
const MAX_SNAPSHOT_BATCH_SIZE: usize = 256;
const MAX_PRUNE_BATCH_SIZE: usize = 128;
const POLICY_KEYS: &[&str] = &[
    "autosnap",
    "autoprune",
    "frequently",
    "hourly",
    "daily",
    "weekly",
    "monthly",
    "yearly",
    "frequent_period",
    "hourly_min",
    "daily_hour",
    "daily_min",
    "weekly_wday",
    "weekly_hour",
    "weekly_min",
    "monthly_mday",
    "monthly_hour",
    "monthly_min",
    "yearly_mon",
    "yearly_mday",
    "yearly_hour",
    "yearly_min",
    "prune_defer",
    "pre_snapshot_script",
    "post_snapshot_script",
    "pre_pruning_script",
    "pruning_script",
    "script_timeout",
    "no_inconsistent_snapshot",
    "force_post_snapshot_script",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub notifications: Notifications,
    #[serde(default)]
    pub templates: BTreeMap<String, PolicyPatch>,
    #[serde(default)]
    pub datasets: BTreeMap<String, DatasetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
    pub hostname: Option<String>,
    pub max_parallel: usize,
    pub timeout_seconds: u64,
    pub max_attempts: u32,
    pub retry_backoff_milliseconds: u64,
    pub fail_on_error: bool,
    pub webhooks: Vec<WebhookConfig>,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            hostname: None,
            max_parallel: 4,
            timeout_seconds: 10,
            max_attempts: 3,
            retry_backoff_milliseconds: 500,
            fail_on_error: false,
            webhooks: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub name: String,
    pub kind: WebhookKind,
    #[serde(default)]
    pub url: Option<SecretString>,
    #[serde(default)]
    pub url_env: Option<String>,
    #[serde(default = "default_webhook_events")]
    pub events: Vec<WebhookEvent>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl fmt::Debug for WebhookConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebhookConfig")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("url", &self.url)
            .field("url_env", &self.url_env)
            .field("events", &self.events)
            .field("enabled", &self.enabled)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebhookKind {
    Discord,
    Flock,
    Slack,
}

impl fmt::Display for WebhookKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Discord => "discord",
            Self::Flock => "flock",
            Self::Slack => "slack",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum WebhookEvent {
    Failure,
    Success,
}

fn default_webhook_events() -> Vec<WebhookEvent> {
    vec![WebhookEvent::Failure]
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub snapshot_prefix: String,
    pub timezone: ScheduleTimezone,
    pub max_parallel_pools: usize,
    pub snapshot_batch_size: usize,
    pub prune_batch_size: usize,
    pub lock_file: PathBuf,
    pub zfs_command: PathBuf,
    pub zpool_command: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            snapshot_prefix: DEFAULT_PREFIX.to_owned(),
            timezone: ScheduleTimezone::Local,
            max_parallel_pools: 0,
            snapshot_batch_size: 128,
            prune_batch_size: 64,
            lock_file: PathBuf::from(DEFAULT_LOCK_FILE),
            zfs_command: PathBuf::from("zfs"),
            zpool_command: PathBuf::from("zpool"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScheduleTimezone {
    #[default]
    Local,
    Utc,
}

impl<'de> Deserialize<'de> for ScheduleTimezone {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "local" | "host" => Ok(Self::Local),
            "utc" => Ok(Self::Utc),
            _ => Err(serde::de::Error::custom(format!(
                "invalid timezone {value:?}; use \"local\" or \"utc\""
            ))),
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

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
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
        let config = Self::parse(&raw)
            .with_context(|| format!("failed to parse TOML configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let expanded = expand_config_shorthand(raw)?;
        let mut value: toml::Value = toml::from_str(&expanded)?;
        normalize_inline_dataset_policies(&mut value)?;
        value
            .try_into()
            .context("configuration does not match the zsnap schema")
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
        self.settings.validate()?;
        validate_prefix(&self.settings.snapshot_prefix)?;
        self.notifications.validate()?;

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

fn expand_config_shorthand(raw: &str) -> Result<String> {
    let mut expanded = String::with_capacity(raw.len());
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(header) = friendly_template_header(trimmed, index + 1)? {
            expanded.push_str(&header);
        } else if let Some(header) = friendly_dataset_header(trimmed, index + 1)? {
            expanded.push_str(&header);
        } else if let Some(template_list) = friendly_template_list(line, index + 1)? {
            expanded.push_str(&template_list);
        } else {
            expanded.push_str(line);
        }
        expanded.push('\n');
    }
    Ok(expanded)
}

fn friendly_template_list(line: &str, line_number: usize) -> Result<Option<String>> {
    let content = line.trim_start();
    let indentation = &line[..line.len() - content.len()];
    let Some((key, value)) = content.split_once('=') else {
        return Ok(None);
    };
    let key = key.trim();
    if !matches!(key, "use_templates" | "use_template") {
        return Ok(None);
    }
    let value = value.trim();
    if !value.starts_with('[') {
        return Ok(None);
    }
    let Some(closing) = value.find(']') else {
        return Ok(None);
    };
    let names = &value[1..closing];
    if names.contains('"') || names.contains('\'') {
        return Ok(None);
    }
    let trailing = value[closing + 1..].trim();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return Ok(None);
    }
    if names.trim().is_empty() {
        return Ok(None);
    }

    let mut quoted = Vec::new();
    for raw_name in names.split(',') {
        let name = raw_name.trim();
        if name.is_empty() || !name.chars().all(is_bare_template_character) {
            bail!(
                "line {line_number}: bare template names may contain only ASCII letters, numbers, '-' and '_'"
            );
        }
        quoted.push(toml::Value::String(name.to_owned()).to_string());
    }
    let comment = if trailing.is_empty() {
        String::new()
    } else {
        format!(" {trailing}")
    };
    Ok(Some(format!(
        "{indentation}{key} = [{}]{comment}",
        quoted.join(", ")
    )))
}

fn is_bare_template_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn friendly_template_header(line: &str, line_number: usize) -> Result<Option<String>> {
    if line.starts_with("[[") || !line.starts_with('[') {
        return Ok(None);
    }
    let Some(closing) = line.find(']') else {
        return Ok(None);
    };
    let raw_name = line[1..closing].trim();
    let Some(template_name) = raw_name.strip_prefix("template_") else {
        return Ok(None);
    };
    if template_name.contains('/') {
        return Ok(None);
    }
    let trailing = line[closing + 1..].trim();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return Ok(None);
    }
    if template_name.is_empty() || !template_name.chars().all(is_bare_template_character) {
        bail!(
            "line {line_number}: template headers must use [template_NAME], where NAME contains only ASCII letters, numbers, '-' and '_'"
        );
    }
    let quoted = toml::Value::String(template_name.to_owned()).to_string();
    let comment = if trailing.is_empty() {
        String::new()
    } else {
        format!(" {trailing}")
    };
    Ok(Some(format!("[templates.{quoted}]{comment}")))
}

fn friendly_dataset_header(line: &str, line_number: usize) -> Result<Option<String>> {
    if line.starts_with("[[") || !line.starts_with('[') {
        return Ok(None);
    }
    let Some(closing) = line.find(']') else {
        return Ok(None);
    };
    let raw_name = line[1..closing].trim();
    if is_reserved_config_header(raw_name) {
        return Ok(None);
    }
    let trailing = line[closing + 1..].trim();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return Ok(None);
    }

    let dataset = if raw_name.starts_with('"') || raw_name.starts_with('\'') {
        let probe = format!("dataset = {raw_name}");
        let value: toml::Value = toml::from_str(&probe)
            .with_context(|| format!("line {line_number}: invalid quoted dataset section name"))?;
        value
            .get("dataset")
            .and_then(toml::Value::as_str)
            .context("quoted dataset section name must be a string")?
            .to_owned()
    } else {
        raw_name.to_owned()
    };
    let quoted = toml::Value::String(dataset).to_string();
    let comment = if trailing.is_empty() {
        String::new()
    } else {
        format!(" {trailing}")
    };
    Ok(Some(format!("[datasets.{quoted}]{comment}")))
}

fn is_reserved_config_header(name: &str) -> bool {
    [
        "settings",
        "notifications",
        "templates",
        "datasets",
        "version",
    ]
    .iter()
    .any(|reserved| name == *reserved || name.starts_with(&format!("{reserved}.")))
}

fn normalize_inline_dataset_policies(value: &mut toml::Value) -> Result<()> {
    let Some(datasets) = value
        .as_table_mut()
        .and_then(|root| root.get_mut("datasets"))
    else {
        return Ok(());
    };
    let datasets = datasets
        .as_table_mut()
        .context("datasets must be a table")?;
    for (dataset_name, dataset_value) in datasets {
        let dataset = dataset_value
            .as_table_mut()
            .with_context(|| format!("dataset {dataset_name:?} must be a table"))?;
        let mut inline = Vec::new();
        for key in POLICY_KEYS {
            if let Some(value) = dataset.remove(*key) {
                inline.push(((*key).to_owned(), value));
            }
        }
        if inline.is_empty() {
            continue;
        }
        let policy = dataset
            .entry("policy")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .with_context(|| format!("dataset {dataset_name:?} policy must be a table"))?;
        for (key, value) in inline {
            if policy.insert(key.clone(), value).is_some() {
                bail!(
                    "dataset {dataset_name:?} defines policy setting {key:?} both inline and in its policy table"
                );
            }
        }
    }
    Ok(())
}

impl Settings {
    fn validate(&self) -> Result<()> {
        validate_batch_sizes(self.snapshot_batch_size, self.prune_batch_size, "settings")?;
        ensure_nonempty_path(&self.lock_file, "settings.lock_file")?;
        ensure_nonempty_path(&self.zfs_command, "settings.zfs_command")?;
        ensure_nonempty_path(&self.zpool_command, "settings.zpool_command")?;
        ensure!(
            self.lock_file.file_name().is_some(),
            "settings.lock_file must name a file, not a directory"
        );
        Ok(())
    }
}

fn ensure_nonempty_path(path: &Path, field: &str) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty() && !path.to_string_lossy().trim().is_empty(),
        "{field} cannot be empty"
    );
    Ok(())
}

fn validate_batch_sizes(snapshot: usize, prune: usize, context: &str) -> Result<()> {
    ensure!(
        (1..=MAX_SNAPSHOT_BATCH_SIZE).contains(&snapshot),
        "{context}: snapshot_batch_size must be between 1 and {MAX_SNAPSHOT_BATCH_SIZE}"
    );
    ensure!(
        (1..=MAX_PRUNE_BATCH_SIZE).contains(&prune),
        "{context}: prune_batch_size must be between 1 and {MAX_PRUNE_BATCH_SIZE}"
    );
    Ok(())
}

impl Notifications {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_parallel > 0,
            "notifications.max_parallel must be greater than zero"
        );
        ensure!(
            (1..=60).contains(&self.timeout_seconds),
            "notifications.timeout_seconds must be between 1 and 60"
        );
        ensure!(
            (1..=5).contains(&self.max_attempts),
            "notifications.max_attempts must be between 1 and 5"
        );
        ensure!(
            self.retry_backoff_milliseconds <= 10_000,
            "notifications.retry_backoff_milliseconds must not exceed 10000"
        );
        if let Some(hostname) = &self.hostname {
            ensure!(
                !hostname.trim().is_empty(),
                "notifications.hostname cannot be empty"
            );
            ensure!(
                !hostname.contains(['\n', '\r', '\0']),
                "notifications.hostname cannot contain line breaks or NUL bytes"
            );
        }

        let mut names = BTreeSet::new();
        for webhook in &self.webhooks {
            let context = format!("notification webhook {:?}", webhook.name);
            ensure!(
                !webhook.name.trim().is_empty(),
                "webhook names cannot be empty"
            );
            ensure!(
                !webhook.name.contains(['\n', '\r', '\0']),
                "{context}: name cannot contain line breaks or NUL bytes"
            );
            ensure!(
                names.insert(webhook.name.as_str()),
                "duplicate notification webhook name {:?}",
                webhook.name
            );
            ensure!(
                webhook.url.is_some() ^ webhook.url_env.is_some(),
                "{context}: configure exactly one of url or url_env"
            );
            ensure!(
                !webhook.events.is_empty(),
                "{context}: events cannot be empty"
            );
            let unique_events: BTreeSet<_> = webhook.events.iter().copied().collect();
            ensure!(
                unique_events.len() == webhook.events.len(),
                "{context}: events cannot contain duplicates"
            );
            if let Some(url) = &webhook.url {
                validate_webhook_url(url.expose(), &context)?;
            }
            if let Some(variable) = &webhook.url_env {
                ensure!(
                    is_portable_environment_name(variable),
                    "{context}: url_env must be a portable environment variable name"
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_webhook_url(url: &str, context: &str) -> Result<()> {
    let uri = url
        .parse::<ureq::http::Uri>()
        .map_err(|_| anyhow::anyhow!("{context}: invalid webhook URL"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| anyhow::anyhow!("{context}: webhook URL must include a scheme"))?;
    ensure!(
        uri.authority().is_some(),
        "{context}: webhook URL must include a host"
    );
    ensure!(scheme == "https", "{context}: webhook URL must use HTTPS");
    Ok(())
}

fn is_portable_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some('_' | 'A'..='Z' | 'a'..='z'))
        && characters.all(|character| matches!(character, '_' | 'A'..='Z' | 'a'..='z' | '0'..='9'))
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

    fn config_with_notifications(notification_toml: &str) -> String {
        format!(
            r#"
version = 1
{notification_toml}
[datasets."tank/data"]
"#
        )
    }

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
    fn parses_short_dataset_headers_and_inline_policy() {
        let config = Config::parse(
            r#"
version = 1

[template_base]
hourly = 24

[tank]
autosnap = false

[tank/data]
use_templates = [base]
recursive = true
daily = 7

["tank/archive"]
autosnap = false
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.datasets.len(), 3);
        assert_eq!(config.datasets["tank"].policy.autosnap, Some(false));
        assert_eq!(config.datasets["tank/data"].recursive, Recursion::Children);
        assert_eq!(config.datasets["tank/data"].use_templates, ["base"]);
        assert_eq!(config.datasets["tank/data"].policy.daily, Some(7));
        assert_eq!(config.datasets["tank/archive"].policy.autosnap, Some(false));

        let unknown = Config::parse("version = 1\n[tank/data]\nhourlies = 3\n")
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("schema"), "{unknown}");
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

    #[test]
    fn validates_global_batch_size_bounds() {
        let valid = config_with_notifications(
            r#"
[settings]
snapshot_batch_size = 256
prune_batch_size = 128
"#,
        );
        toml::from_str::<Config>(&valid)
            .unwrap()
            .validate()
            .unwrap();

        let zero = config_with_notifications(
            r#"
[settings]
snapshot_batch_size = 0
"#,
        );
        assert!(
            toml::from_str::<Config>(&zero)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string()
                .contains("snapshot_batch_size must be between 1 and 256")
        );

        let too_large = config_with_notifications(
            r#"
[settings]
prune_batch_size = 129
"#,
        );
        assert!(
            toml::from_str::<Config>(&too_large)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string()
                .contains("prune_batch_size must be between 1 and 128")
        );
    }

    #[test]
    fn rejects_empty_runtime_paths() {
        for (field, message) in [
            ("lock_file", "settings.lock_file"),
            ("zfs_command", "settings.zfs_command"),
            ("zpool_command", "settings.zpool_command"),
        ] {
            let raw = config_with_notifications(&format!("[settings]\n{field} = \"\"\n"));
            let error = toml::from_str::<Config>(&raw)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
        }
    }

    #[test]
    fn defaults_to_host_timezone_and_accepts_explicit_utc() {
        let local: Config = toml::from_str(&config_with_notifications("")).unwrap();
        local.validate().unwrap();
        assert_eq!(local.settings.timezone, ScheduleTimezone::Local);

        for value in ["utc", "UTC"] {
            let raw = config_with_notifications(&format!("[settings]\ntimezone = {value:?}\n"));
            let config: Config = toml::from_str(&raw).unwrap();
            config.validate().unwrap();
            assert_eq!(config.settings.timezone, ScheduleTimezone::Utc);
        }

        let invalid = config_with_notifications("[settings]\ntimezone = \"America/New_York\"\n");
        assert!(toml::from_str::<Config>(&invalid).is_err());
    }

    #[test]
    fn parses_typed_webhook_configuration_with_failure_default() {
        let raw = config_with_notifications(
            r#"
[notifications]
max_parallel = 3

[[notifications.webhooks]]
name = "operations"
kind = "flock"
url_env = "ZSNAP_FLOCK_WEBHOOK"
"#,
        );
        let config: Config = toml::from_str(&raw).unwrap();
        config.validate().unwrap();
        let webhook = &config.notifications.webhooks[0];
        assert_eq!(webhook.kind, WebhookKind::Flock);
        assert_eq!(webhook.events, vec![WebhookEvent::Failure]);
        assert_eq!(config.notifications.max_parallel, 3);
    }

    #[test]
    fn rejects_unknown_toml_keys_and_webhook_types() {
        let unknown_key = config_with_notifications(
            r#"
[notifications]
timeot_seconds = 10
"#,
        );
        assert!(toml::from_str::<Config>(&unknown_key).is_err());

        let unknown_kind = config_with_notifications(
            r#"
[[notifications.webhooks]]
name = "operations"
kind = "teams"
url = "https://example.invalid/hook"
"#,
        );
        assert!(toml::from_str::<Config>(&unknown_kind).is_err());
    }

    #[test]
    fn rejects_removed_unsafe_switches() {
        let unmanaged_pruning = config_with_notifications(
            r#"
[settings]
prune_sanoid_snapshots = true
"#,
        );
        assert!(toml::from_str::<Config>(&unmanaged_pruning).is_err());

        let insecure_http = config_with_notifications(
            r#"
[notifications]
allow_insecure_http = true
"#,
        );
        assert!(toml::from_str::<Config>(&insecure_http).is_err());
    }

    #[test]
    fn validates_webhook_url_sources_and_transport_security() {
        let both_sources = config_with_notifications(
            r#"
[[notifications.webhooks]]
name = "operations"
kind = "slack"
url = "https://example.invalid/hook"
url_env = "ZSNAP_SLACK_WEBHOOK"
"#,
        );
        let config: Config = toml::from_str(&both_sources).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );

        let insecure = config_with_notifications(
            r#"
[[notifications.webhooks]]
name = "operations"
kind = "discord"
url = "http://example.invalid/hook"
"#,
        );
        let config: Config = toml::from_str(&insecure).unwrap();
        assert!(config.validate().unwrap_err().to_string().contains("HTTPS"));
    }
}
