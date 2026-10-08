//! Non-blocking multi-target webhook notifications.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use crosslane_core::config::{NotifyConfig, WebhookConfig};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WebhookEventKind {
    AccountCreditProtected,
    AccountQuotaExhausted,
    ServiceQuotaExhausted,
    TokenRefreshFailed,
    Test,
}

impl WebhookEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccountCreditProtected => "account-credit-protected",
            Self::AccountQuotaExhausted => "account-quota-exhausted",
            Self::ServiceQuotaExhausted => "service-quota-exhausted",
            Self::TokenRefreshFailed => "token-refresh-failed",
            Self::Test => "test",
        }
    }

    fn matches_subscription(self, configured: &str) -> bool {
        configured == self.as_str()
            || match self {
                // Keep existing webhook configurations working after the
                // alert policy moved to explicit incident names.
                Self::AccountCreditProtected => configured == "low-credit",
                Self::AccountQuotaExhausted => configured == "quota-exhausted",
                Self::ServiceQuotaExhausted => {
                    matches!(configured, "quota-exhausted" | "service-degraded")
                }
                Self::TokenRefreshFailed => {
                    matches!(configured, "token-expired" | "account-banned")
                }
                Self::Test => false,
            }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub kind: WebhookEventKind,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub timestamp: i64,
}

impl WebhookEvent {
    pub fn new(
        kind: WebhookEventKind,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            message: message.into(),
            account_id: None,
            timestamp: now_secs(),
        }
    }
}

#[derive(Clone)]
pub struct Notifier {
    senders: Arc<HashMap<String, mpsc::Sender<Delivery>>>,
    targets: Arc<Vec<WebhookConfig>>,
    active_incidents: Arc<Mutex<HashMap<String, i64>>>,
    incident_file: Arc<Option<PathBuf>>,
    history: Arc<Mutex<VecDeque<DeliveryLog>>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedIncidents {
    #[serde(default = "incident_state_version")]
    version: u32,
    #[serde(default)]
    incidents: HashMap<String, i64>,
}

fn incident_state_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryLog {
    pub timestamp: i64,
    pub target: String,
    pub event: WebhookEventKind,
    pub success: bool,
    pub attempts: u8,
    pub error: Option<String>,
}

#[derive(Debug)]
struct Delivery {
    target: WebhookConfig,
    event: WebhookEvent,
}

#[derive(Debug)]
struct DeliveryError {
    message: String,
    retryable_before_delivery: bool,
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl DeliveryError {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable_before_delivery: false,
        }
    }
}

const ACCOUNT_EVENT_AGGREGATION_WINDOW: Duration = Duration::from_millis(500);

enum SuppressionMark {
    None,
    Incident { key: String, timestamp: i64 },
}

impl Notifier {
    pub fn new(targets: Vec<WebhookConfig>, config: NotifyConfig, queue_size: usize) -> Self {
        Self::with_state(
            targets,
            config,
            queue_size,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(None),
            Arc::new(Mutex::new(VecDeque::new())),
        )
    }

    /// Build a notifier whose incident suppression state survives daemon
    /// restarts. The state is written before an event enters the delivery
    /// queue, so startup reconciliation does not replay an already active
    /// incident.
    pub fn persistent(
        targets: Vec<WebhookConfig>,
        config: NotifyConfig,
        queue_size: usize,
        incident_file: PathBuf,
    ) -> Self {
        let incidents = load_incidents(&incident_file);
        Self::with_state(
            targets,
            config,
            queue_size,
            Arc::new(Mutex::new(incidents)),
            Arc::new(Some(incident_file)),
            Arc::new(Mutex::new(VecDeque::new())),
        )
    }

    fn with_state(
        targets: Vec<WebhookConfig>,
        _config: NotifyConfig,
        queue_size: usize,
        active_incidents: Arc<Mutex<HashMap<String, i64>>>,
        incident_file: Arc<Option<PathBuf>>,
        history: Arc<Mutex<VecDeque<DeliveryLog>>>,
    ) -> Self {
        let senders = targets
            .iter()
            .map(|target| {
                let (sender, receiver) = mpsc::channel(queue_size.max(1));
                tokio::spawn(worker(receiver, Arc::clone(&history)));
                (target.name.clone(), sender)
            })
            .collect();
        Self {
            senders: Arc::new(senders),
            targets: Arc::new(targets),
            active_incidents,
            incident_file,
            history,
        }
    }

