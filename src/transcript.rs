use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

pub struct Transcript {
    file: File,
}

impl Transcript {
    pub fn create(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(Self {
            file: options
                .open(path)
                .with_context(|| format!("create transcript {}", path.display()))?,
        })
    }

    pub fn record(&mut self, kind: &str, data: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.file, &json!({"kind": kind, "data": data}))
            .context("serialize transcript event")?;
        self.file.write_all(b"\n").context("write transcript")?;
        self.file.flush().context("flush transcript")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_one_json_value_per_line_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut transcript = Transcript::create(&path).unwrap();
        transcript
            .record("input", &json!({"text": "first\nsecond"}))
            .unwrap();
        transcript.record("finished", &json!({})).unwrap();
        assert!(Transcript::create(&path).is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["data"]["text"], "first\nsecond");
    }
}
