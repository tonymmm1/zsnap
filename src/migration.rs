use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::{Config, PolicyPatch};

#[derive(Debug, Clone, Serialize)]
pub struct SanoidMigration {
    pub config_toml: String,
    pub warnings: Vec<String>,
    pub datasets: usize,
    pub templates: usize,
}

#[derive(Debug, Clone)]
struct Entry {
    value: String,
    line: usize,
}

type Section = BTreeMap<String, Entry>;

#[derive(Debug, Default)]
struct SanoidConfig {
    sections: BTreeMap<String, Section>,
}

type OutputPolicy = PolicyPatch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum OutputRecursion {
    Children(bool),
    Zfs(String),
}

#[derive(Debug, Clone, Default, Serialize)]
struct OutputDataset {
    #[serde(skip)]
    use_templates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recursive: Option<OutputRecursion>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    process_children_only: bool,
    #[serde(skip)]
    process_children_only_override: Option<bool>,
    #[serde(flatten)]
    policy: OutputPolicy,
}

#[derive(Serialize)]
struct OutputSettings {
    snapshot_prefix: &'static str,
    timezone: &'static str,
}

#[derive(Serialize)]
struct OutputNotifications {
    enabled: bool,
}

struct OutputConfig {
    version: u32,
    settings: OutputSettings,
    notifications: OutputNotifications,
    templates: BTreeMap<String, OutputPolicy>,
    datasets: BTreeMap<String, OutputDataset>,
}

#[derive(Serialize)]
struct OutputHeader<'a> {
    version: u32,
    settings: &'a OutputSettings,
    notifications: &'a OutputNotifications,
}

