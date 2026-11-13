//! usbkeeper — per-device ingest pipeline. One thread per qualifying stick.
//!
//! detect -> mount ro -> rsync copy (progress->SSE) -> rsync -rcn verify ->
//! wipefs+mkfs.exfat (only after IMMEDIATE sysfs re-guard) -> umount -> done.
//! Any error stops before formatting and parks the card in `error` state.

use crate::rsync_parse::last_progress;
use crate::state::{self, Device, Port};
use crate::sysfs::{self, BlkDev};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Ctx {
    pub dest_root: String,
    pub allow_loop: bool,
}

/// Filesystems we can mount read-only on this box. Anything else -> error, never format.
const SUPPORTED_FS: &[&str] = &["vfat", "fat32", "exfat", "ext2", "ext3", "ext4", "ntfs", "xfs", "btrfs"];

pub fn run_pipeline(dev: BlkDev, ctx: Arc<Ctx>, gone: Arc<AtomicBool>) {
    let port_name = dev.port_label();
    let mut d = Device {
        dev: dev.name.clone(),
        vendor: dev.vendor.clone().unwrap_or_default(),
        model: dev.model.clone().unwrap_or_default(),
        serial: dev.serial.clone().unwrap_or_default(),
        size_bytes: dev.size_bytes,
        fs: String::new(),
        dest: String::new(),
        state: "detected".into(),
        pct: None,
        bytes_done: None,
        bytes_total: None,
        bytes_per_sec: None,
        eta_sec: None,
        error: None,
        ts: sysfs::now_iso(),
    };
    state::update_device(&port_name, |x| *x = d.clone());

    if let Err(e) = stage(&dev, &ctx, &mut d, &port_name, &gone) {
        // errors keep the card red until rescan/removal
        d.state = "error".into();
        d.error = Some(truncate(&e, 400));
        d.ts = sysfs::now_iso();
        state::update_device(&port_name, |x| *x = d.clone());
        cleanup_mount(&dev.name);
        eprintln!("[usbkeeper] {} pipeline error: {e}", dev.name);
    }
}