    /// Replace delivery targets while preserving active incident state and
    /// delivery history across config hot reloads.
    pub fn reconfigured(
        &self,
        targets: Vec<WebhookConfig>,
        config: NotifyConfig,
        queue_size: usize,
    ) -> Self {
        Self::with_state(
            targets,
            config,
            queue_size,
            Arc::clone(&self.active_incidents),
            Arc::clone(&self.incident_file),
            Arc::clone(&self.history),
        )
    }

    pub fn emit(&self, event: WebhookEvent) -> usize {
        self.emit_matching(None, event)
    }

    pub fn emit_to(&self, name: &str, event: WebhookEvent) -> usize {
        self.emit_matching(Some(name), event)
    }

    /// Send an operator-requested test without requiring an event subscription
    /// or changing incident suppression state.
    pub fn emit_test(&self, name: Option<&str>, event: WebhookEvent) -> usize {
        self.emit_matching_inner(name, event, true)
    }

    fn emit_matching(&self, name: Option<&str>, event: WebhookEvent) -> usize {
        self.emit_matching_inner(name, event, false)
    }

    fn emit_matching_inner(&self, name: Option<&str>, event: WebhookEvent, is_test: bool) -> usize {
        let mut queued = 0;
        for target in self.targets.iter() {
            if name.is_some_and(|name| target.name != name)
                || !target.enabled
                || (!is_test
                    && !target
                        .events
                        .iter()
                        .any(|kind| event.kind.matches_subscription(kind)))
            {
                continue;
            }
            let mark = if is_test {
                SuppressionMark::None
            } else {
                let Some(mark) = self.mark_send(target, &event) else {
                    continue;
                };
                mark
            };
            let Some(sender) = self.senders.get(&target.name) else {
                self.rollback_mark(mark);
                warn!(webhook = %target.name, "webhook worker missing; dropping event");
                continue;
            };
            if sender
                .try_send(Delivery {
                    target: target.clone(),
                    event: event.clone(),
                })
                .is_ok()
            {
                queued += 1;
            } else {
                self.rollback_mark(mark);
                warn!(webhook = %target.name, "webhook queue full; dropping event");
            }
        }
        queued
    }

    fn mark_send(&self, target: &WebhookConfig, event: &WebhookEvent) -> Option<SuppressionMark> {
        let account = event.account_id.as_deref().unwrap_or("global");
        let key = format!(
            "{}:{}:{account}",
            target_incident_identity(target),
            event.kind.as_str()
        );
        let now = now_millis();
        let mut incidents = lock(&self.active_incidents);
        if let Some(path) = self.incident_file.as_deref() {
            match mutate_incidents(path, |persisted| {
                if persisted.contains_key(&key) {
                    (false, false)
                } else {
                    persisted.insert(key.clone(), now);
                    (true, true)
                }
            }) {
                Ok((persisted, inserted)) => {
                    *incidents = persisted;
                    return inserted.then_some(SuppressionMark::Incident {
                        key,
                        timestamp: now,
                    });
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "failed to update webhook incident state");
                }
            }
        }
        if incidents.contains_key(&key) {
            return None;
        }
        incidents.insert(key.clone(), now);
        Some(SuppressionMark::Incident {
            key,
            timestamp: now,
        })
    }

    fn rollback_mark(&self, mark: SuppressionMark) {
        match mark {
            SuppressionMark::None => {}
            SuppressionMark::Incident { key, timestamp } => {
                let mut incidents = lock(&self.active_incidents);
                if let Some(path) = self.incident_file.as_deref() {
                    match mutate_incidents(path, |persisted| {
                        if persisted.get(&key) == Some(&timestamp) {
                            persisted.remove(&key);
                            ((), true)
                        } else {
                            ((), false)
                        }
                    }) {
                        Ok((persisted, ())) => {
                            *incidents = persisted;
                            return;
                        }
                        Err(error) => {
                            warn!(path = %path.display(), %error, "failed to roll back webhook incident state");
                        }
                    }
                }
                if incidents.get(&key) == Some(&timestamp) {
                    incidents.remove(&key);
                }
            }
        }
    }

    /// Mark an incident as recovered so a future recurrence can alert once.
    pub fn resolve_incident(&self, kind: WebhookEventKind, account_id: Option<&str>) {
        let account = account_id.unwrap_or("global");
        let suffix = format!(":{}:{account}", kind.as_str());
        let mut incidents = lock(&self.active_incidents);
        if let Some(path) = self.incident_file.as_deref() {
            match mutate_incidents(path, |persisted| {
                let previous = persisted.len();
                persisted.retain(|key, _| !key.ends_with(&suffix));
                ((), persisted.len() != previous)
            }) {
                Ok((persisted, ())) => {
                    *incidents = persisted;
                    return;
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "failed to resolve webhook incident state");
                }
            }
        }
        incidents.retain(|key, _| !key.ends_with(&suffix));
    }

    /// Return whether any configured destination currently has this incident
    /// marked active. Quota reconciliation uses this to debounce recovery and
    /// downgrade transitions without treating repeated sync calls as new
    /// observations.
    pub fn incident_active(&self, kind: WebhookEventKind, account_id: Option<&str>) -> bool {
        let account = account_id.unwrap_or("global");
        let suffix = format!(":{}:{account}", kind.as_str());
        lock(&self.active_incidents)
            .keys()
            .any(|key| key.ends_with(&suffix))
    }

    pub fn logs(&self, tail: usize) -> Vec<DeliveryLog> {
        lock(&self.history)
            .iter()
            .rev()
            .take(tail.min(1_000))
            .cloned()
            .collect()
    }
}