pub fn convert_sanoid(source: &str, defaults: Option<&str>) -> Result<SanoidMigration> {
    let mut warnings = Vec::new();
    let source = parse_sanoid(source, "Sanoid configuration", &mut warnings)?;
    let defaults = defaults
        .map(|raw| parse_sanoid(raw, "Sanoid defaults", &mut warnings))
        .transpose()?;

    let mut base_policy = standard_sanoid_defaults();
    if let Some(section) = defaults
        .as_ref()
        .and_then(|config| config.sections.get("template_default"))
    {
        apply_policy(
            "template_default in Sanoid defaults",
            section,
            &mut base_policy,
            &mut warnings,
            false,
        )?;
    }
    let user_default_policy = if let Some(section) = source.sections.get("template_default") {
        let mut policy = OutputPolicy::default();
        apply_policy(
            "template_default",
            section,
            &mut policy,
            &mut warnings,
            true,
        )?;
        apply_policy(
            "template_default",
            section,
            &mut base_policy,
            &mut Vec::new(),
            false,
        )?;
        warn_ignored_structural("template_default", section, &mut warnings);
        Some(policy)
    } else {
        None
    };
    let default_process_children_only = source
        .sections
        .get("template_default")
        .and_then(|section| section.get("process_children_only"))
        .map(|entry| parse_bool("template_default", "process_children_only", entry))
        .transpose()?
        .unwrap_or(false);

    let named_template_names: BTreeSet<_> = source
        .sections
        .keys()
        .filter_map(|name| name.strip_prefix("template_"))
        .filter(|name| *name != "default")
        .map(str::to_owned)
        .collect();
    let base_name = unique_template_name("sanoid_defaults", &named_template_names);
    let mut reserved_template_names = named_template_names.clone();
    reserved_template_names.insert(base_name.clone());
    let user_default_name = user_default_policy
        .as_ref()
        .map(|_| unique_template_name("sanoid_user_defaults", &reserved_template_names));
    let mut templates = BTreeMap::new();
    templates.insert(base_name.clone(), base_policy);
    if let (Some(name), Some(policy)) = (&user_default_name, user_default_policy) {
        templates.insert(name.clone(), policy);
    }

    for (section_name, section) in &source.sections {
        let Some(template_name) = section_name.strip_prefix("template_") else {
            continue;
        };
        if template_name == "default" {
            continue;
        }
        if template_name.trim().is_empty() {
            bail!("Sanoid template section {section_name:?} has an empty name");
        }
        let mut policy = OutputPolicy::default();
        apply_policy(section_name, section, &mut policy, &mut warnings, true)?;
        warn_ignored_structural(section_name, section, &mut warnings);
        templates.insert(template_name.to_owned(), policy);
    }

    let mut datasets = BTreeMap::new();
    for (section_name, section) in &source.sections {
        if section_name.starts_with("template_") {
            continue;
        }
        if section_name == "version" {
            warnings.push("ignored Sanoid [version] metadata section".to_owned());
            continue;
        }

        let dataset_name = match section.get("path") {
            Some(entry) if entry.value.trim().is_empty() => bail!(
                "Sanoid section [{section_name}] has an empty path on line {}; remove path or set a dataset name",
                entry.line
            ),
            Some(entry) => entry.value.trim().to_owned(),
            None => section_name.to_owned(),
        };
        if datasets.contains_key(&dataset_name) {
            bail!(
                "multiple Sanoid sections resolve to dataset {dataset_name:?}; path aliases must be unique"
            );
        }

        let use_templates =
            parse_template_list(section_name, section, user_default_name.as_deref())?;
        for template in &use_templates {
            if user_default_name.as_deref() == Some(template) {
                continue;
            }
            if !named_template_names.contains(template) {
                bail!("Sanoid section [{section_name}] references missing template {template:?}");
            }
        }
        let recursive = parse_recursion(section_name, section.get("recursive"))?;
        let process_children_only_override = resolve_process_children_only(
            section_name,
            section,
            &source,
            &use_templates,
            user_default_name.as_deref(),
        )?;
        if section
            .get("skip_children")
            .map(|entry| parse_bool(section_name, "skip_children", entry))
            .transpose()?
            .unwrap_or(false)
        {
            bail!(
                "Sanoid section [{section_name}] uses skip_children = yes, which cannot be translated without changing subtree coverage"
            );
        }

        let mut policy = OutputPolicy::default();
        apply_policy(section_name, section, &mut policy, &mut warnings, true)?;
        datasets.insert(
            dataset_name,
            OutputDataset {
                use_templates,
                recursive,
                process_children_only: false,
                process_children_only_override,
                policy,
            },
        );
    }
    if datasets.is_empty() {
        bail!("Sanoid configuration does not contain any dataset sections");
    }

    apply_base_template(&base_name, default_process_children_only, &mut datasets)?;
    let output = OutputConfig {
        version: 1,
        settings: OutputSettings {
            snapshot_prefix: "autosnap",
            timezone: "local",
        },
        notifications: OutputNotifications { enabled: false },
        templates,
        datasets,
    };
    let datasets = output.datasets.len();
    let templates = output.templates.len();
    let body = render_output(&output)?;
    let config_toml = format!(
        "# Generated by `zsnap migrate-sanoid`.\n\
         # This conversion does not touch ZFS. Existing unmarked Sanoid snapshots are never pruned.\n\n\
         # Host-local scheduling preserves Sanoid's configured civil times.\n\n\
         {body}"
    );
    let parsed = Config::parse(&config_toml).context("generated zsnap TOML did not parse")?;
    parsed
        .validate()
        .context("generated zsnap configuration failed validation")?;

    Ok(SanoidMigration {
        config_toml,
        warnings,
        datasets,
        templates,
    })
}

fn render_output(output: &OutputConfig) -> Result<String> {
    let header = OutputHeader {
        version: output.version,
        settings: &output.settings,
        notifications: &output.notifications,
    };
    let mut rendered =
        toml::to_string_pretty(&header).context("failed to render migrated TOML header")?;
    for (template_name, policy) in &output.templates {
        rendered.push('\n');
        rendered.push_str(&render_template_header(template_name));
        rendered.push('\n');
        rendered.push_str(
            &toml::to_string_pretty(policy)
                .with_context(|| format!("failed to render template {template_name:?}"))?,
        );
    }
    for (dataset_name, dataset) in &output.datasets {
        rendered.push('\n');
        if can_use_friendly_dataset_header(dataset_name) {
            rendered.push_str(&format!("[{dataset_name}]\n"));
        } else {
            let quoted = toml::Value::String(dataset_name.clone()).to_string();
            rendered.push_str(&format!("[datasets.{quoted}]\n"));
        }
        if !dataset.use_templates.is_empty() {
            rendered.push_str(&format!(
                "use_templates = {}\n",
                render_template_list(&dataset.use_templates)
            ));
        }
        rendered.push_str(
            &toml::to_string_pretty(dataset)
                .with_context(|| format!("failed to render dataset {dataset_name:?}"))?,
        );
    }
    Ok(rendered)
}

