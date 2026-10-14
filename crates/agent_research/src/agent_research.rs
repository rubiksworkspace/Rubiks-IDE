//! Research records the IDE writes for Prime Agent sessions: how agent edits
//! were reviewed, how long accepted agent lines survive and where the code
//! came from, and the session survey. Records go to Prime Agent's research
//! directory, `<agent dir>/research/ide/<ACP session id>.jsonl`, where
//! `prime-agent data research` joins them to the session they belong to.
//!
//! The IDE keeps no code here: accepted lines are remembered as hashes, and
//! survival is measured by finding those hashes in the file later. Writes
//! happen on a background thread; nothing waits on the disk.
//!
//! Prime Agent's `research` settings apply: `enabled: false` (or
//! `PRIME_AGENT_RESEARCH=0`) turns these records off too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How long after acceptance each survival check runs.
const CHECKPOINTS: [(&str, u64); 3] = [("1h", 3600), ("1d", 86_400), ("1w", 604_800)];
/// How often the survival checks look for due checkpoints.
const CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const SURVIVAL_FILE: &str = "survival-pending.jsonl";

enum Job {
    Record {
        acp_session_id: String,
        record: Value,
    },
    Track(Value),
    CheckSurvival,
}

/// Prime Agent's directory: `PRIME_AGENT_CODING_AGENT_DIR`, else
/// `~/.prime/agent`.
fn agent_dir() -> PathBuf {
    match std::env::var_os("PRIME_AGENT_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => util::paths::home_dir().join(".prime").join("agent"),
    }
}

fn ide_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("research").join("ide")
}

/// Whether Prime Agent's research log is on (read once per IDE run).
fn enabled(agent_dir: &Path) -> bool {
    match std::env::var("PRIME_AGENT_RESEARCH")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("0" | "false" | "no" | "off") => return false,
        Some("1" | "true" | "yes" | "on") => return true,
        _ => {}
    }
    std::fs::read_to_string(agent_dir.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|settings| settings["research"]["enabled"].as_bool())
        .unwrap_or(true)
}

/// The writer, started on first use; `None` when the research log is off.
fn writer() -> Option<&'static Sender<Job>> {
    static WRITER: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let agent_dir = agent_dir();
            if cfg!(feature = "test-support") || !enabled(&agent_dir) {
                return None;
            }
            let (sender, receiver) = mpsc::channel::<Job>();
            let spawned = std::thread::Builder::new()
                .name("agent-research".to_string())
                .spawn(move || {
                    for job in receiver {
                        if let Err(error) = run_job(&agent_dir, job, now_seconds()) {
                            log::debug!("agent research write failed: {error}");
                        }
                    }
                });
            if let Err(error) = spawned {
                log::debug!("agent research writer did not start: {error}");
                return None;
            }
            let ticker = sender.clone();
            let ticking = std::thread::Builder::new()
                .name("agent-research-survival".to_string())
                .spawn(move || {
                    while ticker.send(Job::CheckSurvival).is_ok() {
                        std::thread::sleep(CHECK_INTERVAL);
                    }
                });
            if let Err(error) = ticking {
                log::debug!("agent research survival checks did not start: {error}");
            }
            Some(sender)
        })
        .as_ref()
}

/// Write one record for the ACP session `acp_session_id`.
pub fn record(acp_session_id: &str, event_type: &str, data: Value) {
    if let Some(writer) = writer() {
        // A stopped writer only loses research records.
        writer
            .send(Job::Record {
                acp_session_id: acp_session_id.to_string(),
                record: envelope(acp_session_id, event_type, data, now_seconds()),
            })
            .ok();
    }
}

/// Remember agent lines the user just accepted in `path`, so the survival
/// checks can look for them after an hour, a day and a week. Blank lines
/// are not counted.
pub fn track_accepted_lines(acp_session_id: &str, path: &Path, text: &str) {
    let hashes: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| format!("{:016x}", line_hash(line)))
        .collect();
    if hashes.is_empty() {
        return;
    }
    if let Some(writer) = writer() {
        writer
            .send(Job::Track(json!({
                "acpSessionId": acp_session_id,
                "path": path,
                "acceptedAt": now_seconds(),
                "lineHashes": hashes,
                "done": [],
            })))
            .ok();
    }
}

fn run_job(agent_dir: &Path, job: Job, now: u64) -> std::io::Result<()> {
    match job {
        Job::Record {
            acp_session_id,
            record,
        } => append(&session_file(agent_dir, &acp_session_id), &record),
        Job::Track(entry) => append(&ide_dir(agent_dir).join(SURVIVAL_FILE), &entry),
        Job::CheckSurvival => check_survival(agent_dir, now),
    }
}

/// Run every survival checkpoint that is due, writing `diff.survival` and
/// `code.origin`, and keep the entries that still have checkpoints ahead.
fn check_survival(agent_dir: &Path, now: u64) -> std::io::Result<()> {
    let path = ide_dir(agent_dir).join(SURVIVAL_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut remaining = Vec::new();
    let mut changed = false;
    for line in text.lines() {
        let Ok(mut entry) = serde_json::from_str::<Value>(line) else {
            changed = true;
            continue;
        };
        let accepted_at = entry["acceptedAt"].as_u64().unwrap_or(now);
        for (checkpoint, after) in CHECKPOINTS {
            let done = entry["done"]
                .as_array()
                .is_some_and(|done| done.iter().any(|item| item == checkpoint));
            if done || now < accepted_at + after {
                continue;
            }
            for (event_type, data) in survival_records(&entry, checkpoint) {
                let acp_session_id = entry["acpSessionId"].as_str().unwrap_or("unknown");
                append(
                    &session_file(agent_dir, acp_session_id),
                    &envelope(acp_session_id, event_type, data, now),
                )?;
            }
            if let Some(done) = entry["done"].as_array_mut() {
                done.push(json!(checkpoint));
            }
            changed = true;
        }
        let finished = entry["done"]
            .as_array()
            .is_some_and(|done| done.len() >= CHECKPOINTS.len());
        if !finished {
            remaining.push(entry);
        }
    }
    if changed {
        let mut text = String::new();
        for entry in remaining {
            text.push_str(&entry.to_string());
            text.push('\n');
        }
        let partial = path.with_extension("partial");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, &path)?;
    }
    Ok(())
}