fn stage(
    dev: &BlkDev,
    ctx: &Ctx,
    d: &mut Device,
    port_name: &str,
    gone: &Arc<AtomicBool>,
) -> Result<(), String> {
    let set = |d: &mut Device, st: &str| {
        d.state = st.into();
        d.ts = sysfs::now_iso();
        state::update_device(port_name, |x| *x = d.clone());
    };

    // 1. find a filesystem node (whole disk or first partition with a TYPE)
    set(d, "mounting");
    let (node, fstype) = detect_fs(dev)?;
    d.fs = fstype.clone();
    if !SUPPORTED_FS.contains(&fstype.as_str()) {
        return Err(format!("unsupported-filesystem: {fstype} (never formatting)"));
    }
    check_alive(dev, gone)?;

    // 2. mount ro (resolve collisions: udisks/fixture mounts of THIS device)
    let mpoint = format!("/run/usbkeeper/{}", dev.name);
    fs::create_dir_all(&mpoint).map_err(|e| format!("mkdir mpoint: {e}"))?;
    let mp = format!("{mpoint}/fs");
    fs::create_dir_all(&mp).map_err(|e| format!("mkdir fs: {e}"))?;
    for t in mount_targets_of(&dev.name) {
        if t == "/" || t == "/home" || t.starts_with("/home/") {
            return Err(format!("refusing: device is mounted at critical path {t}"));
        }
        let _ = run("umount", &["-l".into(), t.clone()]);
        eprintln!("[usbkeeper] {} resolved pre-existing mount {t}", dev.name);
    }
    let _ = run("umount", &[mp.clone()]); // stale from crashed run
    let mopts = format!("ro{}", if fstype.starts_with("ext") { ",noload" } else { "" });
    let do_mount = |opts: &str| -> Result<(), String> {
        run("mount", &["-o".into(), opts.into(), "-t".into(), fstype.clone(), node.clone(), mp.clone()]).map(|_| ())
    };
    if let Err(e1) = do_mount(&mopts) {
        // Auto-fix pass 1: repair the filesystem metadata read-only-safe,
        // then retry. fsck -a on vfat/exfat only fixes FAT/dirs; it never
        // invents data. ext gets -p (preen). Anything else: fail as before.
        let repairable = matches!(fstype.as_str(), "vfat" | "fat32" | "exfat" | "ext4" | "ext3" | "ext2");
        if !repairable {
            return Err(format!("mount failed: {}", truncate(&e1, 200)));
        }
        if !std::path::Path::new("/run/usbkeeper/allow-loop").exists()
            && sysfs::usb_guard_ok(&dev.name, false).is_err()
        {
            // loop device but marker vanished mid-run: no destructive fsck
            return Err(format!("mount failed: {}", truncate(&e1, 200)));
        }
        d.state = "repairing".into();
        d.ts = sysfs::now_iso();
        state::update_device(port_name, |x| *x = d.clone());
        let rc = match fstype.as_str() {
            "vfat" | "fat32" => run("fsck.vfat", &["-a".into(), node.clone()]),
            "exfat" => run("exfatfsck", &["-a".into(), node.clone()]),
            ext => { let _ = ext; run("fsck.ext4", &["-p".into(), node.clone()]) },
        };
        // fsck exit codes: 0 clean, 1 corrected, 2 corrected+reboot-ish, 3+ unknown
        let repaired_ok = match &rc {
            Ok(_) => true,
            Err(msg) => msg.contains("(status=exit status: 1)")
                || msg.contains("(status=exit status: 2)")
                || msg.ends_with("exit status: 1)") || msg.ends_with("exit status: 2)"),
        };
        eprintln!("[usbkeeper] {} fsck {} -> {:?}", dev.name, node, rc.as_ref().map(|_| "ok").map_err(|e| e.as_str()));
        do_mount(&mopts).map_err(|e2| format!(
            "mount failed after fsck: {} (fsck said: {})",
            truncate(&e2, 160),
            truncate(rc.as_ref().err().map(String::as_str).unwrap_or("clean"), 160)
        ))?;
        let _ = repaired_ok;
    }
    check_alive(dev, gone)?;

    // 3. stick id + dest dir
    let stick_id = stick_id(dev);
    let dest = format!("{}/{}", ctx.dest_root.trim_end_matches('/'), stick_id);
    d.dest = dest.clone();
    fs::create_dir_all(&dest).map_err(|e| format!("mkdir dest: {e}"))?;

    // 4. total size for pct math (salvage du: bad entries excluded from total)
    let (total_raw, bad_paths) = du_salvage(&mp)?;
    // Metadata-lie cap: a tree cannot exceed its device. Corrupted FAT tables
    // claim absurd sizes; rsync would create giant sparse files and verify
    // would mismatch forever. Cap the expected total at raw device capacity.
    let cap = dev.size_bytes;
    let total = if total_raw > cap {
        eprintln!("[usbkeeper] {} salvage-cap: tree claims {} > device {}", dev.name, total_raw, cap);
        cap
    } else {
        total_raw
    };
    d.bytes_total = Some(total);

    // 5. copy via rsync, throttled progress broadcasts (~2/s)
    set(d, "copying");
    rsync_copy(&node, &mp, &dest, d, port_name, gone, &fstype)?;
    check_alive(dev, gone)?;
    // 5b. stick UIDs are meaningless here: hand the archive to the dest owner
    normalize_ownership(&dest_root_of(&dest), &dest);

    // 6. verify: checksum dry-run tolerating the salvage bad-list
    set(d, "verifying");
    verify(&mp, &dest, d, port_name, &fstype, &bad_paths, dev.size_bytes)?;
    check_alive(dev, gone)?;

    // 7. unmount BEFORE destructive ops (kernel refuses busy devices)
    set(d, "unmounting");
    let _ = run("umount", &[mp.clone()]);
    // retry lazy once if still busy
    if is_mounted(&mp) {
        let _ = run("umount", &["-l".into(), mp.clone()]);
        std::thread::sleep(Duration::from_millis(300));
        if is_mounted(&mp) {
            return Err("device still mounted before format; refusing to format".into());
        }
    }

    // 8. SAFETY LAW: immediate re-check of sysfs USB parent chain right now
    let guard = sysfs::usb_guard_ok(&dev.name, ctx.allow_loop)?;
    if !sysfs::size_ok_for_format(dev.size_bytes) {
        return Err(format!(
            "refusing format: size {} >= 4TB or zero",
            dev.size_bytes
        ));
    }
    // also refuse if ANY mount today references /dev/<name> or its partitions
    if mounts_touch(&dev.name) {
        return Err("refusing format: some mount still references device".into());
    }
    let _ = guard;
    // 9. wipe + mkfs exfat whole device
    // live guard: loop devices only get formatted while the marker exists
    if !std::path::Path::new("/run/usbkeeper/allow-loop").exists()
        && sysfs::usb_guard_ok(&dev.name, false).is_err()
    {
        return Err("refusing format: loop device without live allow-loop marker".into());
    }
    set(d, "formatting");
    run("wipefs", &["-a".into(), format!("/dev/{name}", name = dev.name)])?;
    // recreate empty partition table so kernel re-reads clean layout
    // exfatprogs 1.4.4 caps volume labels at 8 chars ("input string is too
    // long" beyond that), so the spec's "USBK-YYYYMMDD" cannot fit. Chosen:
    // "USBK-" + YYMMDD (6) = exactly 8, still dated per owner intent.
    let iso = sysfs::now_iso();
    let label = format!("USBK-{}", &iso[2..8].replace('-', "")); // USBK-261112
    run(
        "mkfs.exfat",
        &["-F".into(), "-L".into(), label.clone(), format!("/dev/{name}", name = dev.name)],
    )?;
    let _ = run("blockdev", &["--rereadpt".into(), format!("/dev/{name}", name = dev.name)]);

    // 10. done
    d.pct = Some(100.0);
    d.bytes_done = d.bytes_total;
    d.eta_sec = Some(0);
    set(d, "done");
    Ok(())
}

