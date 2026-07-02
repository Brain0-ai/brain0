//! Append-only JSONL reading: only complete new lines from a byte cursor.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::Result;

/// Maximum size of a single transcript line we will parse. Transcripts are untrusted input
///; an absurdly long line is skipped rather than buffered into a giant
/// allocation, but the cursor still advances past it.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// A line read from a JSONL file, with its starting byte offset (for provenance).
#[derive(Debug, Clone)]
pub struct Line {
    pub offset: u64,
    pub text: String,
}

/// Read complete lines from `path` starting at byte `from`. A trailing partial line (no
/// final newline, i.e. a transcript still being written) is left unconsumed: `new_offset`
/// only advances past complete lines, so the next read resumes cleanly.
pub fn read_complete_lines(path: &Path, from: u64) -> Result<(Vec<Line>, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if from >= len {
        return Ok((Vec::new(), from));
    }
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let mut lines = Vec::new();
    let mut line_start = 0usize;
    let mut consumed = 0usize;
    for (i, &byte) in buf.iter().enumerate() {
        if byte == b'\n' {
            let raw = &buf[line_start..i];
            // Untrusted input: skip pathologically long lines (cursor still advances).
            if raw.len() <= MAX_LINE_BYTES {
                let text = String::from_utf8_lossy(raw)
                    .trim_end_matches('\r')
                    .to_string();
                lines.push(Line {
                    offset: from + line_start as u64,
                    text,
                });
            }
            consumed = i + 1;
            line_start = i + 1;
        }
    }
    Ok((lines, from + consumed as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_only_complete_lines_and_resumes() {
        let path = std::env::temp_dir().join(format!("brain0-jsonl-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut f = File::create(&path).unwrap();
            // two complete lines + a partial (no trailing newline)
            f.write_all(b"{\"a\":1}\n{\"b\":2}\n{\"partial\":").unwrap();
        }
        let (lines, offset) = read_complete_lines(&path, 0).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].offset, 0);
        assert_eq!(lines[0].text, "{\"a\":1}");

        // Re-reading from the cursor yields nothing new yet (partial not complete).
        let (none, offset2) = read_complete_lines(&path, offset).unwrap();
        assert!(none.is_empty());
        assert_eq!(offset2, offset);

        // Complete the partial line and append another → only the new lines come back.
        {
            use std::fs::OpenOptions;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"3}\n{\"c\":4}\n").unwrap();
        }
        let (more, _) = read_complete_lines(&path, offset).unwrap();
        assert_eq!(more.len(), 2);
        assert_eq!(more[0].text, "{\"partial\":3}");
        assert_eq!(more[1].text, "{\"c\":4}");

        let _ = std::fs::remove_file(&path);
    }
}
