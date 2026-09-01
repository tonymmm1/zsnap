use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;

use crate::config::{
    Notifications, WebhookConfig, WebhookEvent, WebhookKind, is_portable_environment_name,
    validate_webhook_url,
};

const MAX_MESSAGE_CHARACTERS: usize = 1_900;
const MAX_RETRY_DELAY_MILLISECONDS: u64 = 30_000;
const WEBHOOK_ENVIRONMENT_FILE: &str = "webhooks.env";

type WebhookEnvironment = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryEvent {
    Failure,
    Success,
    Test,
}

#[derive(Clone, Copy)]
enum TransportPolicy {
    HttpsOnly,
    #[cfg(test)]
    TestHttp,
}

impl TransportPolicy {
    const fn https_only(self) -> bool {
        match self {
            Self::HttpsOnly => true,
            #[cfg(test)]
            Self::TestHttp => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NotificationReport {
    pub attempted: usize,
    pub delivered: usize,
    pub skipped: usize,
    pub deliveries: Vec<NotificationDelivery>,
}

impl NotificationReport {
    pub fn succeeded(&self) -> bool {
        self.deliveries
            .iter()
            .all(|delivery| delivery.error.is_none())
    }

    pub fn failed(&self) -> usize {
        self.deliveries
            .iter()
            .filter(|delivery| delivery.error.is_some())
            .count()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationDelivery {
    pub target: String,
    pub kind: String,
    pub attempts: u32,
    pub error: Option<String>,
}

pub fn deliver(
    config: &Notifications,
    event: DeliveryEvent,
    message: &str,
) -> Result<NotificationReport> {
    deliver_with_policy(
        config,
        event,
        message,
        TransportPolicy::HttpsOnly,
        &WebhookEnvironment::new(),
    )
}

/// Return the conventional webhook environment file next to a zsnap config.
pub fn environment_file_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(WEBHOOK_ENVIRONMENT_FILE)
}

/// Deliver notifications with secrets from the process environment or an optional file.
///
/// Explicit process environment values take precedence over file values. A missing file is
/// allowed so configurations that use direct URLs or externally supplied variables keep working.
pub fn deliver_with_environment_file(
    config: &Notifications,
    event: DeliveryEvent,
    message: &str,
    environment_file: &Path,
) -> Result<NotificationReport> {
    let file_environment = load_environment_file(environment_file)?;
    deliver_with_policy(
        config,
        event,
        message,
        TransportPolicy::HttpsOnly,
        &file_environment,
    )
}

fn deliver_with_policy(
    config: &Notifications,
    event: DeliveryEvent,
    message: &str,
    transport_policy: TransportPolicy,
    file_environment: &WebhookEnvironment,
) -> Result<NotificationReport> {
    let selected: Vec<_> = if config.enabled {
        config
            .webhooks
            .iter()
            .filter(|webhook| webhook.enabled && subscribes(webhook, event))
            .collect()
    } else {
        Vec::new()
    };
    let skipped = config.webhooks.len().saturating_sub(selected.len());
    if selected.is_empty() {
        return Ok(NotificationReport {
            skipped,
            ..NotificationReport::default()
        });
    }

    let thread_count = config.max_parallel.min(selected.len()).max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .thread_name(|index| format!("zsnap-webhook-{index}"))
        .build()
        .context("failed to create the webhook delivery thread set")?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(config.timeout_seconds)))
        .https_only(transport_policy.https_only())
        .max_redirects(3)
        .user_agent(format!("zsnap/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let message = truncate_message(message);

    let mut deliveries = pool.install(|| {
        selected
            .into_par_iter()
            .map(|webhook| {
                deliver_one(
                    &agent,
                    config,
                    webhook,
                    &message,
                    transport_policy,
                    file_environment,
                )
            })
            .collect::<Vec<_>>()
    });
    deliveries.sort_by(|left, right| left.target.cmp(&right.target));
    let delivered = deliveries
        .iter()
        .filter(|delivery| delivery.error.is_none())
        .count();
    Ok(NotificationReport {
        attempted: deliveries.len(),
        delivered,
        skipped,
        deliveries,
    })
}

pub fn hostname(config: &Notifications) -> String {
    if let Some(hostname) = &config.hostname {
        return hostname.trim().to_owned();
    }
    let detected = env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_else(|| "unknown-host".to_owned());
    detected
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown-host")
        .to_owned()
}

fn subscribes(webhook: &WebhookConfig, event: DeliveryEvent) -> bool {
    match event {
        DeliveryEvent::Failure => webhook.events.contains(&WebhookEvent::Failure),
        DeliveryEvent::Success => webhook.events.contains(&WebhookEvent::Success),
        DeliveryEvent::Test => true,
    }
}

fn deliver_one(
    agent: &ureq::Agent,
    config: &Notifications,
    webhook: &WebhookConfig,
    message: &str,
    transport_policy: TransportPolicy,
    file_environment: &WebhookEnvironment,
) -> NotificationDelivery {
    let mut delivery = NotificationDelivery {
        target: webhook.name.clone(),
        kind: webhook.kind.to_string(),
        attempts: 0,
        error: None,
    };
    let url = match resolve_url(webhook, transport_policy, file_environment) {
        Ok(url) => url,
        Err(error) => {
            delivery.error = Some(error);
            return delivery;
        }
    };
    let body = payload(webhook.kind, message);

    for attempt in 1..=config.max_attempts {
        delivery.attempts = attempt;
        match agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(&body)
        {
            Ok(_) => return delivery,
            Err(error) => {
                let retry = is_retryable(&error) && attempt < config.max_attempts;
                delivery.error = Some(describe_error(&error, &url));
                if !retry {
                    break;
                }
                let multiplier = 1_u64 << (attempt - 1).min(10);
                thread::sleep(Duration::from_millis(
                    config
                        .retry_backoff_milliseconds
                        .saturating_mul(multiplier)
                        .min(MAX_RETRY_DELAY_MILLISECONDS),
                ));
            }
        }
    }
    delivery
}

fn payload(kind: WebhookKind, message: &str) -> String {
    match kind {
        WebhookKind::Discord => serde_json::json!({
            "content": message,
            "allowed_mentions": { "parse": [] }
        }),
        WebhookKind::Flock | WebhookKind::Slack => serde_json::json!({ "text": message }),
    }
    .to_string()
}

fn resolve_url(
    webhook: &WebhookConfig,
    transport_policy: TransportPolicy,
    file_environment: &WebhookEnvironment,
) -> std::result::Result<String, String> {
    let url = if let Some(url) = &webhook.url {
        url.expose().to_owned()
    } else if let Some(variable) = &webhook.url_env {
        match env::var(variable) {
            Ok(value) => value,
            Err(env::VarError::NotPresent) => file_environment
                .get(variable)
                .cloned()
                .ok_or_else(|| format!("environment variable {variable} is not set"))?,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(format!(
                    "environment variable {variable} is not valid UTF-8"
                ));
            }
        }
    } else {
        return Err("no URL source is configured".to_owned());
    };
    match transport_policy {
        TransportPolicy::HttpsOnly => {
            validate_webhook_url(&url, &format!("notification webhook {:?}", webhook.name))
                .map_err(|error| error.to_string())?;
        }
        #[cfg(test)]
        TransportPolicy::TestHttp => {}
    }
    Ok(url)
}

fn load_environment_file(path: &Path) -> Result<WebhookEnvironment> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(WebhookEnvironment::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read webhook environment {}", path.display()));
        }
    };
    let mut environment = WebhookEnvironment::new();
    for (index, raw_line) in raw.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((raw_name, raw_value)) = line.split_once('=') else {
            bail!(
                "invalid webhook environment {} line {}: expected NAME=VALUE",
                path.display(),
                line_number
            );
        };
        let name = raw_name.trim();
        if !is_portable_environment_name(name) {
            bail!(
                "invalid webhook environment {} line {}: invalid variable name",
                path.display(),
                line_number
            );
        }
        if environment.contains_key(name) {
            bail!(
                "invalid webhook environment {} line {}: duplicate variable {name}",
                path.display(),
                line_number
            );
        }
        let value = parse_environment_value(raw_value, path, line_number)?;
        environment.insert(name.to_owned(), value);
    }
    Ok(environment)
}

