// Exchange log: per-slug JSONL of every message in both directions.
// Same pattern as Wicket's exchange log.

use serde_json::Value;

pub struct ExchangeLog {
    path: std::path::PathBuf,
}

impl ExchangeLog {
    pub fn new(slug: &str) -> Self {
        let home = std::env::var("HOME").expect("HOME not set");
        let dir = std::path::Path::new(&home)
            .join(".local/state/puzzle")
            .join(slug);
        let _ = std::fs::create_dir_all(&dir);
        Self {
            path: dir.join("puzzle-exchange.jsonl"),
        }
    }

    pub fn log(&self, dir: &str, data: &Value) {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let entry = serde_json::json!({
            "ts": now,
            "dir": dir,
            "data": data,
        });
        if let Ok(mut line) = serde_json::to_string(&entry) {
            line.push('\n');
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = std::io::Write::write_all(&mut file, line.as_bytes());
            }
        }
    }
}