fn target_incident_identity(target: &WebhookConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target.kind.trim().to_ascii_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(target.url.trim().as_bytes());
    hasher.update([0]);
    if let Some(chat_id) = target.telegram_chat_id.as_deref() {
        hasher.update(chat_id.trim().as_bytes());
    }
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct IncidentFileLock {
    #[cfg(unix)]
    file: File,
}

#[cfg(unix)]
impl Drop for IncidentFileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        // SAFETY: the descriptor belongs to `file` for the duration of this
        // call. Closing it would also release the advisory lock.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn incident_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn lock_incident_file(path: &Path) -> Result<IncidentFileLock, String> {
    let lock_path = incident_lock_path(path);
    if let Some(parent) = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|error| error.to_string())?;
        loop {
            // SAFETY: `file` owns a live descriptor and flock does not retain
            // it after the call returns.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.to_string());
            }
        }
        Ok(IncidentFileLock { file })
    }
    #[cfg(not(unix))]
    {
        let _ = lock_path;
        Ok(IncidentFileLock {})
    }
}

fn mutate_incidents<T>(
    path: &Path,
    mutate: impl FnOnce(&mut HashMap<String, i64>) -> (T, bool),
) -> Result<(HashMap<String, i64>, T), String> {
    let _file_lock = lock_incident_file(path)?;
    let mut incidents = read_incidents(path)?;
    let (value, changed) = mutate(&mut incidents);
    if changed {
        write_incidents(path, &incidents)?;
    }
    Ok((incidents, value))
}

fn load_incidents(path: &Path) -> HashMap<String, i64> {
    match read_incidents(path) {
        Ok(incidents) => incidents,
        Err(error) => {
            warn!(path = %path.display(), %error, "failed to read webhook incident state");
            HashMap::new()
        }
    }
}

fn read_incidents(path: &Path) -> Result<HashMap<String, i64>, String> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error.to_string()),
    };
    match serde_json::from_slice::<PersistedIncidents>(&raw) {
        Ok(persisted) if persisted.version == incident_state_version() => Ok(persisted.incidents),
        Ok(persisted) => Err(format!(
            "unsupported webhook incident state version {}",
            persisted.version
        )),
        Err(error) => {
            let quarantine = path.with_extension(format!("corrupt.{}", now_millis()));
            if let Err(rename_error) = std::fs::rename(path, &quarantine) {
                warn!(
                    path = %path.display(),
                    %rename_error,
                    "failed to quarantine corrupt webhook incident state"
                );
            }
            warn!(
                path = %path.display(),
                %error,
                "corrupt webhook incident state was ignored"
            );
            Ok(HashMap::new())
        }
    }
}

