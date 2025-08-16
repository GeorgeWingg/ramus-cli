use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;
use tokio::fs;
use tracing::{error, info, warn};

use codex_core::plan_tool::UpdatePlanArgs;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEventEnvelope {
    pub event: String,
    pub run_id: String,
    pub task_id: String,
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub plan: UpdatePlanArgs,
    pub meta: EventMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMeta {
    pub model: String,
    pub codex_ver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SequenceMeta {
    last_seq: u64,
}

#[derive(Clone)]
pub struct PlanEmitter {
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
    plan_events_path: Option<PathBuf>,
    plan_state_path: Option<PathBuf>,
    task_id: String,
    run_id: String,
    model: String,
    emit_stdout: bool,
    json_mode: bool,
}

impl PlanEmitter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        webhook_url: Option<String>,
        webhook_secret: Option<String>,
        plan_events_path: Option<PathBuf>,
        plan_state_path: Option<PathBuf>,
        task_id: String,
        run_id: String,
        model: String,
        emit_stdout: bool,
        json_mode: bool,
    ) -> Result<Self> {
        Ok(Self {
            webhook_url,
            webhook_secret,
            plan_events_path,
            plan_state_path,
            task_id,
            run_id,
            model,
            emit_stdout,
            json_mode,
        })
    }

    pub async fn emit_plan_update(
        &self,
        plan: &UpdatePlanArgs,
        sequence_number: u64,
    ) -> Result<()> {
        let envelope = PlanEventEnvelope {
            event: "plan_update".to_string(),
            run_id: self.run_id.clone(),
            task_id: self.task_id.clone(),
            seq: sequence_number,
            ts: OffsetDateTime::now_utc(),
            plan: plan.clone(),
            meta: EventMeta {
                model: self.model.clone(),
                codex_ver: env!("CARGO_PKG_VERSION").to_string(), // Use build-time version
            },
        };

        // Emit to stdout with @plan prefix (use stderr if JSON mode to avoid corruption)
        if self.emit_stdout {
            let json_str =
                serde_json::to_string(&envelope).context("Failed to serialize plan event")?;

            if self.json_mode {
                // Emit to stderr to avoid corrupting JSON output stream
                eprintln!("@plan {json_str}");
            } else {
                println!("@plan {json_str}");
            }
        }

        // Write to event log
        if let Some(ref path) = self.plan_events_path {
            self.write_event_log(path, &envelope).await?;
        }

        // Write current state
        if let Some(ref path) = self.plan_state_path {
            self.write_plan_state(path, &envelope).await?;
        }

        // Send webhook - propagate failures to maintain sequence integrity
        //
        // SEQUENCE SEMANTICS: We implement Option A (contiguous sequences on receiver)
        // - Webhook failures cause emit_plan_update() to return Err
        // - Event loop only increments sequence on successful delivery
        // - This ensures receivers get contiguous, ordered events with no gaps
        // - Trade-off: Local sequence may get "stuck" on persistent webhook failures
        if let (Some(url), Some(secret)) = (&self.webhook_url, &self.webhook_secret) {
            self.send_webhook(url, secret, &envelope)
                .await
                .context("Failed to deliver webhook")?;
        }

        Ok(())
    }

    pub async fn emit_shutdown(&self, sequence_number: u64) -> Result<()> {
        info!("Emitting shutdown event with sequence {}", sequence_number);
        // For now, just log the shutdown. Could extend to send a specific event type.
        Ok(())
    }

    pub async fn load_sequence(&self) -> Result<u64> {
        if let Some(ref state_path) = self.plan_state_path {
            let meta_path = state_path.with_file_name("plan.meta.json");

            if meta_path.exists() {
                let content = fs::read_to_string(&meta_path).await.with_context(|| {
                    format!("Failed to read sequence from {}", meta_path.display())
                })?;

                let meta: SequenceMeta =
                    serde_json::from_str(&content).context("Failed to parse sequence metadata")?;

                return Ok(meta.last_seq + 1);
            }
        }

        Ok(1) // Start from sequence 1 if no metadata found
    }

    pub async fn persist_sequence(&self, sequence_number: u64) -> Result<()> {
        if let Some(ref state_path) = self.plan_state_path {
            let meta_path = state_path.with_file_name("plan.meta.json");

            // Create parent directory if it doesn't exist
            if let Some(parent) = meta_path.parent() {
                fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("Failed to create directory {}", parent.display()))?;
            }

            let meta = SequenceMeta {
                last_seq: sequence_number,
            };

            self.write_atomic(&meta_path, &serde_json::to_string_pretty(&meta)?)
                .await?;
        }

        Ok(())
    }

    async fn write_event_log(&self, path: &PathBuf, envelope: &PlanEventEnvelope) -> Result<()> {
        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let json_line =
            serde_json::to_string(envelope).context("Failed to serialize event for log")?;

        let content = format!("{json_line}\n");

        // Append to file using OpenOptions
        use tokio::fs::OpenOptions;
        use tokio::io::AsyncWriteExt;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .with_context(|| format!("Failed to open event log {}", path.display()))?;

        file.write_all(content.as_bytes())
            .await
            .with_context(|| format!("Failed to write to event log {}", path.display()))?;

        file.flush()
            .await
            .with_context(|| format!("Failed to flush event log {}", path.display()))?;

        Ok(())
    }

    async fn write_plan_state(&self, path: &PathBuf, envelope: &PlanEventEnvelope) -> Result<()> {
        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let json_content =
            serde_json::to_string_pretty(envelope).context("Failed to serialize plan state")?;

        self.write_atomic(path, &json_content).await
    }

    async fn write_atomic(&self, path: &PathBuf, content: &str) -> Result<()> {
        let tmp_path = path.with_extension("tmp");

        // Write to temporary file with explicit fsync
        use tokio::fs::OpenOptions;
        use tokio::io::AsyncWriteExt;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .await
            .with_context(|| format!("Failed to create temporary file {}", tmp_path.display()))?;

        file.write_all(content.as_bytes())
            .await
            .with_context(|| format!("Failed to write to temporary file {}", tmp_path.display()))?;

        file.flush()
            .await
            .with_context(|| format!("Failed to flush temporary file {}", tmp_path.display()))?;

        // Fsync the file before rename
        file.sync_all()
            .await
            .with_context(|| format!("Failed to fsync temporary file {}", tmp_path.display()))?;

        // Close the file before rename
        drop(file);

        // Rename to final location (atomic operation)
        fs::rename(&tmp_path, path).await.with_context(|| {
            format!(
                "Failed to rename {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;

        // Fsync parent directory for durability
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent).await {
                if let Err(e) = dir.sync_all().await {
                    warn!(
                        "Failed to fsync parent directory {}: {}",
                        parent.display(),
                        e
                    );
                }
            }
        }

        Ok(())
    }

    #[allow(unused_assignments)]
    async fn send_webhook(
        &self,
        url: &str,
        secret: &str,
        envelope: &PlanEventEnvelope,
    ) -> Result<()> {
        // Build a short‑lived HTTP client per send to avoid initializing
        // platform networking state in restricted test environments.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(2))
            .build()
            .context("Failed to create HTTP client")?;
        let body =
            serde_json::to_string(envelope).context("Failed to serialize webhook payload")?;

        let timestamp = envelope.ts;
        let timestamp_str = timestamp
            .format(&time::format_description::well_known::Iso8601::DEFAULT)
            .context("Failed to format timestamp")?;

        // Sign canonical string: "v0:{timestamp}:{body}"
        let canonical_string = format!("v0:{timestamp_str}:{body}");
        let signature = self.compute_hmac(secret, &canonical_string)?;

        // Use specific retry delays as per plan: 200ms, 500ms, 1s (3 total attempts)
        let retry_delays = [
            Duration::from_millis(200),
            Duration::from_millis(500),
            Duration::from_secs(1),
        ];

        // Manual retry logic with specific delays
        let mut last_error = None;

        // First attempt (no delay)
        match self
            .try_webhook_request(&client, url, envelope, &timestamp_str, &signature, &body)
            .await
        {
            Ok(()) => {
                info!("Webhook delivered successfully on first attempt: seq={}", envelope.seq);
                return Ok(());
            }
            Err((status_opt, e, _retry_after)) => {
                if let Some(status) = status_opt {
                    if self.should_not_retry(status) {
                        error!(
                            "Webhook delivery failed with non-retryable error {}: {}",
                            status, e
                        );
                        return Err(e);
                    }
                }
                last_error = Some(e);
            }
        }

        // Retry attempts with delays (add jitter)
        for (attempt, base_delay) in retry_delays.iter().enumerate() {
            // Add up to 50ms of jitter
            use rand::Rng;
            let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..=50));
            let total_delay = *base_delay + jitter;
            tokio::time::sleep(total_delay).await;

            match self
                .try_webhook_request(&client, url, envelope, &timestamp_str, &signature, &body)
                .await
            {
                Ok(()) => {
                    info!(
                        "Webhook delivered successfully on attempt {}: seq={}",
                        attempt + 2,
                        envelope.seq
                    );
                    return Ok(());
                }
                Err((status_opt, e, retry_after)) => {
                    if let Some(status) = status_opt {
                        if self.should_not_retry(status) {
                            error!(
                                "Webhook delivery failed with non-retryable error {}: {}",
                                status, e
                            );
                            return Err(e);
                        }

                        // If we got a Retry-After header, respect it for the next attempt
                        if let Some(retry_delay) = retry_after {
                            if attempt + 1 < retry_delays.len() {
                                info!("Rate limited, respecting Retry-After: {}s", retry_delay.as_secs());
                                tokio::time::sleep(retry_delay).await;
                            }
                        }
                    }
                    last_error = Some(e);
                }
            }
        }

        let final_error =
            last_error.unwrap_or_else(|| anyhow::anyhow!("All webhook attempts failed"));
        error!("Webhook delivery failed after all retries: {}", final_error);
        Err(final_error)
    }

    fn should_not_retry(&self, status: reqwest::StatusCode) -> bool {
        match status.as_u16() {
            // 2xx - Success, no retry needed
            200..=299 => true,

            // 3xx - Redirects, treat as non-retryable (webhooks shouldn't redirect)
            300..=399 => true,

            // 4xx - Client errors, mostly non-retryable except specific cases
            408 | 429 => false, // Request timeout and rate limiting are retryable
            400..=499 => true,  // Other 4xx are non-retryable

            // 5xx - Server errors, all retryable
            500..=599 => false,

            // Other status codes, treat as retryable
            _ => false,
        }
    }

    fn parse_retry_after(&self, value: &str) -> Option<Duration> {
        // Try parsing as seconds (numeric)
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }

        // Try parsing as HTTP-date
        if let Ok(datetime) =
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822)
        {
            let now = time::OffsetDateTime::now_utc();
            if datetime > now {
                let duration = datetime - now;
                // Convert to std::time::Duration, capping at reasonable max
                if let Ok(std_duration) = std::time::Duration::try_from(duration) {
                    return Some(std_duration.min(Duration::from_secs(300))); // Cap at 5 minutes
                }
            }
        }

        None
    }

    async fn try_webhook_request(
        &self,
        client: &Client,
        url: &str,
        envelope: &PlanEventEnvelope,
        timestamp_str: &str,
        signature: &str,
        body: &str,
    ) -> Result<(), (Option<reqwest::StatusCode>, anyhow::Error, Option<Duration>)> {
        let response = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Run-Id", &self.run_id)
            .header("X-Task-Id", &self.task_id)
            .header("X-Seq", envelope.seq.to_string())
            .header("X-Timestamp", timestamp_str)
            .header("X-Signature", format!("sha256={signature}"))
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| (None, anyhow::Error::from(e), None))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();

            // Check for Retry-After header on 429 responses
            let retry_after = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| self.parse_retry_after(s))
            } else {
                None
            };

            Err((
                Some(status),
                anyhow::anyhow!("Webhook failed with status {}", status),
                retry_after,
            ))
        }
    }

    fn compute_hmac(&self, secret: &str, message: &str) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).context("Invalid HMAC key")?;

        mac.update(message.as_bytes());
        let result = mac.finalize();

        Ok(hex::encode(result.into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_core::plan_tool::{PlanItemArg, StepStatus};
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_sequence_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("plan.json");

        let emitter = PlanEmitter::new(
            None,
            None,
            None,
            Some(state_path),
            "task-123".to_string(),
            "run-456".to_string(),
            "test-model".to_string(),
            false,
            false,
        )
        .unwrap();

        // Initial sequence should be 1
        assert_eq!(emitter.load_sequence().await.unwrap(), 1);

        // Persist sequence 42
        emitter.persist_sequence(42).await.unwrap();

        // Should load 43 (last_seq + 1)
        assert_eq!(emitter.load_sequence().await.unwrap(), 43);
    }

    #[tokio::test]
    async fn test_plan_state_writing() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("plan.json");

        let emitter = PlanEmitter::new(
            None,
            None,
            None,
            Some(state_path.clone()),
            "task-123".to_string(),
            "run-456".to_string(),
            "test-model".to_string(),
            false,
            false,
        )
        .unwrap();

        let plan = UpdatePlanArgs {
            explanation: Some("Test plan".to_string()),
            plan: vec![PlanItemArg {
                step: "Step 1".to_string(),
                status: StepStatus::Completed,
            }],
        };

        emitter.emit_plan_update(&plan, 1).await.unwrap();

        // Verify file was written
        assert!(state_path.exists());

        let content = fs::read_to_string(&state_path).await.unwrap();
        let parsed: PlanEventEnvelope = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed.event, "plan_update");
        assert_eq!(parsed.task_id, "task-123");
        assert_eq!(parsed.run_id, "run-456");
        assert_eq!(parsed.seq, 1);
    }

    #[test]
    fn test_hmac_computation() {
        let emitter = PlanEmitter::new(
            None,
            Some("test-secret".to_string()),
            None,
            None,
            "task-123".to_string(),
            "run-456".to_string(),
            "test-model".to_string(),
            false,
            false,
        )
        .unwrap();

        let signature = emitter
            .compute_hmac("test-secret", "v0:2025-08-14T16:12:03Z:{\"test\":\"data\"}")
            .unwrap();

        // Should produce a valid hex string
        assert_eq!(signature.len(), 64); // SHA256 = 32 bytes = 64 hex chars
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hmac_verification() {
        let emitter = PlanEmitter::new(
            None,
            None,
            None,
            None,
            "task-123".to_string(),
            "run-456".to_string(),
            "test-model".to_string(),
            false,
            false,
        )
        .unwrap();

        let secret = "test-secret";
        let message = "v0:2025-08-14T16:12:03Z:{\"event\":\"plan_update\"}";

        // Compute signature
        let signature = emitter.compute_hmac(secret, message).unwrap();

        // Verify that the same message produces the same signature
        let signature2 = emitter.compute_hmac(secret, message).unwrap();
        assert_eq!(signature, signature2);

        // Verify that different message produces different signature
        let different_message = "v0:2025-08-14T16:12:03Z:{\"event\":\"different\"}";
        let signature3 = emitter.compute_hmac(secret, different_message).unwrap();
        assert_ne!(signature, signature3);

        // Verify HMAC using the library's verification function
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        let expected_bytes = mac.finalize().into_bytes();
        let expected_hex = hex::encode(expected_bytes);

        assert_eq!(signature, expected_hex);
    }

    #[test]
    fn test_webhook_json_structure() {
        let plan = UpdatePlanArgs {
            explanation: Some("Test explanation".to_string()),
            plan: vec![
                PlanItemArg {
                    step: "Step 1".to_string(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Step 2".to_string(),
                    status: StepStatus::Pending,
                },
            ],
        };

        let envelope = PlanEventEnvelope {
            event: "plan_update".to_string(),
            run_id: "run-123".to_string(),
            task_id: "task-456".to_string(),
            seq: 7,
            ts: time::OffsetDateTime::parse(
                "2024-08-14T16:12:03Z",
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap(),
            plan,
            meta: EventMeta {
                model: "test-model".to_string(),
                codex_ver: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        let json = serde_json::to_value(&envelope).unwrap();

        // Verify top-level structure
        assert_eq!(json["event"], "plan_update");
        assert_eq!(json["run_id"], "run-123");
        assert_eq!(json["task_id"], "task-456");
        assert_eq!(json["seq"], 7);
        // Verify exact timestamp format (RFC3339)
        assert_eq!(json["ts"], "2024-08-14T16:12:03Z");

        // Verify nested plan object (not flattened)
        let plan_obj = &json["plan"];
        assert!(plan_obj.is_object(), "plan should be a nested object");
        assert_eq!(plan_obj["explanation"], "Test explanation");

        let plan_array = &plan_obj["plan"];
        assert!(plan_array.is_array(), "plan.plan should be an array");
        assert_eq!(plan_array.as_array().unwrap().len(), 2);

        // Verify plan items structure
        assert_eq!(plan_array[0]["step"], "Step 1");
        assert_eq!(plan_array[0]["status"], "in_progress");
        assert_eq!(plan_array[1]["step"], "Step 2");
        assert_eq!(plan_array[1]["status"], "pending");

        // Verify meta object
        assert_eq!(json["meta"]["model"], "test-model");
        assert_eq!(json["meta"]["codex_ver"], env!("CARGO_PKG_VERSION"));
    }
}