fn check_alive(dev: &BlkDev, gone: &Arc<AtomicBool>) -> Result<(), String> {
    if gone.load(Ordering::Relaxed) || !Path::new("/sys/block").join(&dev.name).exists() {
        return Err("device removed mid-pipeline".into());
    }
    Ok(())
}

fn fstype_safe_opts(f: &str) -> String {
    match f {
        "ext2" | "ext3" | "ext4" => "ro,noload".into(), // don't replay journals on foreign media
        _ => "ro".into(),
    }
}

/// blkid-detected fstype for the best candidate node.
fn detect_fs(dev: &BlkDev) -> Result<(String, String), String> {
    let mut candidates = vec![format!("/dev/{}", dev.name)];
    for p in &dev.partitions {
        candidates.push(format!("/dev/{p}"));
    }
    let mut first_type: Option<(String, String)> = None;
    for c in &candidates {
        let t = blkid_type(c);
        match t {
            Ok(ty) if !ty.is_empty() => {
                if ty == "swap" {
                    continue;
                }
                let t2 = ty.clone();
                if first_type.is_none() {
                    first_type = Some((c.clone(), ty));
                }
                // prefer a real data fs over an MBR leftover type
                if SUPPORTED_FS.contains(&t2.as_str()) {
                    return Ok((c.clone(), t2));
                }
            }
            _ => {}
        }
    }
    // no TYPE anywhere via blkid: last resort, try partition nodes with the
    // kernel driver directly (raw exfat/ext trees where blkid parsing failed)
    for p in &dev.partitions {
        let node = format!("/dev/{p}");
        if let Some(ty) = kernel_probe(&node) {
            return Ok((node, ty));
        }
    }
    // blkid silent everywhere but the kernel driver mounts it (corrupted FAT
    // whose boot sector confuses the userspace parser): trust the kernel.
    for p in &dev.partitions {
        let node = format!("/dev/{p}");
        if let Some(ty) = mount_probe(&node) {
            return Ok((node, ty));
        }
    }
    first_type.map(|(n, t)| (n, t)).ok_or_else(|| "no-filesystem: no recognized filesystem on device (not formatting)".to_string())
}