fn parse_environment_value(raw: &str, path: &Path, line_number: usize) -> Result<String> {
    let value = raw.trim();
    let malformed = || {
        anyhow::anyhow!(
            "invalid webhook environment {} line {}: use an unquoted value without spaces or one matching pair of quotes",
            path.display(),
            line_number
        )
    };
    let Some(first) = value.chars().next() else {
        return Ok(String::new());
    };
    if matches!(first, '\'' | '"') {
        if value.len() < 2 || !value.ends_with(first) {
            return Err(malformed());
        }
        let inner = &value[first.len_utf8()..value.len() - first.len_utf8()];
        if inner.contains(first) || inner.contains(['\n', '\r', '\0']) {
            return Err(malformed());
        }
        return Ok(inner.to_owned());
    }
    if value.chars().any(char::is_whitespace) || value.contains(['\'', '"', '\0']) {
        return Err(malformed());
    }
    Ok(value.to_owned())
}

fn is_retryable(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(code) => {
            matches!(*code, 408 | 425 | 429) || (500..=599).contains(code)
        }
        ureq::Error::BadUri(_)
        | ureq::Error::Http(_)
        | ureq::Error::RequireHttpsOnly(_)
        | ureq::Error::TlsRequired
        | ureq::Error::Tls(_)
        | ureq::Error::Rustls(_)
        | ureq::Error::RedirectFailed
        | ureq::Error::TooManyRedirects => false,
        _ => true,
    }
}