fn render_template_header(name: &str) -> String {
    if !name.is_empty() && name.chars().all(is_bare_template_character) {
        format!("[template_{name}]")
    } else {
        let quoted = toml::Value::String(name.to_owned()).to_string();
        format!("[templates.{quoted}]")
    }
}

fn can_use_friendly_dataset_header(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with("template_")
        && !matches!(
            name,
            "settings" | "notifications" | "templates" | "datasets" | "version"
        )
        && name.split('/').all(|component| {
            !component.is_empty()
                && component.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
                })
        })
}

fn render_template_list(names: &[String]) -> String {
    let use_bare_names = names
        .iter()
        .all(|name| !name.is_empty() && name.chars().all(is_bare_template_character));
    let values = names
        .iter()
        .map(|name| {
            if use_bare_names {
                name.clone()
            } else {
                toml::Value::String(name.clone()).to_string()
            }
        })
        .collect::<Vec<_>>();
    format!("[{}]", values.join(", "))
}

fn is_bare_template_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn parse_sanoid(raw: &str, label: &str, warnings: &mut Vec<String>) -> Result<SanoidConfig> {
    if raw.contains('\0') {
        bail!("{label} contains a NUL byte");
    }
    let mut parsed = SanoidConfig::default();
    let mut current_section: Option<String> = None;
    for (index, original) in raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .lines()
        .enumerate()
    {
        let line_number = index + 1;
        let line = original.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let Some(closing) = rest.find(']') else {
                bail!("{label} line {line_number}: unterminated section header");
            };
            let name = rest[..closing].trim();
            let trailing = rest[closing + 1..].trim();
            if name.is_empty() {
                bail!("{label} line {line_number}: section name cannot be empty");
            }
            if !trailing.is_empty() && !trailing.starts_with('#') && !trailing.starts_with(';') {
                bail!("{label} line {line_number}: unexpected text after section header");
            }
            if parsed.sections.contains_key(name) {
                warnings.push(format!(
                    "{label} line {line_number}: merged duplicate section [{name}]"
                ));
            }
            parsed.sections.entry(name.to_owned()).or_default();
            current_section = Some(name.to_owned());
            continue;
        }

        let section_name = current_section.as_ref().with_context(|| {
            format!("{label} line {line_number}: setting appears before any section")
        })?;
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("{label} line {line_number}: expected `key = value`"))?;
        let key = key.trim();
        if key.is_empty() {
            bail!("{label} line {line_number}: setting name cannot be empty");
        }
        let section = parsed.sections.get_mut(section_name).unwrap();
        if let Some(first) = section.get(key) {
            warnings.push(format!(
                "{label} line {line_number}: ignored duplicate {key:?} in [{section_name}]; first defined on line {}",
                first.line
            ));
            continue;
        }
        section.insert(
            key.to_owned(),
            Entry {
                value: value.trim().to_owned(),
                line: line_number,
            },
        );
    }
    Ok(parsed)
}

fn standard_sanoid_defaults() -> OutputPolicy {
    OutputPolicy {
        autosnap: Some(true),
        autoprune: Some(true),
        frequently: Some(0),
        hourly: Some(48),
        daily: Some(90),
        weekly: Some(0),
        monthly: Some(6),
        yearly: Some(0),
        frequent_period: Some(15),
        hourly_min: Some(0),
        daily_hour: Some(23),
        daily_min: Some(59),
        weekly_wday: Some(1),
        weekly_hour: Some(23),
        weekly_min: Some(30),
        monthly_mday: Some(1),
        monthly_hour: Some(0),
        monthly_min: Some(0),
        yearly_mon: Some(1),
        yearly_mday: Some(1),
        yearly_hour: Some(0),
        yearly_min: Some(0),
        prune_defer: Some(0),
        script_timeout: Some(5),
        no_inconsistent_snapshot: Some(false),
        force_post_snapshot_script: Some(false),
        ..OutputPolicy::default()
    }
}