fn write_incidents(path: &Path, incidents: &HashMap<String, i64>) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let persisted = PersistedIncidents {
        version: incident_state_version(),
        incidents: incidents.clone(),
    };
    let raw = serde_json::to_vec_pretty(&persisted).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("{}.{}.tmp", std::process::id(), now_millis()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(&raw)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| error.to_string())
}

async fn worker(
    mut receiver: mpsc::Receiver<Delivery>,
    history: Arc<Mutex<VecDeque<DeliveryLog>>>,
) {
    let client = match Client::builder().timeout(Duration::from_secs(8)).build() {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "failed to initialize webhook client");
            return;
        }
    };
    let mut pending = VecDeque::new();
    loop {
        let delivery = if let Some(delivery) = pending.pop_front() {
            delivery
        } else if let Some(delivery) = receiver.recv().await {
            delivery
        } else {
            break;
        };
        let delivery = collect_account_event_batch(&mut receiver, &mut pending, delivery).await;
        deliver(client.clone(), Arc::clone(&history), delivery).await;
    }
}

async fn collect_account_event_batch(
    receiver: &mut mpsc::Receiver<Delivery>,
    pending: &mut VecDeque<Delivery>,
    first: Delivery,
) -> Delivery {
    if first.event.account_id.is_none() {
        return first;
    }
    let kind = first.event.kind;
    let mut batch = vec![first];
    let mut deferred = VecDeque::new();
    while let Some(candidate) = pending.pop_front() {
        if is_matching_account_event(kind, &candidate) {
            batch.push(candidate);
        } else {
            deferred.push_back(candidate);
        }
    }
    *pending = deferred;

    tokio::time::sleep(ACCOUNT_EVENT_AGGREGATION_WINDOW).await;
    while let Ok(candidate) = receiver.try_recv() {
        if is_matching_account_event(kind, &candidate) {
            batch.push(candidate);
        } else {
            pending.push_back(candidate);
        }
    }
    aggregate_account_events(batch)
}

fn is_matching_account_event(kind: WebhookEventKind, delivery: &Delivery) -> bool {
    delivery.event.kind == kind && delivery.event.account_id.is_some()
}

fn aggregate_account_events(mut deliveries: Vec<Delivery>) -> Delivery {
    let count = deliveries.len();
    let mut first = deliveries.remove(0);
    if count == 1 {
        return first;
    }
    let mut messages = Vec::with_capacity(count);
    messages.push(first.event.message.clone());
    messages.extend(
        deliveries
            .into_iter()
            .map(|delivery| delivery.event.message),
    );
    first.event.title = format!("{}（{count} 个账号）", first.event.title);
    first.event.message = format!(
        "- **涉及账号：** {count} 个\n\n{}",
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| format!("**账号 {}**\n{message}", index + 1))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    );
    first.event.account_id = None;
    first
}

async fn deliver(client: Client, history: Arc<Mutex<VecDeque<DeliveryLog>>>, delivery: Delivery) {
    for attempt in 0..3 {
        match send(&client, &delivery.target, &delivery.event).await {
            Ok(()) => {
                debug!(webhook = %delivery.target.name, event = delivery.event.kind.as_str(), "webhook delivered");
                record_delivery(&history, &delivery, true, attempt + 1, None);
                break;
            }
            Err(error) if attempt < 2 && error.retryable_before_delivery => {
                warn!(webhook = %delivery.target.name, %error, attempt, "webhook retrying");
                tokio::time::sleep(Duration::from_millis(1_500 * (1 << attempt))).await;
            }
            Err(error) => {
                warn!(webhook = %delivery.target.name, %error, "webhook delivery failed");
                record_delivery(
                    &history,
                    &delivery,
                    false,
                    attempt + 1,
                    Some(error.to_string()),
                );
                break;
            }
        }
    }
}

fn record_delivery(
    history: &Mutex<VecDeque<DeliveryLog>>,
    delivery: &Delivery,
    success: bool,
    attempts: u8,
    error: Option<String>,
) {
    let mut history = lock(history);
    history.push_back(DeliveryLog {
        timestamp: now_secs(),
        target: delivery.target.name.clone(),
        event: delivery.event.kind,
        success,
        attempts,
        error,
    });
    while history.len() > 1_000 {
        history.pop_front();
    }
}

