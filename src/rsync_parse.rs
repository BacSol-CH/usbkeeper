//! usbkeeper — rsync progress-line parsing (pure, unit-tested).
//!
//! `rsync -aH --info=progress2` emits lines like:
//!   "  1,234,567,890  45%   12.34MB/s    0:01:23 (xfr#123, ir-chk=1005/2010)"
//! separated by \r (in-place updates) or \n. We parse the first three fields.

#[derive(Debug, PartialEq)]
pub struct Progress {
    pub bytes_done: u64,
    pub pct: f64,
    pub bytes_per_sec: Option<u64>,
}

pub fn parse_progress_line(line: &str) -> Option<Progress> {
    let line = line.trim();
    if line.is_empty() || !line.contains('%') {
        return None;
    }
    // split on whitespace, tolerate thousands separators in the byte count
    let mut it = line.split_whitespace();
    let first = it.next()?;
    let bytes_done = parse_size(first)?;
    let pct_tok = it.next()?;
    let pct: f64 = pct_tok.trim_end_matches('%').parse().ok()?;
    let rate_tok = it.next();
    let bytes_per_sec = rate_tok.and_then(parse_rate);
    Some(Progress {
        bytes_done,
        pct,
        bytes_per_sec,
    })
}

/// rsync >=3.5 prints byte counts with a unit suffix and thousands
/// separators: "32,768B", "209,715,200B", "1.2MB", "2.44GB".
/// Older rsync prints a bare number: "1,234,567,890".
fn parse_size(tok: &str) -> Option<u64> {
    let t = tok.replace(',', "");
    let (num, mult) = if let Some(n) = t.strip_suffix("TB") {
        (n, 1_000_000_000_000f64)
    } else if let Some(n) = t.strip_suffix("GB") {
        (n, 1_000_000_000f64)
    } else if let Some(n) = t.strip_suffix("MB") {
        (n, 1_000_000f64)
    } else if let Some(n) = t.strip_suffix("kB") {
        (n, 1_000f64)
    } else if let Some(n) = t.strip_suffix("B") {
        (n, 1f64)
    } else {
        (t.as_str(), 1f64)
    };
    if num.is_empty() {
        return None;
    }
    let v: f64 = num.parse().ok()?;
    Some((v * mult) as u64)
}

/// "12.34MB/s" | "987kB/s" | "1.5GB/s" | "0.00kB/s" -> bytes/sec
fn parse_rate(tok: &str) -> Option<u64> {
    let t = tok.trim_end_matches("/s");
    let (num, mult) = if let Some(n) = t.strip_suffix("TB") {
        (n, 1_000_000_000_000u64)
    } else if let Some(n) = t.strip_suffix("GB") {
        (n, 1_000_000_000)
    } else if let Some(n) = t.strip_suffix("MB") {
        (n, 1_000_000)
    } else if let Some(n) = t.strip_suffix("kB") {
        (n, 1_000)
    } else if let Some(n) = t.strip_suffix("B") {
        (n, 1)
    } else {
        return None;
    };
    let v: f64 = num.parse().ok()?;
    Some((v * mult as f64) as u64)
}

/// Feed a raw chunk of rsync stdout (may contain many \r-separated updates);
/// returns the LAST complete progress sample found, if any.
pub fn last_progress(buf: &str) -> Option<Progress> {
    buf.rsplit(|c| c == '\r' || c == '\n')
        .find_map(parse_progress_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_line() {
        let p = parse_progress_line("  1,234,567  45%   12.34MB/s    0:00:10 (xfr#1, to-chk=2/3)").unwrap();
        assert_eq!(p.bytes_done, 1_234_567);
        assert!((p.pct - 45.0).abs() < 1e-9);
        assert_eq!(p.bytes_per_sec, Some(12_340_000));
    }

    #[test]
    fn zero_rate_and_kb() {
        let p = parse_progress_line("0  0%    0.00kB/s    0:00:00 (xfr#0, to-chk=0/1)").unwrap();
        assert_eq!(p.bytes_done, 0);
        assert_eq!(p.bytes_per_sec, Some(0));
    }

    #[test]
    fn non_progress_lines_rejected() {
        assert!(parse_progress_line("sending incremental file list").is_none());
        assert!(parse_progress_line("").is_none());
        assert!(parse_progress_line("rsync error: some files exited").is_none());
    }

    #[test]
    fn rsync_35_suffixed_sizes() {
        // rsync 3.5.2 real output format (unit suffix on the byte count)
        let p = parse_progress_line("         32,768B   0%       0B/s    0:00:00").unwrap();
        assert_eq!(p.bytes_done, 32768);
        assert_eq!(p.pct, 0.0);
        assert_eq!(p.bytes_per_sec, Some(0));
        let p = parse_progress_line("    209,715,200B 100%   2.44GB/s    0:00:00 (xfr#1, to-chk=0/2)").unwrap();
        assert_eq!(p.bytes_done, 209715200);
        assert_eq!(p.bytes_per_sec, Some(2_440_000_000));
        let p = parse_progress_line("      1.2MB  45%   12.34kB/s    0:01:23").unwrap();
        assert_eq!(p.bytes_done, 1_200_000);
    }

    #[test]
    fn last_of_many_cr_updates() {
        let buf = "100  1%   1.00MB/s    0:01:00\r200  2%   2.00MB/s    0:00:30\rsome trailing";
        let p = last_progress(buf).unwrap();
        assert_eq!(p.bytes_done, 200);
        assert_eq!(p.pct, 2.0);
    }

    #[test]
    fn verify_output_parser() {
        // rsync -rcn --out-format='%n' prints one name per differing file.
        fn mismatch_out(s: &str) -> bool {
            s.lines().any(|l| !l.trim().is_empty())
        }
        assert!(!mismatch_out(""));
        assert!(!mismatch_out("\n"));
        assert!(mismatch_out("docs/a.txt\n"));
        assert!(mismatch_out("b.bin"));
    }
}