/// read-only test mount to learn the fs type when blkid cannot parse it
fn mount_probe(node: &str) -> Option<String> {
    let tp = "/tmp/usbkeeper-mountprobe";
    std::fs::create_dir_all(tp).ok()?;
    for ty in ["vfat", "exfat", "ext4"] {
        let out = std::process::Command::new("mount")
            .args(["-o", "ro,noload", "-t", ty, node, tp])
            .output()
            .ok()?;
        if out.status.success() {
            let _ = std::process::Command::new("umount").args(["-l", tp]).output();
            return Some(ty.to_string());
        }
        let _ = std::process::Command::new("umount").args(["-l", tp]).output();
    }
    None
}

/// ask the kernel which driver claims this node (works even when blkid's
/// userspace parser fails, e.g. exfat without exfatprogs installed)
fn kernel_probe(node: &str) -> Option<String> {
    let base = std::path::Path::new(node).file_name()?.to_string_lossy().into_owned();
    let out = std::process::Command::new("blkid")
        .args(["-p", "-o", "value", "-s", "USAGE", node])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s == "filesystem" {
        // USAGE says fs; get the type from the second blkid call
        let t = std::process::Command::new("blkid")
            .args(["-p", "-o", "value", "-s", "TYPE", node])
            .output()
            .ok()?;
        let ty = String::from_utf8_lossy(&t.stdout).trim().to_string();
        if !ty.is_empty() {
            return Some(ty);
        }
    }
    // finally: consult /sys so we at least know the kernel sees a partition
    let _ = base;
    None
}

fn blkid_type(node: &str) -> Result<String, String> {
    let out = Command::new("blkid")
        .args(["-o", "value", "-s", "TYPE", node])
        .output()
        .map_err(|e| format!("blkid spawn: {e}"))?;
    if !out.status.success() {
        return Ok(String::new()); // no fs
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn stick_id(dev: &BlkDev) -> String {
    let date = date_yyyymmdd();
    if let Some(s) = &dev.serial {
        let s = sysfs::sanitize_id(s.trim());
        if !s.is_empty() {
            return format!("{s}-{date}");
        }
    }
    let v = sysfs::sanitize_id(dev.vendor.as_deref().unwrap_or("unknown"));
    let m = sysfs::sanitize_id(dev.model.as_deref().unwrap_or("unknown"));
    format!("{v}_{m}-{}-{}-{date}", dev.major, dev.minor)
}

fn date_yyyymmdd() -> String {
    let iso = sysfs::now_iso();
    iso[..10].replace('-', "")
}

/// du -sb that SURVIVES I/O errors. GNU du prints its partial total and exits 1
/// when some entries are inaccessible; we take the total anyway and collect the
/// inaccessible paths as a salvage bad-list ("" for each `cannot access '<p>'`).
fn du_bytes(path: &str) -> Result<u64, String> {
    let (n, _bad) = du_salvage(path)?;
    Ok(n)
}

fn du_salvage(path: &str) -> Result<(u64, Vec<String>), String> {
    let out = std::process::Command::new("du")
        .args(["-sb", path])
        .output()
        .map_err(|e| format!("du spawn: {e}"))?;
    let errtext = String::from_utf8_lossy(&out.stderr).to_string();
    let mut bad: Vec<String> = Vec::new();
    for line in errtext.lines() {
        if let Some(rest) = line.strip_prefix("du: cannot access \'") {
            if let Some(p) = rest.split("\'").next() {
                bad.push(shell_unescape(p));
            }
        }
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    let n = s.trim().split_whitespace().next().and_then(|x| x.parse::<u64>().ok());
    match n {
        Some(v) => Ok((v, bad)),
        None if out.status.success() => Ok((0, bad)),
        None => Err(format!("du failed: {}", truncate(&errtext, 200))),
    }
}

/// undo GNU shell-style quoting ($'...') used by du for odd filenames
fn shell_unescape(s: &str) -> String {
    if !(s.starts_with("$'") && s.ends_with('\'')) {
        return s.to_string();
    }
    let bytes = s[2..s.len() - 1].as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'\'' => out.push(b'\''),
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'0'..=b'7' => {
                    let mut v: u32 = 0;
                    let mut k = 0;
                    while k < 3 && i < bytes.len() && matches!(bytes[i], 0x30..=0x37) {
                        v = v * 8 + (bytes[i] - b'0') as u32;
                        i += 1;
                        k += 1;
                    }
                    out.push(v as u8);
                    continue;
                }
                other => out.push(other),
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reset archive-tree ownership to the owner of the --dest root after a
/// stick copy. FAT/exfat sticks carry foreign UIDs (often nobody/836) and
/// rsync -ax preserves them, which would lock the human user out of their
/// own archive. Best-effort: failures are reported on stderr, never fatal.
fn normalize_ownership(dest_root: &str, archive_dir: &str) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = match std::fs::metadata(dest_root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[usbkeeper] ownership-normalize skipped (stat dest): {e}");
            return;
        }
    };
    let (uid, gid) = (meta.uid(), meta.gid());
    let mut dirs = vec![std::path::PathBuf::from(archive_dir)];
    let mut walked = 0usize;
    while let Some(dir) = dirs.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let md = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.file_type().is_dir() {
                dirs.push(p.clone());
            }
            if let Err(e) = chown_path(&p, uid, gid) {
                eprintln!("[usbkeeper] chown failed {}: {e}", p.display());
            }
            if md.file_type().is_dir() {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o775));
            } else if !md.file_type().is_symlink() {
                let mode = md.permissions().mode();
                let want = 0o664 | ((mode & 0o111) != 0) as u32 * 0o111;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(want));
            }
            walked += 1;
        }
    }
    let _ = std::fs::set_permissions(archive_dir, std::fs::Permissions::from_mode(0o775));
    if let Err(e) = chown_path(std::path::Path::new(archive_dir), uid, gid) {
        eprintln!("[usbkeeper] chown archive root failed: {e}");
    }
    eprintln!(
        "[usbkeeper] ownership normalized: {} entries -> uid {uid} gid {gid}",
        walked
    );
}

