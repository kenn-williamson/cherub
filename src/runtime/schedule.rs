//! Schedule triggers: cron-based proactive agent behavior (feature-gated).
//!
//! The `ScheduleRunner` parses cron expressions at startup and sleeps until
//! the next trigger fires, then sends a `ScheduledMessage` via the provided
//! channel. The consumer (CLI or Telegram) wraps it into a user turn.

use std::str::FromStr;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::error::CherubError;

/// A single schedule entry from the config file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEntry {
    pub name: String,
    pub cron: String,
    pub message: String,
}

/// Top-level schedule config (TOML).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    pub schedules: Vec<ScheduleEntry>,
}

impl ScheduleConfig {
    /// Load schedule config from a TOML file.
    pub fn load(path: &str) -> Result<Self, CherubError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| CherubError::Config(format!("failed to read schedule config: {e}")))?;
        if content.len() > 64 * 1024 {
            return Err(CherubError::Config(
                "schedule config exceeds 64 KiB limit".to_owned(),
            ));
        }
        toml::from_str(&content)
            .map_err(|e| CherubError::Config(format!("invalid schedule config: {e}")))
    }
}

/// A parsed schedule ready to run.
#[derive(Debug)]
pub struct ParsedSchedule {
    pub name: String,
    pub cron: cron::Schedule,
    pub message: String,
}

/// Parse and validate all schedule entries at startup.
pub fn parse_entries(entries: &[ScheduleEntry]) -> Result<Vec<ParsedSchedule>, CherubError> {
    entries
        .iter()
        .map(|entry| {
            let cron = cron::Schedule::from_str(&entry.cron).map_err(|e| {
                CherubError::Config(format!(
                    "invalid cron expression '{}' for schedule '{}': {e}",
                    entry.cron, entry.name
                ))
            })?;
            Ok(ParsedSchedule {
                name: entry.name.clone(),
                cron,
                message: entry.message.clone(),
            })
        })
        .collect()
}

/// A message emitted when a schedule trigger fires.
#[derive(Debug, Clone)]
pub struct ScheduledMessage {
    pub name: String,
    pub message: String,
}

/// Run schedule triggers. Sends messages via the provided sender when cron fires.
/// Returns when the sender's receiver is dropped (session ended) or no schedules exist.
pub async fn schedule_runner(schedules: Vec<ParsedSchedule>, tx: mpsc::Sender<ScheduledMessage>) {
    if schedules.is_empty() {
        return;
    }

    info!(count = schedules.len(), "schedule runner started");

    loop {
        // Find the next trigger across all schedules.
        let now = chrono::Utc::now();
        let next = schedules
            .iter()
            .filter_map(|s| s.cron.upcoming(chrono::Utc).next().map(|t| (t, s)))
            .min_by_key(|(t, _)| *t);

        match next {
            Some((when, schedule)) => {
                let delay = (when - now).to_std().unwrap_or(Duration::ZERO);

                info!(
                    schedule = %schedule.name,
                    next_fire = %when.format("%Y-%m-%d %H:%M:%S UTC"),
                    delay_secs = delay.as_secs(),
                    "waiting for next trigger"
                );

                tokio::time::sleep(delay).await;

                info!(schedule = %schedule.name, "schedule triggered");

                if tx
                    .send(ScheduledMessage {
                        name: schedule.name.clone(),
                        message: schedule.message.clone(),
                    })
                    .await
                    .is_err()
                {
                    info!("schedule runner stopping — receiver dropped");
                    break;
                }
            }
            None => {
                warn!("no upcoming triggers found, schedule runner stopping");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_cron() {
        let entries = vec![ScheduleEntry {
            name: "test".to_owned(),
            cron: "0 9 * * * *".to_owned(),
            message: "hello".to_owned(),
        }];
        let parsed = parse_entries(&entries).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "test");
    }

    #[test]
    fn parse_invalid_cron_rejects() {
        let entries = vec![ScheduleEntry {
            name: "bad".to_owned(),
            cron: "not a cron".to_owned(),
            message: "hello".to_owned(),
        }];
        let result = parse_entries(&entries);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("bad"), "error should name the schedule: {err}");
    }

    #[test]
    fn parse_multiple_schedules() {
        let entries = vec![
            ScheduleEntry {
                name: "hourly".to_owned(),
                cron: "0 0 * * * *".to_owned(),
                message: "hourly check".to_owned(),
            },
            ScheduleEntry {
                name: "daily".to_owned(),
                cron: "0 0 9 * * *".to_owned(),
                message: "daily report".to_owned(),
            },
        ];
        let parsed = parse_entries(&entries).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn schedule_config_from_toml() {
        let toml_str = r#"
[[schedules]]
name = "test"
cron = "0 0 9 * * *"
message = "Check status"

[[schedules]]
name = "hourly"
cron = "0 0 * * * *"
message = "Summarize"
"#;
        let config: ScheduleConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.schedules.len(), 2);
        assert_eq!(config.schedules[0].name, "test");
    }

    #[tokio::test]
    async fn schedule_runner_stops_on_drop() {
        let entries = vec![ScheduleEntry {
            name: "test".to_owned(),
            // Every second (for testing) — cron crate uses 7-field format
            cron: "* * * * * * *".to_owned(),
            message: "hello".to_owned(),
        }];
        let parsed = parse_entries(&entries).unwrap();

        let (tx, mut rx) = mpsc::channel(10);

        let handle = tokio::spawn(async move {
            schedule_runner(parsed, tx).await;
        });

        // Should receive at least one message quickly.
        let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("should receive within timeout")
            .expect("should get a message");
        assert_eq!(msg.name, "test");
        assert_eq!(msg.message, "hello");

        // Drop receiver to stop runner.
        drop(rx);

        // Runner should exit cleanly.
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("runner should stop within timeout")
            .expect("runner should not panic");
    }

    #[tokio::test]
    async fn schedule_runner_empty_schedules_returns() {
        let (tx, _rx) = mpsc::channel(10);
        // Should return immediately with empty schedules.
        tokio::time::timeout(Duration::from_secs(1), schedule_runner(Vec::new(), tx))
            .await
            .expect("should return immediately");
    }
}
