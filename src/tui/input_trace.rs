use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::event::Event;
use serde_json::json;

use super::Focus;

const TRACE_ENV: &str = "LCTR_INPUT_TRACE";
const TRACE_SCHEMA_VERSION: u8 = 1;

pub(super) struct InputTrace {
    writer: Option<TraceWriter>,
}

struct TraceWriter {
    file: File,
    session_start: Instant,
    sequence: u64,
}

impl InputTrace {
    pub(super) fn from_env() -> Result<Self> {
        let path = std::env::var_os(TRACE_ENV);
        Self::new(path.as_deref().map(Path::new))
    }

    fn new(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self { writer: None });
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create input trace {}", path.display()))?;
        let session_start = Instant::now();
        let header = json!({
            "record": "session_start",
            "schema_version": TRACE_SCHEMA_VERSION,
            "lctr_version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "term": std::env::var_os("TERM").and_then(|value| value.into_string().ok()),
            "term_program": std::env::var_os("TERM_PROGRAM").and_then(|value| value.into_string().ok()),
            "monotonic_start_micros": 0,
        });
        write_record(&mut file, &header, path)?;
        Ok(Self {
            writer: Some(TraceWriter {
                file,
                session_start,
                sequence: 0,
            }),
        })
    }

    pub(super) fn record(
        &mut self,
        event: &Event,
        focus: Focus,
        query: &str,
        cursor_column: usize,
        selected_result_index: Option<usize>,
    ) -> Result<()> {
        let Some(writer) = &mut self.writer else {
            return Ok(());
        };
        writer.sequence += 1;
        let record = json!({
            "record": "event",
            "sequence": writer.sequence,
            "elapsed_micros": writer.session_start.elapsed().as_micros(),
            "event": format!("{event:?}"),
            "focus": focus_name(focus),
            "query": query,
            "cursor_column": cursor_column,
            "selected_result_index": selected_result_index,
        });
        write_record(&mut writer.file, &record, Path::new("input trace"))
    }
}

fn focus_name(focus: Focus) -> &'static str {
    match focus {
        Focus::Search => "search",
        Focus::Results => "results",
    }
}

fn write_record(file: &mut File, record: &serde_json::Value, path: &Path) -> Result<()> {
    serde_json::to_writer(&mut *file, record)
        .with_context(|| format!("write input trace {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write input trace {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush input trace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use serde_json::Value;
    use tempfile::tempdir;

    use super::InputTrace;
    use crate::tui::Focus;

    fn lines(path: &std::path::Path) -> Vec<Value> {
        fs::read_to_string(path)
            .expect("read trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL"))
            .collect()
    }

    #[test]
    fn disabled_trace_creates_no_file_or_writes() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        let mut trace = InputTrace::new(None).expect("disabled trace");

        trace
            .record(
                &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                Focus::Search,
                "query",
                5,
                Some(2),
            )
            .expect("record disabled trace");

        assert!(!path.exists());
    }

    #[test]
    fn key_event_is_valid_jsonl_with_state_and_flushes_immediately() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        let mut trace = InputTrace::new(Some(&path)).expect("create trace");

        trace
            .record(
                &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                Focus::Results,
                "needle",
                4,
                Some(3),
            )
            .expect("record key");

        let records = lines(&path);
        assert_eq!(records.len(), 2);
        let event = &records[1];
        assert_eq!(event["sequence"], 1);
        assert!(event["elapsed_micros"].is_u64());
        assert_eq!(event["focus"], "results");
        assert_eq!(event["query"], "needle");
        assert_eq!(event["cursor_column"], 4);
        assert_eq!(event["selected_result_index"], 3);
        assert!(event["event"].as_str().expect("event text").contains("Key"));
    }

    #[test]
    fn non_key_event_families_serialize() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        let mut trace = InputTrace::new(Some(&path)).expect("create trace");
        let events = [
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Paste("pasted".to_string()),
            Event::FocusGained,
            Event::Resize(120, 40),
        ];

        for event in &events {
            trace
                .record(event, Focus::Search, "query", 5, None)
                .expect("record event");
        }

        let records = lines(&path);
        assert_eq!(records.len(), 5);
        for record in &records[1..] {
            assert!(record["event"].is_string());
        }
    }

    #[test]
    fn existing_trace_path_is_rejected_without_truncation() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        fs::write(&path, "existing\n").expect("seed trace");

        let error = match InputTrace::new(Some(&path)) {
            Ok(_) => panic!("existing trace accepted"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("create input trace"));
        assert_eq!(
            fs::read_to_string(&path).expect("read existing trace"),
            "existing\n"
        );
    }

    #[test]
    fn sequence_increments_for_consecutive_records() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        let mut trace = InputTrace::new(Some(&path)).expect("create trace");
        let event = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        trace
            .record(&event, Focus::Search, "one", 3, None)
            .expect("first record");
        trace
            .record(&event, Focus::Results, "two", 3, Some(1))
            .expect("second record");

        let records = lines(&path);
        assert_eq!(records[1]["sequence"], 1);
        assert_eq!(records[2]["sequence"], 2);
    }
}
