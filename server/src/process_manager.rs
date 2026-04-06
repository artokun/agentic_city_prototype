//! Tracks spawned child process PIDs and kills them on shutdown.
//! Ensures no orphaned Claude CLI processes survive server restarts.

use bevy::prelude::Resource;
use std::sync::{Arc, Mutex};

/// Bevy resource wrapper for the process registry.
#[derive(Resource, Clone)]
pub struct ProcessRegistryRes(pub ProcessRegistry);

/// Shared registry of spawned child process PIDs.
#[derive(Clone, Default)]
pub struct ProcessRegistry {
    pids: Arc<Mutex<Vec<u32>>>,
}

impl ProcessRegistry {
    pub fn register(&self, pid: u32) {
        self.pids.lock().unwrap().push(pid);
        tracing::info!("[ProcessManager] Registered PID {}", pid);
    }

    pub fn remove(&self, pid: u32) {
        self.pids.lock().unwrap().retain(|p| *p != pid);
        tracing::debug!("[ProcessManager] Removed PID {}", pid);
    }

    pub fn kill_all(&self) {
        let pids = self.pids.lock().unwrap().clone();
        if pids.is_empty() {
            return;
        }
        tracing::info!("[ProcessManager] Killing {} child processes", pids.len());
        for pid in &pids {
            tracing::info!("[ProcessManager] Killing PID {}", pid);
            unsafe {
                libc::kill(*pid as i32, libc::SIGTERM);
            }
        }
        // Give them a moment, then force kill.
        std::thread::sleep(std::time::Duration::from_millis(500));
        for pid in &pids {
            unsafe {
                libc::kill(*pid as i32, libc::SIGKILL);
            }
        }
        self.pids.lock().unwrap().clear();
    }
}

/// Install signal handlers that kill all child processes on SIGINT/SIGTERM.
pub fn install_signal_handler(registry: ProcessRegistry) {
    let reg = registry.clone();
    ctrlc::set_handler(move || {
        tracing::info!("[ProcessManager] Signal received — cleaning up child processes");
        reg.kill_all();
        std::process::exit(0);
    })
    .expect("Failed to set signal handler");
}