fn describe_error(error: &ureq::Error, url: &str) -> String {
    match error {
        ureq::Error::StatusCode(code) => format!("remote endpoint returned HTTP {code}"),
        ureq::Error::Timeout(_) => "request timed out".to_owned(),
        ureq::Error::HostNotFound => "webhook host could not be resolved".to_owned(),
        ureq::Error::RequireHttpsOnly(_) => "a non-HTTPS URL or redirect was refused".to_owned(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::TlsRequired => {
            "TLS connection or certificate validation failed".to_owned()
        }
        _ => error.to_string().replace(url, "[redacted-url]"),
    }
}

fn truncate_message(message: &str) -> Cow<'_, str> {
    if message.chars().count() <= MAX_MESSAGE_CHARACTERS {
        return Cow::Borrowed(message);
    }
    let mut shortened: String = message.chars().take(MAX_MESSAGE_CHARACTERS - 1).collect();
    shortened.push('…');
    Cow::Owned(shortened)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::SecretString;

    fn webhook(kind: WebhookKind) -> WebhookConfig {
        WebhookConfig {
            name: "test".to_owned(),
            kind,
            url: Some(SecretString::new("https://example.invalid/hook")),
            url_env: None,
            events: vec![WebhookEvent::Failure],
            enabled: true,
        }
    }

    #[test]
    fn subscriptions_default_to_failure_and_tests_select_everything() {
        let webhook = webhook(WebhookKind::Slack);
        assert!(subscribes(&webhook, DeliveryEvent::Failure));
        assert!(!subscribes(&webhook, DeliveryEvent::Success));
        assert!(subscribes(&webhook, DeliveryEvent::Test));
    }

    #[test]
    fn truncation_is_unicode_safe_and_within_discord_limit() {
        let input = "🦀".repeat(2_001);
        let output = truncate_message(&input);
        assert_eq!(output.chars().count(), MAX_MESSAGE_CHARACTERS);
        assert!(output.ends_with('…'));
    }

    #[test]
    fn secrets_are_redacted_from_debug_output() {
        let output = format!("{:?}", webhook(WebhookKind::Discord));
        assert!(output.contains("[redacted]"));
        assert!(!output.contains("example.invalid"));
    }

    #[test]
    fn provider_payloads_use_the_expected_fields() {
        let discord: serde_json::Value =
            serde_json::from_str(&payload(WebhookKind::Discord, "hello @everyone")).unwrap();
        assert_eq!(discord["content"], "hello @everyone");
        assert_eq!(discord["allowed_mentions"]["parse"], serde_json::json!([]));
        assert!(discord.get("text").is_none());

        for kind in [WebhookKind::Flock, WebhookKind::Slack] {
            let value: serde_json::Value = serde_json::from_str(&payload(kind, "hello")).unwrap();
            assert_eq!(value["text"], "hello");
            assert!(value.get("content").is_none());
        }
    }

    #[test]
    fn delivery_runs_concurrently_with_the_configured_bound() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let server_active = Arc::clone(&active);
        let server_maximum = Arc::clone(&maximum);
        let server = thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let active = Arc::clone(&server_active);
                let maximum = Arc::clone(&server_maximum);
                handlers.push(thread::spawn(move || {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    let mut request = [0_u8; 2_048];
                    let _ = stream.read(&mut request).unwrap();
                    thread::sleep(Duration::from_millis(100));
                    stream
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });

        let webhooks = (0..4)
            .map(|index| WebhookConfig {
                name: format!("target-{index}"),
                kind: WebhookKind::Slack,
                url: Some(SecretString::new(format!("http://{address}/{index}"))),
                url_env: None,
                events: vec![WebhookEvent::Failure],
                enabled: true,
            })
            .collect();
        let config = Notifications {
            max_parallel: 2,
            max_attempts: 1,
            retry_backoff_milliseconds: 0,
            webhooks,
            ..Notifications::default()
        };
        let report = deliver_with_policy(
            &config,
            DeliveryEvent::Failure,
            "test",
            TransportPolicy::TestHttp,
            &WebhookEnvironment::new(),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(report.delivered, 4);
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn loads_quoted_values_from_the_config_sibling_environment_file() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("zsnap.toml");
        let environment_path = environment_file_for_config(&config_path);
        fs::write(
            &environment_path,
            "# webhook secrets\nZSNAP_TEST_DISCORD='https://example.invalid/discord'\nZSNAP_TEST_SLACK=https://example.invalid/slack\n",
        )
        .unwrap();

        let environment = load_environment_file(&environment_path).unwrap();
        assert_eq!(
            environment.get("ZSNAP_TEST_DISCORD").unwrap(),
            "https://example.invalid/discord"
        );
        assert_eq!(
            environment.get("ZSNAP_TEST_SLACK").unwrap(),
            "https://example.invalid/slack"
        );
    }

    #[test]
    fn resolves_url_env_from_the_environment_file_fallback() {
        const VARIABLE: &str = "ZSNAP_UNIT_TEST_FILE_ONLY_7D5ED06D";
        let mut target = webhook(WebhookKind::Discord);
        target.url = None;
        target.url_env = Some(VARIABLE.to_owned());
        let environment = WebhookEnvironment::from([(
            VARIABLE.to_owned(),
            "https://example.invalid/from-file".to_owned(),
        )]);

        let resolved = resolve_url(&target, TransportPolicy::HttpsOnly, &environment).unwrap();
        assert_eq!(resolved, "https://example.invalid/from-file");
    }

    #[test]
    fn rejects_malformed_environment_files_without_exposing_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("webhooks.env");
        fs::write(&path, "ZSNAP_SECRET='do-not-print\n").unwrap();

        let error = load_environment_file(&path).unwrap_err().to_string();
        assert!(error.contains("line 1"));
        assert!(!error.contains("do-not-print"));
    }
}