fn apply_policy(
    section_name: &str,
    section: &Section,
    policy: &mut OutputPolicy,
    warnings: &mut Vec<String>,
    report_ignored: bool,
) -> Result<()> {
    for (key, entry) in section {
        match key.as_str() {
            "autosnap" => policy.autosnap = Some(parse_bool(section_name, key, entry)?),
            "autoprune" => policy.autoprune = Some(parse_bool(section_name, key, entry)?),
            "frequently" => policy.frequently = Some(parse_u32(section_name, key, entry)?),
            "hourly" => policy.hourly = Some(parse_u32(section_name, key, entry)?),
            "daily" => policy.daily = Some(parse_u32(section_name, key, entry)?),
            "weekly" => policy.weekly = Some(parse_u32(section_name, key, entry)?),
            "monthly" => policy.monthly = Some(parse_u32(section_name, key, entry)?),
            "yearly" => policy.yearly = Some(parse_u32(section_name, key, entry)?),
            "frequent_period" => {
                policy.frequent_period = Some(parse_u32(section_name, key, entry)?)
            }
            "hourly_min" => policy.hourly_min = Some(parse_u32(section_name, key, entry)?),
            "daily_hour" => policy.daily_hour = Some(parse_u32(section_name, key, entry)?),
            "daily_min" => policy.daily_min = Some(parse_u32(section_name, key, entry)?),
            "weekly_wday" => policy.weekly_wday = Some(parse_weekly_wday(section_name, entry)?),
            "weekly_hour" => policy.weekly_hour = Some(parse_u32(section_name, key, entry)?),
            "weekly_min" => policy.weekly_min = Some(parse_u32(section_name, key, entry)?),
            "monthly_mday" => policy.monthly_mday = Some(parse_u32(section_name, key, entry)?),
            "monthly_hour" => policy.monthly_hour = Some(parse_u32(section_name, key, entry)?),
            "monthly_min" => policy.monthly_min = Some(parse_u32(section_name, key, entry)?),
            "yearly_mon" => policy.yearly_mon = Some(parse_u32(section_name, key, entry)?),
            "yearly_mday" => policy.yearly_mday = Some(parse_u32(section_name, key, entry)?),
            "yearly_hour" => policy.yearly_hour = Some(parse_u32(section_name, key, entry)?),
            "yearly_min" => policy.yearly_min = Some(parse_u32(section_name, key, entry)?),
            "prune_defer" => {
                let value = parse_u32(section_name, key, entry)?;
                policy.prune_defer = Some(value.try_into().with_context(|| {
                    format!("[{section_name}] {key} on line {} exceeds 255", entry.line)
                })?);
            }
            "script_timeout" => policy.script_timeout = Some(parse_timeout(section_name, entry)?),
            "no_inconsistent_snapshot" => {
                policy.no_inconsistent_snapshot = Some(parse_bool(section_name, key, entry)?)
            }
            "force_post_snapshot_script" => {
                policy.force_post_snapshot_script = Some(parse_bool(section_name, key, entry)?)
            }
            "pre_snapshot_script" => {
                set_hook(
                    &mut policy.pre_snapshot_script,
                    section_name,
                    key,
                    entry,
                    warnings,
                );
            }
            "post_snapshot_script" => {
                set_hook(
                    &mut policy.post_snapshot_script,
                    section_name,
                    key,
                    entry,
                    warnings,
                );
            }
            "pre_pruning_script" => {
                set_hook(
                    &mut policy.pre_pruning_script,
                    section_name,
                    key,
                    entry,
                    warnings,
                );
            }
            "pruning_script" => {
                set_hook(
                    &mut policy.pruning_script,
                    section_name,
                    key,
                    entry,
                    warnings,
                );
            }
            "path" | "recursive" | "use_template" | "process_children_only" | "skip_children" => {}
            _ if is_monitoring_setting(key) => {
                if report_ignored {
                    warnings.push(format!(
                        "[{section_name}] ignored monitoring-only setting {key:?} on line {}",
                        entry.line
                    ));
                }
            }
            _ => bail!(
                "[{section_name}] unknown or unsupported Sanoid setting {key:?} on line {}",
                entry.line
            ),
        }
    }
    Ok(())
}