async fn send(
    client: &Client,
    target: &WebhookConfig,
    event: &WebhookEvent,
) -> Result<(), DeliveryError> {
    let (url, payload) = payload(target, event).map_err(DeliveryError::permanent)?;
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| DeliveryError {
            // Only retry failures for which reqwest knows that no HTTP
            // response was received and the connection failed before a
            // delivery could be confirmed. Timeouts are intentionally not
            // retried: the robot may already have accepted the request.
            retryable_before_delivery: error.is_connect() && !error.is_timeout(),
            message: error.to_string(),
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(DeliveryError::permanent(format!("HTTP {status}")));
    }
    if target.kind.eq_ignore_ascii_case("dingtalk") {
        let body = response.bytes().await.map_err(|error| {
            DeliveryError::permanent(format!("cannot read DingTalk response: {error}"))
        })?;
        let response: Value = serde_json::from_slice(&body).map_err(|error| {
            DeliveryError::permanent(format!("invalid DingTalk response: {error}"))
        })?;
        let errcode = response.get("errcode").and_then(Value::as_i64);
        if errcode != Some(0) {
            let errmsg = response
                .get("errmsg")
                .and_then(Value::as_str)
                .unwrap_or("unknown DingTalk error");
            return Err(DeliveryError::permanent(format!(
                "DingTalk errcode {}: {errmsg}",
                errcode
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "missing".into())
            )));
        }
    }
    Ok(())
}

fn payload(target: &WebhookConfig, event: &WebhookEvent) -> Result<(String, Value), String> {
    let markdown = format!("### {}\n\n{}", event.title, event.message);
    match target.kind.as_str() {
        "dingtalk" => {
            let url = if let Some(secret) = &target.dingtalk_sign {
                signed_dingtalk_url(&target.url, secret)?
            } else {
                target.url.clone()
            };
            Ok((
                url,
                json!({"msgtype":"markdown","markdown":{"title":event.title,"text":markdown}}),
            ))
        }
        "wechat-work" | "wechat" => Ok((
            target.url.clone(),
            json!({"msgtype":"markdown","markdown":{"content":markdown}}),
        )),
        "feishu" => Ok((
            target.url.clone(),
            json!({
                "msg_type":"interactive",
                "card":{
                    "header":{"title":{"tag":"plain_text","content":event.title}},
                    "elements":[{"tag":"markdown","content":event.message}]
                }
            }),
        )),
        "telegram" => Ok((
            target.url.clone(),
            json!({"chat_id":target.telegram_chat_id,"text":markdown,"parse_mode":"Markdown"}),
        )),
        "discord" => Ok((target.url.clone(), json!({"content":markdown}))),
        "custom" => {
            let value = if let Some(template) = target.custom_template.as_deref() {
                let json_body = template
                    .replace("{{event}}", &json_string_fragment(event.kind.as_str()))
                    .replace("{{title}}", &json_string_fragment(&event.title))
                    .replace("{{message}}", &json_string_fragment(&event.message));
                serde_json::from_str(&json_body).unwrap_or_else(|_| {
                    let text = template
                        .replace("{{event}}", event.kind.as_str())
                        .replace("{{title}}", &event.title)
                        .replace("{{message}}", &event.message);
                    json!({"text":text})
                })
            } else {
                json!({
                    "event":event.kind.as_str(),
                    "title":event.title,
                    "message":event.message,
                })
            };
            Ok((target.url.clone(), value))
        }
        kind => Err(format!("unknown webhook kind {kind}")),
    }
}

fn json_string_fragment(value: &str) -> String {
    serde_json::to_string(value)
        .map(|encoded| encoded[1..encoded.len().saturating_sub(1)].to_owned())
        .unwrap_or_default()
}

