//! usbkeeper — sysfs block/USB discovery + the safety guard.
//!
//! SAFETY LAW: every code path that is about to touch a block device for
//! wipe/format calls `usb_guard_ok()` IMMEDIATELY before issuing the command.
//! The guard re-reads sysfs from scratch — no cached facts are trusted.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// One candidate whole disk found in /sys/class/block.
#[derive(Clone, Debug)]
pub struct BlkDev {
    /// kernel name, e.g. "sdb", "loop0"
    pub name: String,
    pub major: u32,
    pub minor: u32,
    pub size_bytes: u64,
    /// USB port label like "1-6.1"; None for non-USB (loop) devices
    pub usb_port: Option<String>,
    /// negotiated USB link speed in Mbps (from sysfs), if available
    pub usb_speed_mbps: Option<u32>,
    pub removable: bool,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub partitions: Vec<String>,
    /// true when this is a loop device (test mode only)
    pub is_loop: bool,
}

fn read_first_line(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Resolve /sys/block/<name> canonical path (contains the device's devpath).
pub fn sysfs_path(name: &str) -> std::path::PathBuf {
    let p = Path::new("/sys/block").join(name);
    fs::canonicalize(&p).unwrap_or(p)
}

/// The USB device directory name (e.g. "1-6.1") if this block device's
/// sysfs path hangs off a USB device. Works for both real sticks and any
/// hypothetical future layout because it scans every path component.
pub fn usb_device_name(sysfs_canonical: &Path) -> Option<String> {
    for comp in sysfs_canonical.components() {
        if let std::path::Component::Normal(os) = comp {
            let s = os.to_string_lossy().to_string();
            // USB device dirs look like "N-K.M..." : digits, '-', then
            // digit groups separated by '.', possibly with further suffix.
            // Interface dirs additionally have ":x.y" — we skip those.
            if s.contains(':') {
                continue;
            }
            if is_usb_devname(&s) {
                return Some(s);
            }
        }
    }
    None
}

fn is_usb_devname(s: &str) -> bool {
    // patterns: "1-2", "1-2.3", "2-10.4.1"  (root hubs "usb1" excluded: start with "usb")
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return false;
    }
    let mut seen_dash = false;
    for &b in bytes {
        match b {
            b'0'..=b'9' => {}
            b'-' => seen_dash = true,
            b'.' => {}
            _ => return false,
        }
    }
    seen_dash
}

/// Core safety guard. Returns Ok(()) only if, RIGHT NOW, the device
/// qualifies for destructive operations.
pub fn usb_guard_ok(name: &str, allow_loop: bool) -> Result<String, String> {
    let p = sysfs_path(name);
    let sp = p.to_string_lossy().to_string();
    if let Some(usb) = usb_device_name(&p) {
        return Ok(usb);
    }
    // loop allowance is gated by a LIVE marker file, re-checked on every call:
    // the moment /run/usbkeeper/allow-loop disappears, no NEW destructive op
    // may start (in-flight pipelines finish against their already-mounted fs).
    // This makes an e2e run and a production daemon mutually exclusive even
    // when they share /run/usbkeeper.
    if allow_loop
        && std::path::Path::new("/run/usbkeeper/allow-loop").exists()
        && sp.contains("/devices/virtual/block/")
        && name.starts_with("loop")
    {
        return Ok(format!("loop:{name}"));
    }
    // Hard-fail anything else: NVMe/SATA/virtual non-loop devices NEVER pass.
    Err(format!("guard: {name} sysfs path '{sp}' is not USB and not an allowed loop device"))
}

/// Collect all whole disks (never partitions) that look like ingest candidates.
pub fn scan_blocks(allow_loop: bool) -> Vec<BlkDev> {
    let mut out = Vec::new();
    let rd = match fs::read_dir("/sys/block") {
        Ok(r) => r,
        Err(_) => return out,
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    for name in names {
        if is_partition(Path::new("/sys/block").join(&name).as_path()) {
            continue;
        }
        let Some(d) = describe(&name) else { continue };
        // never touch unconfigured/empty devices (spare loop nodes etc.)
        if d.size_bytes == 0 {
            continue;
        }
        // qualification: USB in parent chain AND removable, or usb-storage;
        // loop devices only in allow-loop mode.
        let qualifies = if d.is_loop {
            allow_loop
        } else if d.usb_port.is_some() {
            d.removable || has_usb_storage_driver(&name)
        } else {
            false
        };
        if qualifies {
            out.push(d);
        }
    }
    out
}

fn is_partition(sysfs_dir: &Path) -> bool {
    sysfs_dir.join("partition").exists()
}

fn has_usb_storage_driver(name: &str) -> bool {
    // /sys/block/sdb/device -> .../usb-storage.HOST:X:X:X/hostX/... or driver link
    if let Ok(l) = fs::canonicalize(Path::new("/sys/block").join(name).join("device")) {
        let s = l.to_string_lossy();
        if s.contains("usb-storage") || s.contains("uas") {
            return true;
        }
    }
    if let Ok(d) = fs::read_link(Path::new("/sys/block").join(name).join("device").join("driver")) {
        if let Some(f) = d.file_name() {
            let f = f.to_string_lossy();
            if f.starts_with("usb-storage") || f == "uas" {
                return true;
            }
        }
    }
    false
}

pub fn describe(name: &str) -> Option<BlkDev> {
    let base = Path::new("/sys/block").join(name);
    if !base.exists() {
        return None;
    }
    let dev_val = read_first_line(&base.join("dev"))?; // "8:16"
    let (major, minor) = dev_val
        .split_once(':')
        .map(|(a, b)| (a.parse().unwrap_or(0u32), b.parse().unwrap_or(0u32)))
        .unwrap();
    let size_sectors: u64 = read_first_line(&base.join("size"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let removable = read_first_line(&base.join("removable"))
        .map(|s| s != "0")
        .unwrap_or(false);
    let canon = fs::canonicalize(&base).unwrap_or_else(|_| base.clone());
    let usb_port = usb_device_name(&canon);
    let usb_speed_mbps = usb_port
        .as_ref()
        .and_then(|u| read_first_line(&Path::new("/sys/bus/usb/devices").join(u).join("speed")))
        .and_then(|s| s.parse::<u32>().ok());
    let dev_dir = base.join("device");
    let vendor = read_first_line(&dev_dir.join("vendor"));
    let model = read_first_line(&dev_dir.join("model"));
    let serial = read_first_line(&dev_dir.join("serial"))
        .or_else(|| read_first_line(&dev_dir.join("uss")) );
    let mut partitions = Vec::new();
    if let Ok(rd) = fs::read_dir(&base) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with(name) && n[name.len()..].chars().all(|c| c.is_ascii_digit()) && n.len() > name.len() {
                partitions.push(n);
            }
        }
    }
    partitions.sort();
    Some(BlkDev {
        name: name.to_string(),
        major,
        minor,
        size_bytes: size_sectors * 512,
        usb_port,
        usb_speed_mbps,
        removable,
        vendor,
        model,
        serial,
        partitions,
        is_loop: canon.to_string_lossy().contains("/devices/virtual/block/"),
    })
}

/// Refuse destructive ops on absurd sizes (spec: >= 4TB never formatted).
pub fn size_ok_for_format(size_bytes: u64) -> bool {
    size_bytes > 0 && size_bytes < 4_000_000_000_000
}

/// RFC3339 UTC timestamp without external crates.
pub fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso_from_unix(secs)
}

pub fn iso_fromunix(s: i64) -> String { iso_from_unix(s) }

fn iso_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil_from_days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

pub fn sanitize_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts_roundtrip_known() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_735_689_600), "2025-01-01T00:00:00Z");
    }
    #[test]
    fn usb_names() {
        assert!(is_usb_devname("1-6.1"));
        assert!(is_usb_devname("2-10"));
        assert!(!is_usb_devname("usb1"));
        assert!(!is_usb_devname("nvme0"));
        assert!(!is_usb_devname("1-6:1.0"));
    }
    #[test]
    fn ids() {
        assert_eq!(sanitize_id("USB  Stick 3.0"), "USB_Stick_3_0");
    }
}