fn set_hook(
    target: &mut Option<Vec<String>>,
    section_name: &str,
    key: &str,
    entry: &Entry,
    warnings: &mut Vec<String>,
) {
    if entry.value.is_empty() {
        *target = Some(Vec::new());
        return;
    }
    *target = Some(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        entry.value.clone(),
    ]);
    warnings.push(format!(
        "[{section_name}] converted {key} on line {} through `/bin/sh -c`; review the command before enabling zsnap",
        entry.line
    ));
}

fn warn_ignored_structural(section_name: &str, section: &Section, warnings: &mut Vec<String>) {
    for key in ["path", "recursive", "use_template", "skip_children"] {
        if let Some(entry) = section.get(key) {
            if !entry.value.is_empty() {
                warnings.push(format!(
                    "[{section_name}] ignored structural setting {key:?} on line {}; Sanoid does not apply it from templates",
                    entry.line
                ));
            }
        }
    }
}

fn resolve_process_children_only(
    section_name: &str,
    section: &Section,
    source: &SanoidConfig,
    templates: &[String],
    user_default_name: Option<&str>,
) -> Result<Option<bool>> {
    let mut value = None;
    for template in templates {
        let source_name = if user_default_name == Some(template) {
            "template_default".to_owned()
        } else {
            format!("template_{template}")
        };
        let template_section = source.sections.get(&source_name).with_context(|| {
            format!("Sanoid section [{section_name}] references missing template {template:?}")
        })?;
        if let Some(entry) = template_section.get("process_children_only") {
            value = Some(parse_bool(
                &format!("template_{template}"),
                "process_children_only",
                entry,
            )?);
        }
    }
    if let Some(entry) = section.get("process_children_only") {
        value = Some(parse_bool(section_name, "process_children_only", entry)?);
    }
    Ok(value)
}

fn parse_template_list(
    section_name: &str,
    section: &Section,
    user_default_name: Option<&str>,
) -> Result<Vec<String>> {
    let Some(entry) = section.get("use_template") else {
        return Ok(Vec::new());
    };
    let mut templates = Vec::new();
    for raw in entry.value.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            bail!(
                "[{section_name}] use_template on line {} contains an empty template name",
                entry.line
            );
        }
        if name == "default" {
            if let Some(generated_name) = user_default_name {
                templates.push(generated_name.to_owned());
            }
        } else {
            templates.push(name.to_owned());
        }
    }
    Ok(templates)
}

fn parse_recursion(section_name: &str, entry: Option<&Entry>) -> Result<Option<OutputRecursion>> {
    let Some(entry) = entry else { return Ok(None) };
    if entry.value.eq_ignore_ascii_case("zfs") {
        return Ok(Some(OutputRecursion::Zfs("zfs".to_owned())));
    }
    if parse_bool(section_name, "recursive", entry)? {
        Ok(Some(OutputRecursion::Children(true)))
    } else {
        Ok(None)
    }
}

fn parse_bool(section_name: &str, key: &str, entry: &Entry) -> Result<bool> {
    match entry.value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!(
            "[{section_name}] {key} on line {} must be yes/no, true/false, on/off, or 1/0",
            entry.line
        ),
    }
}

fn parse_u32(section_name: &str, key: &str, entry: &Entry) -> Result<u32> {
    entry.value.parse::<u32>().with_context(|| {
        format!(
            "[{section_name}] {key} on line {} must be a non-negative integer",
            entry.line
        )
    })
}

