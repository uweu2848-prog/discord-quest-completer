#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{CustomMenuItem, Manager, SystemTray, SystemTrayEvent, SystemTrayMenu};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

// -----------------------------------------------------------------------------
// Structs & Models
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Executable {
    pub name: String,
    pub os: String,
    #[serde(default)]
    pub is_launcher: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DetectableApp {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub executables: Vec<Executable>,
}

#[derive(Serialize, Clone)]
pub struct QuestStatus {
    pub id: String,
    pub name: String,
    pub elapsed_secs: u64,
    pub total_secs: u64,
    pub done: bool,
}

// -----------------------------------------------------------------------------
// Discord API Helpers
// -----------------------------------------------------------------------------

pub async fn fetch_detectable_games() -> Result<Vec<DetectableApp>, reqwest::Error> {
    let client = reqwest::Client::builder()
        .user_agent("discord-quest-completer/0.1")
        .build()?;

    let url = format!("{}/applications/detectable", DISCORD_API_BASE);
    let mut apps: Vec<DetectableApp> = client.get(&url).send().await?.json().await?;

    apps.retain(|a| a.executables.iter().any(|e| e.os == "win32"));
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(apps)
}

// -----------------------------------------------------------------------------
// Process Management
// -----------------------------------------------------------------------------

struct RunningEntry {
    child: Child,
    name: String,
    started_at: Instant,
    duration: Duration,
    target_path: PathBuf,
}

pub struct ProcessManager {
    running: HashMap<String, RunningEntry>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            running: HashMap::new(),
        }
    }

    pub fn launch(&mut self, game: &DetectableApp, games_dir: &Path, duration: Duration) -> std::io::Result<()> {
        self.stop(&game.id);

        let exe = game.executables.iter().find(|e| e.os == "win32").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no win32 executable for game")
        })?;

        let target: PathBuf = games_dir.join(&game.id).join(&exe.name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let current_exe = std::env::current_exe()?;
        std::fs::copy(&current_exe, &target)?;

        let mut cmd = std::process::Command::new(&target);
        cmd.arg("--dummy-runner")
            .arg(duration.as_secs().to_string())
            .current_dir(target.parent().unwrap());

        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

        let child = cmd.spawn()?;

        self.running.insert(
            game.id.clone(),
            RunningEntry {
                child,
                name: game.name.clone(),
                started_at: Instant::now(),
                duration,
                target_path: target,
            },
        );

        Ok(())
    }

    pub fn stop(&mut self, game_id: &str) {
        if let Some(mut entry) = self.running.remove(game_id) {
            let _ = entry.child.kill();
            let _ = entry.child.wait();
            let _ = std::fs::remove_file(&entry.target_path);
        }
    }

    pub fn stop_all(&mut self) {
        let keys: Vec<String> = self.running.keys().cloned().collect();
        for id in keys {
            self.stop(&id);
        }
    }

    pub fn snapshot(&mut self) -> Vec<QuestStatus> {
        let mut finished_ids = Vec::new();
        for (id, entry) in self.running.iter_mut() {
            if matches!(entry.child.try_wait(), Ok(Some(_))) {
                finished_ids.push(id.clone());
            }
        }
        for id in &finished_ids {
            if let Some(entry) = self.running.remove(id) {
                let _ = std::fs::remove_file(&entry.target_path);
            }
        }

        self.running
            .iter()
            .map(|(id, entry)| {
                let elapsed = entry.started_at.elapsed();
                QuestStatus {
                    id: id.clone(),
                    name: entry.name.clone(),
                    elapsed_secs: elapsed.as_secs().min(entry.duration.as_secs()),
                    total_secs: entry.duration.as_secs(),
                    done: elapsed >= entry.duration,
                }
            })
            .collect()
    }
}

fn discord_process_names() -> [&'static str; 4] {
    ["Discord.exe", "DiscordCanary.exe", "DiscordPTB.exe", "DiscordDevelopment.exe"]
}

// -----------------------------------------------------------------------------
// Tauri App Commands
// -----------------------------------------------------------------------------

