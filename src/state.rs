//! usbkeeper — shared state registry + SSE broadcast hub.

use crate::json::{q, qf, qn, qs};
use std::collections::HashMap;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
pub struct Device {
    pub dev: String,           // "sdb"
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub size_bytes: u64,
    pub fs: String,            // detected fstype ("" until known)
    pub dest: String,          // archive dir for this stick
    pub state: String,         // detected|mounting|copying|verifying|formatting|unmounting|done|error
    pub pct: Option<f64>,
    pub bytes_done: Option<u64>,
    pub bytes_total: Option<u64>,
    pub bytes_per_sec: Option<u64>,
    pub eta_sec: Option<u64>,
    pub error: Option<String>,
    pub ts: String,
}

impl Device {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"dev\":{},\"vendor\":{},\"model\":{},\"serial\":{},\"size_bytes\":{},\"fs\":{},\"dest\":{},\"state\":{},\"pct\":{},\"bytes_done\":{},\"bytes_total\":{},\"bytes_per_sec\":{},\"eta_sec\":{},\"error\":{},\"ts\":{}}}",
            q(&self.dev),
            q(&self.vendor),
            q(&self.model),
            q(&self.serial),
            self.size_bytes,
            q(&self.fs),
            q(&self.dest),
            q(&self.state),
            qf(self.pct),
            qn(self.bytes_done),
            qn(self.bytes_total),
            qn(self.bytes_per_sec),
            qn(self.eta_sec),
            qs(self.error.as_deref()),
            q(&self.ts),
        )
    }
}

#[derive(Clone, Debug)]
pub struct Port {
    pub port: String,   // "1-6.1" or "loop0"
    pub speed: String,  // "5000" | "480" | "unknown"
    pub device: Option<Device>,
}

impl Port {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"port\":{},\"speed\":{},\"occupied\":{},\"device\":{}}}",
            q(&self.port),
            q(&self.speed),
            self.device.is_some(),
            match &self.device {
                Some(d) => d.to_json(),
                None => "null".to_string(),
            }
        )
    }
}

pub struct Registry {
    pub ports: HashMap<String, Port>,
    clients: Vec<SyncSender<String>>,
}

static REG: OnceLock<Mutex<Registry>> = OnceLock::new();

pub fn reg() -> &'static Mutex<Registry> {
    REG.get_or_init(|| {
        Mutex::new(Registry {
            ports: HashMap::new(),
            clients: Vec::new(),
        })
    })
}

/// Register an SSE client; returns receiver of port JSON events.
pub fn subscribe() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::sync_channel::<String>(256);
    reg().lock().unwrap().clients.push(tx);
    rx
}

/// Set/replace a port entry and broadcast its JSON to all SSE clients.
pub fn upsert(port: Port) {
    let json = port.to_json();
    let mut r = reg().lock().unwrap();
    r.ports.insert(port.port.clone(), port);
    broadcast(&mut r, &json);
}

pub fn remove_port(name: &str) {
    let mut r = reg().lock().unwrap();
    if let Some(p) = r.ports.get_mut(name) {
        p.device = None;
        p.speed = "unknown".into();
        let j = p.to_json();
        broadcast(&mut r, &j);
    }
}

/// Mutate the device of a port (if present) and broadcast. Returns false if gone.
pub fn update_device<F: FnOnce(&mut Device)>(port_name: &str, f: F) -> bool {
    let mut r = reg().lock().unwrap();
    let Some(p) = r.ports.get_mut(port_name) else {
        return false;
    };
    let Some(d) = p.device.as_mut() else {
        return false;
    };
    f(d);
    let j = p.to_json();
    broadcast(&mut r, &j);
    true
}

fn broadcast(r: &mut Registry, json: &str) {
    r.clients.retain(|tx| match tx.try_send(json.to_string()) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(_)) => true, // slow client: drop event, keep conn
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    });
}

pub fn snapshot_json() -> String {
    let r = reg().lock().unwrap();
    let mut v: Vec<&Port> = r.ports.values().collect();
    v.sort_by(|a, b| a.port.cmp(&b.port));
    let items: Vec<String> = v.iter().map(|p| p.to_json()).collect();
    format!("{{\"ports\":[{}]}}", items.join(","))
}

pub fn occupied_ports() -> Vec<String> {
    let r = reg().lock().unwrap();
    r.ports
        .values()
        .filter(|p| p.device.is_some())
        .map(|p| p.port.clone())
        .collect()
}