/// `diff.survival` and `code.origin` for one checkpoint of one accepted hunk.
fn survival_records(entry: &Value, checkpoint: &str) -> [(&'static str, Value); 2] {
    let file_path = PathBuf::from(entry["path"].as_str().unwrap_or_default());
    let accepted: Vec<&str> = entry["lineHashes"]
        .as_array()
        .map(|hashes| hashes.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let current = std::fs::read_to_string(&file_path).ok();
    let mut present: HashMap<String, usize> = HashMap::new();
    for line in current.as_deref().unwrap_or_default().lines() {
        *present
            .entry(format!("{:016x}", line_hash(line)))
            .or_default() += 1;
    }
    let surviving = accepted
        .iter()
        .filter(|hash| match present.get_mut(**hash) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        })
        .count();
    let origin = if surviving == accepted.len() {
        "agent"
    } else if surviving == 0 {
        "human"
    } else {
        "mixed"
    };
    let accepted_at = entry["acceptedAt"].as_u64().unwrap_or_default();
    [
        (
            "diff.survival",
            json!({
                "path": file_path,
                "checkpoint": checkpoint,
                "acceptedAt": iso(accepted_at),
                "acceptedLines": accepted.len(),
                "survivingLines": surviving,
                "survivalRate": if accepted.is_empty() { 1.0 } else { surviving as f64 / accepted.len() as f64 },
                "fileExists": current.is_some(),
            }),
        ),
        (
            "code.origin",
            json!({
                "path": file_path,
                "checkpoint": checkpoint,
                "origin": origin,
                "agentLines": surviving,
                "changedOrRemovedLines": accepted.len() - surviving,
            }),
        ),
    ]
}

fn envelope(acp_session_id: &str, event_type: &str, data: Value, now: u64) -> Value {
    json!({
        "ts": iso(now),
        "event_type": event_type,
        "source": "ide",
        "acp_session_id": acp_session_id,
        "data": data,
    })
}

fn session_file(agent_dir: &Path, acp_session_id: &str) -> PathBuf {
    let stem: String = acp_session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    ide_dir(agent_dir).join(format!("{stem}.jsonl"))
}

fn append(path: &Path, record: &Value) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = record.to_string();
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// FNV-1a over the line without its line ending: stable across Rust
/// versions, unlike the standard hasher.
fn line_hash(line: &str) -> u64 {
    line.trim_end_matches('\r')
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn iso(seconds: u64) -> String {
    chrono::DateTime::from_timestamp(i64::try_from(seconds).unwrap_or(i64::MAX), 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(agent_dir: &Path, acp_session_id: &str) -> Vec<Value> {
        std::fs::read_to_string(session_file(agent_dir, acp_session_id))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn survival_checkpoints_measure_what_is_left_of_accepted_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\n\nfn c() {}\n").unwrap();
        let hashes: Vec<String> = ["fn a() {}", "fn b() {}", "fn c() {}"]
            .iter()
            .map(|line| format!("{:016x}", line_hash(line)))
            .collect();
        run_job(
            dir.path(),
            Job::Track(json!({
                "acpSessionId": "acp-1", "path": file, "acceptedAt": 1000,
                "lineHashes": hashes, "done": [],
            })),
            1000,
        )
        .unwrap();

        // Not due yet: nothing written.
        run_job(dir.path(), Job::CheckSurvival, 1000 + 60).unwrap();
        assert!(records(dir.path(), "acp-1").is_empty());

        // The user rewrote one line within the hour.
        std::fs::write(&file, "fn a() {}\nfn b2() {}\n\nfn c() {}\n").unwrap();
        run_job(dir.path(), Job::CheckSurvival, 1000 + 3600).unwrap();
        let after_an_hour = records(dir.path(), "acp-1");
        assert_eq!(after_an_hour.len(), 2);
        assert_eq!(after_an_hour[0]["data"]["survivingLines"], 2);
        assert_eq!(after_an_hour[1]["data"]["origin"], "mixed");

        // A week later both remaining checkpoints run, then the entry is gone.
        std::fs::remove_file(&file).unwrap();
        run_job(dir.path(), Job::CheckSurvival, 1000 + 604_800).unwrap();
        let all = records(dir.path(), "acp-1");
        let checkpoints: Vec<(&str, &str)> = all
            .iter()
            .map(|record| {
                (
                    record["event_type"].as_str().unwrap(),
                    record["data"]["checkpoint"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            checkpoints,
            [
                ("diff.survival", "1h"),
                ("code.origin", "1h"),
                ("diff.survival", "1d"),
                ("code.origin", "1d"),
                ("diff.survival", "1w"),
                ("code.origin", "1w"),
            ]
        );
        assert_eq!(all[4]["data"]["fileExists"], false);
        assert_eq!(all[5]["data"]["origin"], "human");
        assert_eq!(
            std::fs::read_to_string(ide_dir(dir.path()).join(SURVIVAL_FILE)).unwrap(),
            ""
        );
    }
}