#[tauri::command]
async fn fetch_games() -> Result<Vec<DetectableApp>, String> {
    fetch_detectable_games().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn start_quest(
    game: DetectableApp,
    minutes: u32,
    state: tauri::State<'_, Mutex<ProcessManager>>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let mut pm = state.lock().unwrap();
    let path = app_handle
        .path_resolver()
        .app_local_data_dir()
        .unwrap_or_else(|| PathBuf::from("."));
    let duration = Duration::from_secs(minutes.max(1) as u64 * 60);
    pm.launch(&game, &path, duration).map_err(|e| e.to_string())
}

#[tauri::command]
fn stop_quest(game_id: String, state: tauri::State<'_, Mutex<ProcessManager>>) {
    let mut pm = state.lock().unwrap();
    pm.stop(&game_id);
}

#[tauri::command]
fn quest_status(state: tauri::State<'_, Mutex<ProcessManager>>) -> Vec<QuestStatus> {
    let mut pm = state.lock().unwrap();
    pm.snapshot()
}

#[tauri::command]
fn check_discord_running() -> bool {
    let mut cmd = std::process::Command::new("tasklist");
    cmd.args(["/FO", "CSV", "/NH"]);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
            discord_process_names()
                .iter()
                .any(|name| stdout.contains(&name.to_lowercase()))
        }
        Err(_) => false,
    }
}

#[tauri::command]
fn restart_discord() -> Result<(), String> {
    for name in discord_process_names() {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/IM", name, "/F"]);

        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);

        let _ = cmd.output();
    }

    std::thread::sleep(Duration::from_millis(1000));

    let local_appdata = std::env::var("LOCALAPPDATA")
        .map_err(|_| "LOCALAPPDATA not set".to_string())?;
    let update_exe = PathBuf::from(local_appdata).join("Discord").join("Update.exe");

    let mut cmd = std::process::Command::new(&update_exe);
    cmd.args(["--processStart", "Discord.exe"]);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    cmd.spawn()
        .map_err(|e| format!("failed to relaunch Discord: {e}"))?;

    Ok(())
}

// -----------------------------------------------------------------------------
// Main Entry
// -----------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();

    if let Some(pos) = args.iter().position(|a| a == "--dummy-runner") {
        let duration_secs = args.get(pos + 1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(900);
        let app_id = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().and_then(|parent| parent.file_name()).map(|name| name.to_string_lossy().to_string()))
            .unwrap_or_default();

        #[cfg(target_os = "windows")]
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }

        let mut client_opt = DiscordIpcClient::new(&app_id).ok();
        let mut is_connected = false;

        if let Some(ref mut client) = client_opt {
            if client.connect().is_ok() {
                let current_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;

                let end_time = current_time + duration_secs as i64;

                let buttons = vec![activity::Button::new(
                    "Created by Yuniku",
                    "https://github.com/uweu2848-prog",
                )];

                let payload = activity::Activity::new()
                    .details("Running Yuni Orb")
                    .state("Quest session")
                    .timestamps(activity::Timestamps::new().start(current_time).end(end_time))
                    .buttons(buttons);

                let _ = client.set_activity(payload);
                is_connected = true;
            }
        }

        std::thread::sleep(Duration::from_secs(duration_secs));

        if is_connected {
            if let Some(ref mut client) = client_opt {
                let _ = client.close();
            }
        }

        #[cfg(target_os = "windows")]
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
        return;
    }

    let quit = CustomMenuItem::new("quit".to_string(), "Quit");
    let tray_menu = SystemTrayMenu::new().add_item(quit);
    let system_tray = SystemTray::new().with_menu(tray_menu);

    let app = tauri::Builder::default()
        .manage(Mutex::new(ProcessManager::new()))
        .system_tray(system_tray)
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::MenuItemClick { id, .. } => {
                if id == "quit" {
                    let state = app.state::<Mutex<ProcessManager>>();
                    if let Ok(mut pm) = state.inner().lock() {
                        pm.stop_all();
                    }
                    std::process::exit(0);
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            fetch_games,
            start_quest,
            stop_quest,
            quest_status,
            check_discord_running,
            restart_discord
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| match event {
        tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. } => {
            let state = app_handle.state::<Mutex<ProcessManager>>();
            if let Ok(mut pm) = state.inner().lock() {
                pm.stop_all();
            }
        }
        _ => {}
    });
}