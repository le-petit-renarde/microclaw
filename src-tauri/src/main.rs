// Prevent the console window from appearing on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use regex::Regex;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::Mutex;

struct AppState {
    server_port: Arc<Mutex<Option<u16>>>,
    child_process: Arc<Mutex<Option<tokio::process::Child>>>,
}

#[tauri::command]
async fn get_server_url(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let port = state.server_port.lock().await;
    port.map(|p| format!("http://127.0.0.1:{}", p))
        .ok_or_else(|| "Server not started yet".to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let app_handle = app.handle().clone();
            let microclaw_binary = find_microclaw_binary();
            let gui_dir = gui_directory();
            let config_path = find_config_file(&gui_dir);

            tauri::async_runtime::spawn(async move {
                match start_microclaw_server(&microclaw_binary, &gui_dir, config_path.as_ref()).await {
                    Ok((port, mut child)) => {
                        println!("MicroClaw server started on port {}", port);

                        tokio::select! {
                            status = child.wait() => {
                                println!("MicroClaw server exited with: {:?}", status);
                            }
                            _ = tokio::signal::ctrl_c() => {
                                println!("Shutting down...");
                                let _ = child.kill().await;
                            }
                        }

                        app_handle.exit(0);
                    }
                    Err(e) => {
                        eprintln!("Failed to start MicroClaw server: {}", e);
                        app_handle.exit(1);
                    }
                }
            });

            Ok(())
        })
        .manage(AppState {
            server_port: Arc::new(Mutex::new(None)),
            child_process: Arc::new(Mutex::new(None)),
        })
        .run(tauri::generate_context!())
        .expect("error while running MicroClaw GUI");
}

fn gui_directory() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn find_microclaw_binary() -> String {
    let gui_dir = gui_directory();
    let candidate = gui_dir
        .join("microclaw")
        .with_extension(std::env::consts::EXE_EXTENSION);
    if candidate.exists() {
        return candidate.to_string_lossy().to_string();
    }
    "microclaw".to_string()
}

fn find_config_file(gui_dir: &PathBuf) -> Option<PathBuf> {
    // Check MICROCLAW_CONFIG env var first
    if let Ok(env_path) = std::env::var("MICROCLAW_CONFIG") {
        let p = PathBuf::from(&env_path);
        if p.exists() {
            return Some(p);
        }
    }
    // Then look alongside the GUI binary
    for name in &["microclaw.config.yaml", "microclaw.config.yml"] {
        let candidate = gui_dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

async fn start_microclaw_server(
    binary: &str,
    gui_dir: &PathBuf,
    config_path: Option<&PathBuf>,
) -> anyhow::Result<(u16, tokio::process::Child)> {
    let mut cmd = Command::new(binary);
    cmd.arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .current_dir(gui_dir);

    // Pass --config if we found one
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
        // Also set the env var so child processes (gateway, etc.) pick it up
        cmd.env("MICROCLAW_CONFIG", cfg);
    }

    let mut child = cmd
        .spawn()
        .context("Failed to spawn microclaw process. Make sure microclaw is installed.")?;

    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;

    let url_regex = Regex::new(r"Web UI available at http://[\d.]+:(\d+)")?;

    let stdout_reader = BufReader::new(stdout);
    let mut stdout_lines = stdout_reader.lines();

    let stderr_reader = BufReader::new(stderr);
    let mut stderr_lines = stderr_reader.lines();

    let port: Arc<Mutex<Option<u16>>> = Arc::new(Mutex::new(None));
    let port_clone = port.clone();

    let timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            line_result = stdout_lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        println!("[microclaw] {}", line);
                        if let Some(caps) = url_regex.captures(&line) {
                            if let Some(port_str) = caps.get(1) {
                                let p: u16 = port_str.as_str().parse()?;
                                *port_clone.lock().await = Some(p);
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("Error reading stdout: {}", e);
                        break;
                    }
                }
            }
            line_result = stderr_lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        eprintln!("[microclaw:stderr] {}", line);
                    }
                    _ => {}
                }
            }
            _ = &mut timeout => {
                anyhow::bail!("Timed out waiting for MicroClaw server to start");
            }
        }
    }

    tokio::spawn(async move {
        while let Ok(Some(line)) = stdout_lines.next_line().await {
            println!("[microclaw] {}", line);
        }
    });
    tokio::spawn(async move {
        while let Ok(Some(line)) = stderr_lines.next_line().await {
            eprintln!("[microclaw:stderr] {}", line);
        }
    });

    let port = port_clone
        .lock()
        .await
        .ok_or_else(|| anyhow::anyhow!("Could not determine server port"))?;
    Ok((port, child))
}