fn parse_weekly_wday(section_name: &str, entry: &Entry) -> Result<u32> {
    match parse_u32(section_name, "weekly_wday", entry)? {
        // Perl localtime numbers Sunday as zero; zsnap uses ISO Monday=1, Sunday=7.
        0 => Ok(7),
        value @ 1..=7 => Ok(value),
        _ => bail!(
            "[{section_name}] weekly_wday on line {} must be 0 (Sunday) through 7 (Sunday)",
            entry.line
        ),
    }
}

fn parse_timeout(section_name: &str, entry: &Entry) -> Result<u64> {
    let value = entry.value.parse::<i64>().with_context(|| {
        format!(
            "[{section_name}] script_timeout on line {} must be an integer",
            entry.line
        )
    })?;
    // Sanoid treats every non-positive timeout as unlimited; zsnap uses zero.
    Ok(value.max(0) as u64)
}

fn is_monitoring_setting(key: &str) -> bool {
    matches!(
        key,
        "monitor" | "monitor_dont_warn" | "monitor_dont_crit" | "capacity_warn" | "capacity_crit"
    ) || key.ends_with("_warn")
        || key.ends_with("_crit")
}

fn unique_template_name(base: &str, existing: &BTreeSet<String>) -> String {
    if !existing.contains(base) {
        return base.to_owned();
    }
    for suffix in 2.. {
        let candidate = format!("{base}_{suffix}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn apply_base_template(
    base_name: &str,
    default_process_children_only: bool,
    datasets: &mut BTreeMap<String, OutputDataset>,
) -> Result<()> {
    let recursion: Vec<_> = datasets
        .iter()
        .filter_map(|(name, dataset)| {
            dataset
                .recursive
                .as_ref()
                .map(|mode| (name.clone(), mode.clone()))
        })
        .collect();
    for (name, dataset) in datasets.iter_mut() {
        let mut inherited = false;
        for (ancestor, mode) in &recursion {
            if ancestor == name || !name.starts_with(&format!("{ancestor}/")) {
                continue;
            }
            if matches!(mode, OutputRecursion::Zfs(_)) {
                bail!(
                    "dataset {name:?} is explicitly configured below Sanoid atomic recursive root {ancestor:?}; this cannot be translated without changing semantics"
                );
            }
            inherited = true;
        }
        if !inherited {
            dataset.use_templates.insert(0, base_name.to_owned());
        }
        dataset.process_children_only = dataset
            .process_children_only_override
            .unwrap_or(!inherited && default_process_children_only);
        if dataset.process_children_only && dataset.recursive.is_none() {
            bail!(
                "dataset {name:?} resolves process_children_only = yes without recursive = yes; zsnap cannot represent that Sanoid no-op safely"
            );
        }
        if dataset.process_children_only
            && matches!(dataset.recursive, Some(OutputRecursion::Zfs(_)))
        {
            bail!(
                "dataset {name:?} combines process_children_only = yes with recursive = zfs; zsnap cannot translate that combination safely"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Recursion;

    #[test]
    fn converts_templates_recursion_overrides_and_monitoring_warnings() {
        let migration = convert_sanoid(
            r#"
[tank/data]
use_template = production,demo
recursive = yes
hourly = 12

[tank/data/db]
daily = 3

[template_production]
frequently = 0
hourly = 36
daily = 30
weekly = 4
monthly = 3
yearly = 0
autosnap = yes
autoprune = yes
process_children_only = yes
hourly_warn = 90m

[template_demo]
daily = 60
"#,
            None,
        )
        .unwrap();
        let config = Config::parse(&migration.config_toml).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.settings.timezone,
            crate::config::ScheduleTimezone::Local
        );
        assert!(migration.config_toml.contains("[tank/data]"));
        assert!(!migration.config_toml.contains("[datasets.\"tank/data\"]"));
        assert!(migration.config_toml.contains("[template_sanoid_defaults]"));
        assert!(migration.config_toml.contains("[template_production]"));
        assert!(!migration.config_toml.contains("[templates.production]"));
        assert!(
            migration
                .config_toml
                .contains("use_templates = [sanoid_defaults, production, demo]")
        );
        assert_eq!(migration.datasets, 2);
        assert_eq!(config.datasets["tank/data"].recursive, Recursion::Children);
        assert!(config.datasets["tank/data"].process_children_only);
        assert!(!config.datasets["tank/data/db"].process_children_only);
        assert_eq!(config.datasets["tank/data"].use_templates.len(), 3);
        assert_eq!(config.datasets["tank/data/db"].use_templates.len(), 0);
        assert_eq!(config.datasets["tank/data/db"].policy.daily, Some(3));
        assert!(
            migration
                .warnings
                .iter()
                .any(|warning| warning.contains("hourly_warn"))
        );
        let root = config.effective_policy("tank/data", None).unwrap();
        let child = config
            .effective_policy("tank/data/db", Some(&root))
            .unwrap();
        assert_eq!(root.hourly, 12);
        assert_eq!(root.daily, 60);
        assert_eq!(child.daily, 3);
        assert!(!root.no_inconsistent_snapshot);
    }

    #[test]
    fn converts_path_atomic_recursion_and_shell_hooks() {
        let migration = convert_sanoid(
            r#"
[alias]
path = pool/actual
use_template = scripts
recursive = zfs

[template_scripts]
pre_snapshot_script = /usr/local/bin/freeze --all && logger frozen
script_timeout = -1
no_inconsistent_snapshot = yes
"#,
            None,
        )
        .unwrap();
        let config = Config::parse(&migration.config_toml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.datasets["pool/actual"].recursive, Recursion::Zfs);
        let hook = config.templates["scripts"]
            .pre_snapshot_script
            .as_ref()
            .unwrap();
        assert_eq!(&hook[..2], ["/bin/sh", "-c"]);
        assert!(hook[2].contains("logger frozen"));
        assert_eq!(config.templates["scripts"].script_timeout, Some(0));
        assert!(
            migration
                .warnings
                .iter()
                .any(|warning| warning.contains("review"))
        );
    }

    #[test]
    fn applies_defaults_file_and_user_default_template() {
        let defaults = r#"
[version]
version = 2
[template_default]
hourly = 24
daily = 10
autosnap = yes
autoprune = yes
no_inconsistent_snapshot =
"#;
        let migration = convert_sanoid(
            r#"
[tank/data]
use_template = override,default

[template_override]
daily = 99

[template_default]
daily = 7
weekly_wday = 0
"#,
            Some(defaults),
        )
        .unwrap();
        let config = Config::parse(&migration.config_toml).unwrap();
        let policy = config.effective_policy("tank/data", None).unwrap();
        assert_eq!(policy.hourly, 24);
        assert_eq!(policy.daily, 7);
        assert_eq!(policy.weekly_wday, 7);
        assert!(!policy.no_inconsistent_snapshot);
        assert!(
            config.datasets["tank/data"]
                .use_templates
                .last()
                .unwrap()
                .starts_with("sanoid_user_defaults")
        );
    }

    #[test]
    fn rejects_lossy_or_invalid_source_configuration() {
        let skip = convert_sanoid("[tank/data]\nskip_children = yes\n", None)
            .unwrap_err()
            .to_string();
        assert!(skip.contains("cannot be translated"));

        let unknown = convert_sanoid("[tank/data]\nhourlies = 12\n", None)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown or unsupported"));

        let atomic_child = convert_sanoid(
            "[tank/data]\nrecursive = zfs\n[tank/data/db]\nhourly = 4\n",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(atomic_child.contains("atomic recursive root"));

        let children_only = convert_sanoid(
            "[tank/data]\nuse_template = children\n[template_children]\nprocess_children_only = yes\n",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(children_only.contains("without recursive"));
    }

    #[test]
    fn keeps_first_duplicate_like_sanoid() {
        let migration = convert_sanoid("[tank]\nhourly = 12\nhourly = 99\n", None).unwrap();
        let config = Config::parse(&migration.config_toml).unwrap();
        assert_eq!(config.datasets["tank"].policy.hourly, Some(12));
        assert!(migration.config_toml.contains("[tank]"));
        assert!(!migration.config_toml.contains("[datasets.\"tank\"]"));
        assert!(
            migration
                .warnings
                .iter()
                .any(|warning| warning.contains("duplicate"))
        );
    }
}
