//! usbkeeper — JSON serialization (hand-rolled; no external crates).
//! Every string goes through `esc` so arbitrary device-sourced text is safe.

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn q(s: &str) -> String {
    format!("\"{}\"", esc(s))
}

/// Optional helpers: None -> JSON null.
pub fn qs(v: Option<&str>) -> String {
    match v {
        Some(s) => q(s),
        None => "null".to_string(),
    }
}

pub fn qn(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

pub fn qf(v: Option<f64>) -> String {
    match v {
        Some(n) => {
            if n.is_finite() {
                format!("{}", (n * 10.0).round() / 10.0)
            } else {
                "null".to_string()
            }
        }
        None => "null".to_string(),
    }
}
