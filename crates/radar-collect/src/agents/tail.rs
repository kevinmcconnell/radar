//! Reads the lines appended to a growing file since the last read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How far back a file is read when first seen.
pub const TAIL_BYTES: u64 = 64 * 1024;
const MAX_READ: u64 = 4 << 20;
const BUF_KEEP: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    /// Bytes processed so far; at a line boundary once `aligned`.
    offset: u64,
    aligned: bool,
}

impl Tail {
    /// Starts at the beginning of the file.
    pub fn from_start() -> Self {
        Tail {
            offset: 0,
            aligned: true,
        }
    }

    /// Starts within the last `TAIL_BYTES` of a file of `len` bytes, skipping the partial
    /// line the cut lands in.
    pub fn from_end(len: u64) -> Self {
        let offset = len.saturating_sub(TAIL_BYTES);
        Tail {
            offset,
            aligned: offset == 0,
        }
    }

    /// Whether a file now `len` bytes long has unread data. A shorter file was replaced
    /// and is read again from its start.
    pub fn pending(&mut self, len: u64) -> bool {
        if len < self.offset {
            *self = Tail::from_start();
        }
        len > self.offset
    }

    /// Calls `line` for each complete line appended up to `len`, reading through `buf`.
    /// A line longer than the read limit is skipped. An unaligned read starts one byte
    /// early, so a cut that landed on a line boundary keeps that line.
    pub fn read(&mut self, path: &Path, len: u64, buf: &mut Vec<u8>, mut line: impl FnMut(&[u8])) {
        let Ok(mut file) = File::open(path) else {
            return;
        };
        let start = if self.aligned {
            self.offset
        } else {
            self.offset.saturating_sub(1)
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        buf.clear();
        let want = len.saturating_sub(start).min(MAX_READ);
        if file.take(want).read_to_end(buf).is_err() {
            return;
        }
        let mut chunk = &buf[..];
        let mut consumed = 0;
        if !self.aligned {
            let Some(nl) = chunk.iter().position(|&b| b == b'\n') else {
                self.offset = start + chunk.len() as u64;
                return;
            };
            consumed = nl + 1;
            chunk = &chunk[consumed..];
            self.aligned = true;
        }
        match chunk.iter().rposition(|&b| b == b'\n') {
            Some(end) => {
                for l in chunk[..end].split(|&b| b == b'\n') {
                    line(l);
                }
                self.offset = start + (consumed + end + 1) as u64;
            }
            None if chunk.len() as u64 >= MAX_READ => {
                self.offset = start + (consumed + chunk.len()) as u64;
                self.aligned = false;
            }
            None => self.offset = start + consumed as u64,
        }
        buf.shrink_to(BUF_KEEP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("radar-tail-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn lines(tail: &mut Tail, path: &Path) -> Vec<String> {
        let len = std::fs::metadata(path).unwrap().len();
        let mut out = Vec::new();
        if tail.pending(len) {
            tail.read(path, len, &mut Vec::new(), |l| {
                out.push(String::from_utf8_lossy(l).into_owned())
            });
        }
        out
    }

    #[test]
    fn reads_only_complete_new_lines() {
        let path = scratch("lines");
        let mut f = File::create(&path).unwrap();
        write!(f, "one\ntwo\nthr").unwrap();
        let mut tail = Tail::from_start();
        assert_eq!(lines(&mut tail, &path), vec!["one", "two"]);
        assert_eq!(lines(&mut tail, &path), Vec::<String>::new());
        write!(f, "ee\nfour\n").unwrap();
        assert_eq!(lines(&mut tail, &path), vec!["three", "four"]);
        std::fs::write(&path, "new\n").unwrap();
        assert_eq!(
            lines(&mut tail, &path),
            vec!["new"],
            "a shorter file restarts"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn starting_from_the_end_skips_the_cut_line() {
        let path = scratch("end");
        let mut f = File::create(&path).unwrap();
        let filler = "x".repeat(TAIL_BYTES as usize);
        write!(f, "{filler}\n{filler}mid\nlast\n").unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let mut tail = Tail::from_end(len);
        assert_eq!(lines(&mut tail, &path), vec!["last"]);

        let mut f = File::create(&path).unwrap();
        write!(f, "head\n{}\n", "b".repeat(TAIL_BYTES as usize - 1)).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let mut tail = Tail::from_end(len);
        assert_eq!(
            lines(&mut tail, &path),
            vec!["b".repeat(TAIL_BYTES as usize - 1)],
            "a cut on a line boundary keeps that line"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn oversized_lines_are_skipped() {
        let path = scratch("huge");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "first").unwrap();
        let huge = vec![b'y'; MAX_READ as usize + 10];
        f.write_all(&huge).unwrap();
        write!(f, "\nafter\n").unwrap();
        let mut tail = Tail::from_start();
        assert_eq!(lines(&mut tail, &path), vec!["first"]);
        assert_eq!(lines(&mut tail, &path), Vec::<String>::new());
        assert_eq!(lines(&mut tail, &path), vec!["after"]);
        let _ = std::fs::remove_file(&path);
    }
}
