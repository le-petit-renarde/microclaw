// Prevent the console window from appearing on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use regex::Regex;
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

            tauri::async_runtime::spawn(async move {
                match start_microclaw_server(&microclaw_binary).await {
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

fn find_microclaw_binary() -> String {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(dir) = exe_path.parent() {
            let candidate = dir
                .join("microclaw")
                .with_extension(std::env::consts::EXE_EXTENSION);
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    "microclaw".to_string()
}

async fn start_microclaw_server(binary: &str) -> anyhow::Result<(u16, tokio::process::Child)> {
    let mut child = Command::new(binary)
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
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