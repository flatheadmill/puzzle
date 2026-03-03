// File tailer for viewer mode. Polls a JSONL transcript at 100ms intervals,
// reads new bytes, and sends parsed ConversationEntry values over a channel.
//
// In viewer mode, Puzzle is reading Claude's raw JSONL directly — there is
// no Wicket in the loop. The tailer does a minimal parse: it reads each line
// as a JSON value and uses ConversationEntry::from_value to convert. Entries
// that don't convert (system, progress, sidechains) are silently dropped.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::model::ConversationEntry;

pub async fn run_tailer(
    path: PathBuf,
    tx: mpsc::Sender<ConversationEntry>,
) -> color_eyre::Result<()> {
    // Poll for the file to appear.
    loop {
        if path.exists() {
            break;
        }
        time::sleep(Duration::from_millis(100)).await;
    }

    let mut file = std::fs::File::open(&path)?;
    let mut pos: u64 = 0;
    let mut buf = String::new();
    let mut interval = time::interval(Duration::from_millis(100));

    loop {
        interval.tick().await;

        let metadata = std::fs::metadata(&path)?;
        let size = metadata.len();

        if size <= pos {
            continue;
        }

        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)?;
        pos = file.stream_position()?;

        buf.push_str(&chunk);

        while let Some(newline_pos) = buf.find('\n') {
            let line = buf[..newline_pos].to_string();
            buf = buf[newline_pos + 1..].to_string();

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Viewer mode: parse raw JSONL lines directly. The entry
            // needs to look like what Wicket would send, but here we
            // are reading Claude's raw format. For now, viewer mode
            // won't render these — it would need the old parser. This
            // is a known limitation until viewer mode is either removed
            // or updated to talk to Wicket.
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(ce) = ConversationEntry::from_value(value) {
                    if tx.send(ce).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}
