//! Agent OS desktop shell.
//!
//! The window loads the same web client the browser uses (apps/web). The only desktop-specific
//! responsibilities are:
//!   * telling the UI where the runtime is,
//!   * optionally launching the runtime binary for a self-contained desktop experience,
//!   * exposing the data directory so a user can inspect state without a terminal.
//!
//! Nothing here reimplements runtime logic: the UI talks HTTP/WS to the runtime exactly as the
//! browser does.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopConfig {
    pub runtime_url: String,
    pub grpc_addr: String,
    pub data_dir: String,
    pub workspace_root: String,
    pub runtime_binary: Option<String>,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        let runtime_url = std::env::var("AGENTOS_RUNTIME_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8788".to_string());
        let data_dir = std::env::var("AGENTOS_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
        let workspace_root =
            std::env::var("AGENTOS_WORKSPACE_ROOT").unwrap_or_else(|_| "./workspace".to_string());
        Self {
            runtime_url,
            grpc_addr: std::env::var("AGENTOS_GRPC_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8789".to_string()),
            data_dir,
            workspace_root,
            runtime_binary: find_runtime_binary(),
        }
    }
}

/// Look for the runtime next to the desktop binary, then in the workspace target directory.
fn find_runtime_binary() -> Option<String> {
    let candidate = if cfg!(windows) { "agentos-server.exe" } else { "agentos-server" };
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(candidate));
            paths.push(dir.join("..").join("..").join("..").join("target").join("debug").join(candidate));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join("target").join("debug").join(candidate));
        paths.push(cwd.join("..").join("..").join("target").join("debug").join(candidate));
    }
    paths
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
}

struct RuntimeHandle(Mutex<Option<Child>>);

#[tauri::command]
fn runtime_config() -> DesktopConfig {
    DesktopConfig::default()
}

/// Probe the runtime once. The UI polls this to render a connection badge.
#[tauri::command]
async fn runtime_health(url: String) -> Result<serde_json::Value, String> {
    let response = reqwest::Client::new()
        .get(format!("{}/healthz", url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("runtime unreachable: {e}"))?;
    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("invalid health payload: {e}"))
}

/// Start the runtime as a child process. Returns immediately; the UI waits for health.
#[tauri::command]
fn start_runtime(state: tauri::State<'_, RuntimeHandle>) -> Result<String, String> {
    let config = DesktopConfig::default();
    let binary = config
        .runtime_binary
        .clone()
        .ok_or_else(|| "agentos-server binary was not found; build it with: cargo build -p agentos-server".to_string())?;
    let mut guard = state.0.lock().map_err(|_| "runtime handle is poisoned".to_string())?;
    if let Some(child) = guard.as_mut() {
        if child.try_wait().map_err(|e| e.to_string())?.is_none() {
            return Ok("runtime is already running".to_string());
        }
    }
    let child = Command::new(&binary)
        .env("AGENTOS_DATA_DIR", &config.data_dir)
        .env("AGENTOS_WORKSPACE_ROOT", &config.workspace_root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot spawn {binary}: {e}"))?;
    *guard = Some(child);
    Ok(format!("runtime started from {binary}"))
}

#[tauri::command]
fn stop_runtime(state: tauri::State<'_, RuntimeHandle>) -> Result<String, String> {
    let mut guard = state.0.lock().map_err(|_| "runtime handle is poisoned".to_string())?;
    match guard.take() {
        Some(mut child) => {
            child.kill().map_err(|e| e.to_string())?;
            Ok("runtime stopped".to_string())
        }
        None => Ok("no runtime was started by this app".to_string()),
    }
}

#[tauri::command]
fn data_dir() -> String {
    DesktopConfig::default().data_dir
}

pub fn run() {
    tauri::Builder::default()
        .manage(RuntimeHandle(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            runtime_config,
            runtime_health,
            start_runtime,
            stop_runtime,
            data_dir
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Agent OS desktop shell");
}