fn signed_dingtalk_url(url: &str, secret: &str) -> Result<String, String> {
    let timestamp = now_millis();
    let message = format!("{timestamp}\n{secret}");
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|error| error.to_string())?;
    mac.update(message.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let encoded = url::form_urlencoded::byte_serialize(signature.as_bytes()).collect::<String>();
    let separator = if url.contains('?') { '&' } else { '?' };
    Ok(format!(
        "{url}{separator}timestamp={timestamp}&sign={encoded}"
    ))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: &str, events: &[&str]) -> WebhookConfig {
        WebhookConfig {
            name: kind.into(),
            kind: kind.into(),
            url: "https://example.com/hook".into(),
            enabled: true,
            events: events.iter().map(|event| (*event).into()).collect(),
            dingtalk_sign: None,
            telegram_chat_id: Some("1".into()),
            custom_template: None,
        }
    }

    #[tokio::test]
    async fn incidents_alert_once_until_resolved() {
        let target = target("dingtalk", &["account-quota-exhausted"]);
        let notifier = Notifier::new(vec![target.clone()], NotifyConfig::default(), 1);
        let mut event = WebhookEvent::new(WebhookEventKind::AccountQuotaExhausted, "title", "body");
        event.account_id = Some("acc_1".into());

        assert!(notifier.mark_send(&target, &event).is_some());
        assert!(notifier.mark_send(&target, &event).is_none());
        notifier.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some("acc_1"));
        assert!(notifier.mark_send(&target, &event).is_some());
    }

    #[tokio::test]
    async fn persistent_incidents_survive_restart_and_target_rename() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("alert-incidents.json");
        let original = target("dingtalk", &["account-quota-exhausted"]);
        let mut event = WebhookEvent::new(WebhookEventKind::AccountQuotaExhausted, "title", "body");
        event.account_id = Some("acc_1".into());

        let notifier = Notifier::persistent(
            vec![original.clone()],
            NotifyConfig::default(),
            1,
            path.clone(),
        );
        let peer = Notifier::persistent(
            vec![original.clone()],
            NotifyConfig::default(),
            1,
            path.clone(),
        );
        assert!(notifier.mark_send(&original, &event).is_some());
        assert!(peer.mark_send(&original, &event).is_none());
        notifier.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some("acc_1"));
        assert!(peer.mark_send(&original, &event).is_some());
        drop(notifier);
        drop(peer);

        let mut renamed = original.clone();
        renamed.name = "renamed-target".into();
        let restarted = Notifier::persistent(
            vec![renamed.clone()],
            NotifyConfig::default(),
            1,
            path.clone(),
        );
        assert!(restarted.mark_send(&renamed, &event).is_none());
        restarted.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some("acc_1"));
        drop(restarted);

        let recovered =
            Notifier::persistent(vec![renamed.clone()], NotifyConfig::default(), 1, path);
        assert!(recovered.mark_send(&renamed, &event).is_some());
    }

    #[tokio::test]
    async fn destinations_with_different_names_share_incident_suppression() {
        let first = target("dingtalk", &["account-quota-exhausted"]);
        let mut second = first.clone();
        second.name = "another-name".into();
        let notifier = Notifier::new(
            vec![first.clone(), second.clone()],
            NotifyConfig::default(),
            1,
        );
        let mut event = WebhookEvent::new(WebhookEventKind::AccountQuotaExhausted, "title", "body");
        event.account_id = Some("acc_1".into());

        assert!(notifier.mark_send(&first, &event).is_some());
        assert!(notifier.mark_send(&second, &event).is_none());
    }

    #[test]
    fn legacy_subscriptions_map_to_explicit_incidents() {
        assert!(WebhookEventKind::AccountCreditProtected.matches_subscription("low-credit"));
        assert!(WebhookEventKind::AccountQuotaExhausted.matches_subscription("quota-exhausted"));
        assert!(WebhookEventKind::ServiceQuotaExhausted.matches_subscription("service-degraded"));
        assert!(WebhookEventKind::TokenRefreshFailed.matches_subscription("token-expired"));
    }

    #[test]
    fn same_kind_account_events_are_aggregated_into_one_delivery() {
        let target = target("dingtalk", &["account-quota-exhausted"]);
        let event = |account_id: &str, account: &str| {
            let mut event = WebhookEvent::new(
                WebhookEventKind::AccountQuotaExhausted,
                "Crosslane 账号额度耗尽",
                format!("- **账号：** `{account}`\n- **账号 ID：** `{account_id}`"),
            );
            event.account_id = Some(account_id.into());
            Delivery {
                target: target.clone(),
                event,
            }
        };

        let delivery = aggregate_account_events(vec![
            event("acc_1", "one@example.com"),
            event("acc_2", "two@example.com"),
        ]);

        assert_eq!(delivery.event.title, "Crosslane 账号额度耗尽（2 个账号）");
        assert!(delivery.event.message.contains("涉及账号：** 2 个"));
        assert!(delivery.event.message.contains("one@example.com"));
        assert!(delivery.event.message.contains("two@example.com"));
        assert!(delivery.event.account_id.is_none());
    }

    #[tokio::test]
    async fn worker_batch_collects_queued_same_kind_account_events() {
        let target = target("dingtalk", &["account-credit-protected"]);
        let event = |account_id: &str| {
            let mut event = WebhookEvent::new(
                WebhookEventKind::AccountCreditProtected,
                "Crosslane 账号剩余额度保护",
                format!("- **账号 ID：** `{account_id}`"),
            );
            event.account_id = Some(account_id.into());
            Delivery {
                target: target.clone(),
                event,
            }
        };
        let (sender, mut receiver) = mpsc::channel(4);
        sender.send(event("acc_1")).await.expect("first event");
        sender.send(event("acc_2")).await.expect("second event");
        let mut exhausted = WebhookEvent::new(
            WebhookEventKind::AccountQuotaExhausted,
            "Crosslane 账号额度耗尽",
            "- **账号 ID：** `acc_3`",
        );
        exhausted.account_id = Some("acc_3".into());
        sender
            .send(Delivery {
                target: target.clone(),
                event: exhausted,
            })
            .await
            .expect("different event kind");
        drop(sender);
        let first = receiver.recv().await.expect("queued event");
        let mut pending = VecDeque::new();

        let delivery = collect_account_event_batch(&mut receiver, &mut pending, first).await;

        assert_eq!(
            delivery.event.title,
            "Crosslane 账号剩余额度保护（2 个账号）"
        );
        assert!(delivery.event.message.contains("acc_1"));
        assert!(delivery.event.message.contains("acc_2"));
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].event.kind,
            WebhookEventKind::AccountQuotaExhausted
        );
    }

    #[test]
    fn all_six_payload_kinds_are_supported() {
        let event = WebhookEvent::new(
            WebhookEventKind::ServiceQuotaExhausted,
            "title",
            "- **状态：** 异常",
        );
        for kind in [
            "dingtalk",
            "wechat-work",
            "feishu",
            "telegram",
            "discord",
            "custom",
        ] {
            let target = target(kind, &["service-quota-exhausted"]);
            assert!(payload(&target, &event).is_ok(), "{kind}");
        }
    }

    #[test]
    fn native_webhook_payloads_use_markdown() {
        let event = WebhookEvent::new(
            WebhookEventKind::TokenRefreshFailed,
            "Token 刷新失败",
            "- **账号：** `user@example.com`",
        );
        let (_, dingtalk) = payload(&target("dingtalk", &[]), &event).expect("dingtalk");
        assert_eq!(dingtalk["msgtype"], "markdown");
        assert!(dingtalk["markdown"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("**账号：**")));

        let (_, wechat) = payload(&target("wechat-work", &[]), &event).expect("wechat");
        assert_eq!(wechat["msgtype"], "markdown");
        let (_, feishu) = payload(&target("feishu", &[]), &event).expect("feishu");
        assert_eq!(feishu["msg_type"], "interactive");
        assert_eq!(feishu["card"]["elements"][0]["tag"], "markdown");

        let (_, custom) = payload(&target("custom", &[]), &event).expect("custom");
        assert_eq!(custom["event"], "token-refresh-failed");
        assert_eq!(custom["message"], "- **账号：** `user@example.com`");
    }

    #[test]
    fn dingtalk_signature_is_url_encoded() {
        let signed = signed_dingtalk_url("https://example.com/hook?a=1", "secret").expect("sign");
        assert!(signed.contains("&timestamp="));
        assert!(signed.contains("&sign="));
        assert!(!signed.contains('+'));
    }
}
