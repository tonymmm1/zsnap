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
const MAX_DETAIL_CHARACTERS: usize = 500;
/// Room kept for the closing code fence and the "more details" line.
const DETAIL_OVERFLOW_RESERVE: usize = 80;
const MAX_RETRY_DELAY_MILLISECONDS: u64 = 30_000;
const WEBHOOK_ENVIRONMENT_FILE: &str = "webhooks.env";

type WebhookEnvironment = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryEvent {
    Failure,
    Success,
    Test,
}

/// A provider-neutral notification rendered into each webhook's native markup.
#[derive(Debug, Clone, Default)]
pub struct Notice {
    /// Short headline such as "zsnap run failed".
    pub headline: String,
    pub host: String,
    pub fields: Vec<NoticeField>,
    /// Heading shown above `details`, such as "Errors (2)".
    pub details_heading: String,
    /// Verbatim diagnostic lines, rendered in a code block where supported.
    pub details: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NoticeField {
    pub label: String,
    pub value: String,
    /// Render the value as inline code (paths, commands, identifiers).
    pub code: bool,
}

impl NoticeField {
    pub fn text(label: &str, value: impl Into<String>) -> Self {
        Self {
            label: label.to_owned(),
            value: value.into(),
            code: false,
        }
    }

    pub fn code(label: &str, value: impl Into<String>) -> Self {
        Self {
            label: label.to_owned(),
            value: value.into(),
            code: true,
        }
    }
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
    notice: &Notice,
) -> Result<NotificationReport> {
    deliver_with_policy(
        config,
        event,
        notice,
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
    notice: &Notice,
    environment_file: &Path,
) -> Result<NotificationReport> {
    let file_environment = load_environment_file(environment_file)?;
    deliver_with_policy(
        config,
        event,
        notice,
        TransportPolicy::HttpsOnly,
        &file_environment,
    )
}

fn deliver_with_policy(
    config: &Notifications,
    event: DeliveryEvent,
    notice: &Notice,
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

    let mut deliveries = pool.install(|| {
        selected
            .into_par_iter()
            .map(|webhook| {
                deliver_one(
                    &agent,
                    config,
                    webhook,
                    &render(webhook.kind, event, notice),
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

#[derive(Clone, Copy)]
enum Markup {
    Discord,
    Slack,
    Plain,
}

impl Markup {
    const fn for_kind(kind: WebhookKind) -> Self {
        match kind {
            WebhookKind::Discord => Self::Discord,
            WebhookKind::Slack => Self::Slack,
            WebhookKind::Flock => Self::Plain,
        }
    }

    fn bold(self, text: &str) -> String {
        match self {
            Self::Discord => format!("**{}**", self.escape(text)),
            Self::Slack => format!("*{}*", self.escape(text)),
            Self::Plain => text.to_owned(),
        }
    }

    fn code(self, text: &str) -> String {
        match self {
            // A backtick would close the span early, so swap in a look-alike.
            Self::Discord => format!("`{}`", text.replace('`', "ˋ")),
            Self::Slack => format!("`{}`", escape_slack(text).replace('`', "ˋ")),
            Self::Plain => text.to_owned(),
        }
    }

    /// Escape plain text so hostnames or messages cannot inject formatting.
    fn escape(self, text: &str) -> String {
        match self {
            Self::Discord => {
                let mut escaped = String::with_capacity(text.len());
                for character in text.chars() {
                    if matches!(character, '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']') {
                        escaped.push('\\');
                    }
                    escaped.push(character);
                }
                escaped
            }
            Self::Slack => escape_slack(text),
            Self::Plain => text.to_owned(),
        }
    }

    const fn fences(self) -> (&'static str, &'static str) {
        match self {
            Self::Discord => ("\n```text", "\n```"),
            Self::Slack => ("\n```", "\n```"),
            Self::Plain => ("", ""),
        }
    }

    fn detail(self, text: &str) -> String {
        match self {
            Self::Discord => text.replace("```", "ˋˋˋ"),
            Self::Slack => escape_slack(text).replace("```", "ˋˋˋ"),
            Self::Plain => format!("- {}", text.replace('\n', "\n  ")),
        }
    }
}

fn escape_slack(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a notice for one provider, keeping it inside the shared message budget.
///
/// Fields are always kept; detail lines are added in order until the budget runs out, and
/// the remainder is summarized so a closing code fence is never cut off.
fn render(kind: WebhookKind, event: DeliveryEvent, notice: &Notice) -> String {
    let markup = Markup::for_kind(kind);
    let icon = match event {
        DeliveryEvent::Failure => "❌",
        DeliveryEvent::Success => "✅",
        DeliveryEvent::Test => "🔔",
    };
    let mut text = format!(
        "{icon} {} on {}",
        markup.bold(&notice.headline),
        markup.code(&notice.host)
    );
    for field in &notice.fields {
        let value = if field.code {
            markup.code(&field.value)
        } else {
            markup.escape(&field.value)
        };
        text.push('\n');
        text.push_str(&markup.bold(&format!("{}:", field.label)));
        text.push(' ');
        text.push_str(&value);
    }

    if !notice.details.is_empty() {
        let (open, close) = markup.fences();
        text.push('\n');
        text.push_str(&markup.bold(&format!("{}:", notice.details_heading)));
        text.push_str(open);
        let mut budget = MAX_MESSAGE_CHARACTERS
            .saturating_sub(text.chars().count() + close.chars().count() + DETAIL_OVERFLOW_RESERVE);
        let mut shown = 0;
        for detail in &notice.details {
            let limit = if shown == 0 {
                // Always show the first error, even if it has to be shortened further.
                MAX_DETAIL_CHARACTERS.min(budget.saturating_sub(1))
            } else {
                MAX_DETAIL_CHARACTERS
            };
            let line = format!("\n{}", markup.detail(&shorten(detail.trim(), limit)));
            let length = line.chars().count();
            if length > budget {
                break;
            }
            text.push_str(&line);
            budget -= length;
            shown += 1;
        }
        text.push_str(close);
        let omitted = notice.details.len() - shown;
        if omitted > 0 {
            text.push('\n');
            text.push_str(&markup.escape(&format!(
                "…and {omitted} more; see the host log for the full list"
            )));
        }
    }
    shorten(&text, MAX_MESSAGE_CHARACTERS).into_owned()
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

fn shorten(text: &str, limit: usize) -> Cow<'_, str> {
    if text.chars().count() <= limit {
        return Cow::Borrowed(text);
    }
    let mut shortened: String = text.chars().take(limit.saturating_sub(1)).collect();
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
        let output = shorten(&input, MAX_MESSAGE_CHARACTERS);
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

    fn failure_notice(details: Vec<String>) -> Notice {
        Notice {
            headline: "zsnap run failed".to_owned(),
            host: "nas01".to_owned(),
            fields: vec![
                NoticeField::text("Snapshots", "created 1 of 2"),
                NoticeField::code("Config", "/etc/zsnap/zsnap.toml"),
            ],
            details_heading: format!("Errors ({})", details.len()),
            details,
        }
    }

    #[test]
    fn discord_failures_show_each_error_in_a_closed_code_block() {
        let notice = failure_notice(vec![
            "[tank] \"zfs\" \"snapshot\" failed with exit status: 1: out of space".to_owned(),
            "[backup] hook \"/usr/local/bin/pre\" timed out after 30s".to_owned(),
        ]);
        let text = render(WebhookKind::Discord, DeliveryEvent::Failure, &notice);
        assert!(
            text.starts_with("❌ **zsnap run failed** on `nas01`\n"),
            "{text}"
        );
        assert!(
            text.contains("**Config:** `/etc/zsnap/zsnap.toml`"),
            "{text}"
        );
        assert!(text.contains("**Errors (2):**\n```text\n[tank]"), "{text}");
        assert!(text.contains("timed out after 30s\n```"), "{text}");
        assert!(!text.contains("more; see"), "{text}");
    }

    #[test]
    fn long_error_lists_stay_within_budget_and_keep_the_fence_closed() {
        let details = (0..40)
            .map(|index| format!("[tank] error {index}: {}", "x".repeat(200)))
            .collect();
        let text = render(
            WebhookKind::Discord,
            DeliveryEvent::Failure,
            &failure_notice(details),
        );
        assert!(text.chars().count() <= MAX_MESSAGE_CHARACTERS);
        assert_eq!(text.matches("```").count(), 2, "{text}");
        assert!(text.contains("[tank] error 0:"));
        assert!(
            text.ends_with("more; see the host log for the full list"),
            "{text}"
        );
    }

    #[test]
    fn an_oversized_first_error_is_shortened_rather_than_dropped() {
        let text = render(
            WebhookKind::Discord,
            DeliveryEvent::Failure,
            &failure_notice(vec!["y".repeat(5_000)]),
        );
        assert!(text.contains("yyy…\n```"), "{text}");
        assert!(!text.contains("more; see"));
    }

    #[test]
    fn untrusted_text_cannot_break_out_of_formatting() {
        let mut notice = failure_notice(vec!["stderr with ``` fence & <tag>".to_owned()]);
        notice.host = "evil`host".to_owned();
        notice.fields = vec![NoticeField::text("Message", "*bold* _it_ [link](x)")];

        let discord = render(WebhookKind::Discord, DeliveryEvent::Failure, &notice);
        assert!(discord.contains("`evilˋhost`"), "{discord}");
        assert!(
            discord.contains("\\*bold\\* \\_it\\_ \\[link\\](x)"),
            "{discord}"
        );
        assert_eq!(discord.matches("```").count(), 2, "{discord}");

        let slack = render(WebhookKind::Slack, DeliveryEvent::Failure, &notice);
        assert!(
            slack.starts_with("❌ *zsnap run failed* on `evilˋhost`"),
            "{slack}"
        );
        assert!(slack.contains("&amp; &lt;tag&gt;"), "{slack}");
    }

    #[test]
    fn flock_receives_plain_text() {
        let notice = failure_notice(vec!["first\nsecond line".to_owned()]);
        let text = render(WebhookKind::Flock, DeliveryEvent::Failure, &notice);
        assert!(!text.contains('`') && !text.contains('*'), "{text}");
        assert!(text.contains("Config: /etc/zsnap/zsnap.toml"), "{text}");
        assert!(
            text.contains("Errors (1):\n- first\n  second line"),
            "{text}"
        );
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
            &Notice::default(),
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
