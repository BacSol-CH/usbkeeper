//! usbkeeper — root-run USB stick ingest daemon with embedded HTTP/SSE API.
//!
//! Design: std::thread everywhere, hand-rolled HTTP/1.1+SSE over TcpListener.
//! Justification vs tokio+axum: the workload is blocking subprocess I/O
//! (rsync/mkfs) plus a 2s poll loop; async buys nothing here and a zero-dep
//! build has far fewer toolchain headaches on an Arch-like box.

mod http;
mod json;
mod pipeline;
mod rsync_parse;
mod state;
mod sysfs;

use pipeline::Ctx;
use state::{Device, Port};
use std::collections::HashMap;
use std::path::Path;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const ALLOW_LOOP_MARKER: &str = "/run/usbkeeper/allow-loop";

struct Args {
    dest: String,
    port: u16,
    allow_loop_cli: bool,
    ui_root: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        dest: "/var/lib/usb-archive".into(),
        port: 8488,
        allow_loop_cli: false,
        ui_root: None,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--dest" => {
                i += 1;
                a.dest = argv.get(i).ok_or("--dest needs a value")?.clone();
            }
            "--ui-root" => {
                i += 1;
                a.ui_root = Some(argv.get(i).ok_or("--ui-root needs a value")?.clone());
            }
            "--port" => {
                i += 1;
                a.port = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("--port needs a number")?;
            }
            "--allow-loop" => a.allow_loop_cli = true,
            "-h" | "--help" => {
                println!("usage: usbkeeper --dest DIR --port N [--allow-loop]");
                exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
        i += 1;
    }
    Ok(a)
}

fn allow_loop(args_cli: bool) -> bool {
    args_cli || Path::new(ALLOW_LOOP_MARKER).exists()
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("usbkeeper: {e}");
            exit(2);
        }
    };
    let loop_ok = allow_loop(args.allow_loop_cli);
    http::set_ui_root(args.ui_root.clone());
    if read_euid() != 0 {
        eprintln!("[usbkeeper] WARNING: not running as root; mount/format will fail");
    }
    std::fs::create_dir_all(&args.dest).ok();
    std::fs::create_dir_all("/run/usbkeeper").ok();

    let ctx = Arc::new(Ctx {
        dest_root: args.dest.clone(),
        allow_loop: loop_ok,
    });
    let rescan = Arc::new(AtomicBool::new(true));

    // one in-flight tracker per device name
    let inflight: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));

    {
        let ctx = ctx.clone();
        let rescan = rescan.clone();
        let inflight = inflight.clone();
        thread::spawn(move || watcher(ctx, rescan, inflight, loop_ok));
    }

    let addr = format!("0.0.0.0:{}", args.port);
    if let Err(e) = http::serve(&addr, rescan.clone()) {
        eprintln!("[usbkeeper] http server died: {e}");
        exit(1);
    }
}

/// Poll /sys/class/block every 2s. New qualifying stick -> register port +
/// spawn pipeline thread. Vanished stick -> mark port unoccupied.
fn watcher(
    ctx: Arc<Ctx>,
    rescan: Arc<AtomicBool>,
    inflight: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    loop_ok: bool,
) {
    let mut known: HashMap<String, String> = HashMap::new(); // port -> dev name
    loop {
        let force = rescan.swap(false, Ordering::SeqCst);
        let _ = force;
        let devs = sysfs::scan_blocks(loop_ok);
        let mut now_ports: HashMap<String, String> = HashMap::new();
        for dev in devs {
            let port = dev
                .usb_port
                .clone()
                .unwrap_or_else(|| dev.name.clone());
            now_ports.insert(port.clone(), dev.name.clone());
            let was_known = known.contains_key(&port);
            let busy = inflight
                .lock()
                .unwrap()
                .contains_key(&dev.name);
            if !was_known && !busy {
                // fresh stick: register port occupied + launch pipeline
                let d = Device {
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
                let speed = match dev.usb_speed_mbps {
                    Some(s) if s > 0 => s.to_string(),
                    _ => "unknown".into(),
                };
                state::upsert(Port {
                    port: port.clone(),
                    speed,
                    device: Some(d),
                });
                let gone = Arc::new(AtomicBool::new(false));
                inflight.lock().unwrap().insert(dev.name.clone(), gone.clone());
                let ctx2 = ctx.clone();
                let infl2 = inflight.clone();
                let name = dev.name.clone();
                thread::spawn(move || {
                    pipeline::run_pipeline(dev, ctx2, gone);
                    infl2.lock().unwrap().remove(&name);
                });
            } else if !busy && was_known {
                // same port still present after removal+reinsert of another disk?
                // if the tracked dev changed (e.g. sdb->sdc same port), treat as new
                if known.get(&port) != Some(&dev.name) {
                    let gone = Arc::new(AtomicBool::new(false));
                    let d = Device {
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
                    state::upsert(Port {
                        port: port.clone(),
                        speed: match dev.usb_speed_mbps {
                            Some(s) if s > 0 => s.to_string(),
                            _ => "unknown".into(),
                        },
                        device: Some(d),
                    });
                    inflight.lock().unwrap().insert(dev.name.clone(), gone.clone());
                    let ctx2 = ctx.clone();
                    let infl2 = inflight.clone();
                    let name = dev.name.clone();
                    thread::spawn(move || {
                        pipeline::run_pipeline(dev, ctx2, gone);
                        infl2.lock().unwrap().remove(&name);
                    });
                }
            }
        }
        // vanished devices: tell any running pipeline it was pulled, and clear
        // port occupancy once no pipeline is in flight for that old device name.
        for (port, old_name) in known.iter() {
            if !now_ports.contains_key(port) && !Path::new("/sys/block").join(old_name).exists() {
                if let Some(g) = inflight.lock().unwrap().get(old_name) {
                    g.store(true, Ordering::Relaxed);
                }
                let still_busy = inflight.lock().unwrap().contains_key(old_name);
                if !still_busy {
                    state::remove_port(port);
                }
            }
        }
        known = now_ports;
        thread::sleep(Duration::from_secs(2));
    }
}

fn read_euid() -> u32 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for l in s.lines() {
            if let Some(rest) = l.strip_prefix("Uid:") {
                if let Some(euid) = rest.split_whitespace().nth(1) {
                    return euid.parse().unwrap_or(1);
                }
            }
        }
    }
    1
}