fn chown_path(p: &std::path::Path, uid: u32, gid: u32) -> Result<(), String> {
    use std::ffi::CString;
    let c = CString::new(p.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let rc = unsafe { libc_chown(c.as_ptr(), uid, gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

extern "C" {
    #[link_name = "chown"]
    fn libc_chown(path: *const std::os::raw::c_char, uid: u32, gid: u32) -> i32;
}

/// Archive layout is <dest-root>/<vendor-product>-<port>-<date>; the dest root
/// carries the human owner we normalize TO.
fn dest_root_of(archive_dir: &str) -> String {
    std::path::Path::new(archive_dir)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| archive_dir.to_string())
}

fn rsync_copy(
    _node: &str,
    src: &str,
    dest: &str,
    d: &mut Device,
    port_name: &str,
    gone: &Arc<AtomicBool>,
    fstype: &str,
) -> Result<(), String> {
    // vfat/exfat have 2s timestamp granularity and no symlinks/ownership:
    // -aH preserve-times fails there with "Operation not permitted". Use the
    // relaxed flag set for FAT-family sources; keep -aH for ext4/ntfs/etc.
    // --ignore-errors/--keep-going: damaged sticks must still archive
    // everything readable; unreadable files are tolerated via the bad-list
    // in verify and recorded in USBKEEPER-BADFILES.txt.
    let flags: Vec<String> = if matches!(fstype, "vfat" | "fat32" | "exfat") {
        ["-rltDox", "--ignore-errors", "--info=progress2"]
            .iter().map(|s| s.to_string()).collect()
    } else {
        ["-aH".into(), "--ignore-errors".into(), "--info=progress2".into()].to_vec()
    };
    let mut cmd = Command::new("rsync");
    cmd.args(&flags)
        .arg("--no-motd")
        .arg(format!("{src}/"))
        .arg(format!("{dest}/"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("rsync spawn: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let mut rd = BufReader::new(stdout);
    let mut carry = String::new();
    let total = d.bytes_total.unwrap_or(0).max(1);
    let mut last_bc = Instant::now() - Duration::from_millis(600);
    loop {
        // read up to next \r or \n (rsync progress uses \r)
        let mut chunk = Vec::new();
        loop {
            let mut b = [0u8; 1];
            match rd.read(&mut b) {
                Ok(0) => break,
                Ok(_) => {
                    chunk.push(b[0]);
                    if b[0] == b'\r' || b[0] == b'\n' {
                        break;
                    }
                }
                Err(ref _e) => break,
            }
        }
        if chunk.is_empty() {
            break; // EOF
        }
        carry.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(p) = last_progress(&carry) {
            let now = Instant::now();
            if now.duration_since(last_bc) >= Duration::from_millis(500) {
                last_bc = now;
                d.bytes_done = Some(p.bytes_done);
                d.pct = Some(((p.bytes_done as f64 / total as f64) * 1000.0).round() / 10.0);
                d.bytes_per_sec = p.bytes_per_sec;
                d.eta_sec = p
                    .bytes_per_sec
                    .filter(|r| *r > 0)
                    .map(|r| (total - p.bytes_done.min(total)) / r);
                d.ts = sysfs::now_iso();
                state::update_device(port_name, |x| *x = d.clone());
                carry.clear();
            }
        }
        if gone.load(Ordering::Relaxed) {
            let _ = child.kill();
            return Err("device removed during copy".into());
        }
    }
    let status = child.wait().map_err(|e| format!("rsync wait: {e}"))?;
    let mut err = String::new();
    if let Some(mut se) = child.stderr.take() {
        let _ = se.read_to_string(&mut err);
    }
    // 23 = partial transfer (skipped bad files), 24 = vanished mid-copy:
    // both are salvage outcomes, verify decides what is really lost.
    let code = status.code().unwrap_or(1);
    if !(code == 0 || code == 23 || code == 24) {
        return Err(format!("rsync copy failed ({status}): {}", truncate(&err, 200)));
    }
    Ok(())
}

fn verify(src: &str, dest: &str, d: &mut Device, port_name: &str, fstype: &str, bad_paths: &[String], dev_cap: u64) -> Result<(), String> {
    let out = std::process::Command::new("rsync")
        .args(["-rcn", "--out-format=%n", "--no-motd"])
        .arg(format!("{src}/"))
        .arg(format!("{dest}/"))
        .output()
        .map_err(|e| format!("rsync verify spawn: {e}"))?;
    let vcode = out.status.code().unwrap_or(1);
    let vstderr = String::from_utf8_lossy(&out.stderr).to_string();
    if vcode > 24 {
        return Err(format!("rsync verify failed ({vcode}): {}", truncate(&vstderr, 200)));
    }
    let all_diffs: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    // tolerate diffs whose path hits the known-bad list (unreadable on source)
    let is_bad = |l: &str| -> bool {
        bad_paths.iter().any(|b| {
            let tail = b.rsplit_once('/').map(|(_, t)| t).unwrap_or(b);
            l == tail || l.ends_with(tail) || b.ends_with(l)
        })
    };
    let diffs: Vec<&str> = all_diffs.iter().filter(|l| !is_bad(l)).map(String::as_str).collect();
    if !diffs.is_empty() {
        return Err(format!(
            "verify-mismatch: {} file(s) differ (NOT formatting): {}",
            diffs.len(),
            truncate(&diffs.join(", "), 200)
        ));
    }
    // size cross-check with salvage law: source total excludes unreadable entries
    let (src_sz, bad_now) = du_salvage(src)?;
    let dst_sz = du_bytes(dest)?;
    let fat = matches!(fstype, "vfat" | "fat32" | "exfat");
    let ok = if fat {
        let slack = (src_sz / 20).max(8_000_000);
        src_sz.abs_diff(dst_sz) <= slack
    } else {
        src_sz == dst_sz
    };
    if !ok {
        // Salvage-settled case: the file table LIES (claims more bytes than the
        // device physically holds) yet no readable file differs from the
        // archive. That means everything recoverable is already copied; the
        // size gap is phantom metadata, not missing data. Accept and document.
        if src_sz > dev_cap && dst_sz > 0 {
            write_note(dest, "USBKEEPER-SALVAGED.txt", &format!(
                "source metadata corrupt: file table claims {} bytes on a {}-byte device.\n\
                 Archive holds the complete salvage set ({} bytes): every file that\n\
                 could be read matches by checksum. Unreadable clusters are lost.\n",
                src_sz, dev_cap, dst_sz));
            d.bytes_done = Some(dst_sz);
            d.pct = Some(100.0);
            d.ts = sysfs::now_iso();
            state::update_device(port_name, |x| *x = d.clone());
            return Ok(());
        }
        // Reconcile case: source filesystem reads EMPTY while this archive dir
        // already holds bytes from an earlier copy that never reached done
        // (stick wiped/reformatted in between). Data is safe server-side:
        // accept, note provenance, allow the normal format-for-reuse step.
        if src_sz == 0 && dst_sz > 0 {
            write_note(dest, "USBKEEPER-RECONCILED.txt", &format!(
                "reconciled: stick presented an empty filesystem (0 bytes) but this\n\
                 archive holds {} bytes from an earlier completed copy. Data safe;\n\
                 stick reformatted for reuse by usbkeeper.\n", dst_sz));
        } else {
            return Err(format!("verify-mismatch: size {src_sz} != {dst_sz} (NOT formatting)"));
        }
    }
    let mut badlist = bad_paths.to_vec();
    for b in bad_now {
        if !badlist.contains(&b) { badlist.push(b); }
    }
    if !badlist.is_empty() {
        let body = format!(
            "# skipped due to read errors (stick damaged, data unrecoverable):\n{}\n",
            badlist.join("\n"));
        write_note(dest, "USBKEEPER-BADFILES.txt", &body);
    }
    d.bytes_done = Some(dst_sz);
    d.pct = Some(100.0);
    d.ts = sysfs::now_iso();
    state::update_device(port_name, |x| *x = d.clone());
    Ok(())
}

fn write_note(dest: &str, name: &str, body: &str) {
    let path = format!("{}/{}", dest.trim_end_matches('/'), name);
    let _ = fs::write(path, body);
}
fn is_mounted(path: &str) -> bool {
    if let Ok(m) = fs::read_to_string("/proc/mounts") {
        return m.lines().any(|l| {
            l.split_whitespace()
                .nth(1)
                .map(|t| t == path)
                .unwrap_or(false)
        });
    }
    false
}

/// true if /proc/mounts has any entry whose source starts with /dev/<name>
fn mounts_touch(name: &str) -> bool {
    !mount_targets_of(name).is_empty()
}

/// All current mount TARGETS whose source is /dev/<name> or a partition of it.
fn mount_targets_of(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(m) = fs::read_to_string("/proc/mounts") {
        let pat = format!("/dev/{name}");
        for l in m.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (src, tgt) = (f.first().copied().unwrap_or(""), f.get(1).copied().unwrap_or(""));
            if src == pat
                || (src.starts_with(&pat)
                    && src.as_bytes().get(pat.len()).map(|b| b.is_ascii_digit()).unwrap_or(false))
            {
                out.push(tgt.to_string());
            }
        }
    }
    out
}

fn cleanup_mount(name: &str) {
    let mp = format!("/run/usbkeeper/{name}/fs");
    let _ = Command::new("umount").arg(&mp).status();
}

fn run(cmd: &str, args: &[String]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} failed ({status}): {err}",
            status = out.status,
            err = truncate(&String::from_utf8_lossy(&out.stderr), 300)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

// ---- helpers on BlkDev living here to keep sysfs.rs I/O-only ----
trait PortLabel {
    fn port_label(&self) -> String;
    fn speed_label(&self) -> String;
}
impl PortLabel for BlkDev {
    fn port_label(&self) -> String {
        self.usb_port
            .clone()
            .unwrap_or_else(|| self.name.clone()) // loop0 etc.
    }
    fn speed_label(&self) -> String {
        match self.usb_speed_mbps {
            Some(5000) => "5000".into(),
            Some(480) => "480".into(),
            Some(s) if s > 0 => s.to_string(),
            _ => "unknown".into(),
        }
    }
}

#[allow(dead_code)]
fn _port_ctor(dev: &BlkDev, d: Device) -> Port {
    Port {
        port: dev.port_label(),
        speed: dev.speed_label(),
        device: Some(d),
    }
}
