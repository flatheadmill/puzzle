use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::model::{try_convert, ConversationEntry};
use crate::parser::parse_line;

pub async fn run_tailer(
    path: PathBuf,
    tx: mpsc::Sender<ConversationEntry>,
) -> color_eyre::Result<()> {
    // poll for the file to appear (REPL mode creates it on first call)
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

            if let Some(entry) = parse_line(line) {
                if let Some(ce) = try_convert(entry) {
                    if tx.send(ce).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}
