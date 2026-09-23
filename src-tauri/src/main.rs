#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{CustomMenuItem, Manager, SystemTray, SystemTrayEvent, SystemTrayMenu};

use std::ptr;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect,
    SetBkMode, SetTextColor, DT_CENTER, DT_WORDBREAK, PAINTSTRUCT, TRANSPARENT,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, KillTimer,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetTimer, ShowWindow,
    TranslateMessage, MSG, SW_SHOWNORMAL, WM_CREATE, WM_DESTROY, WM_PAINT, WM_TIMER,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

static REMAINING_SECS: AtomicU64 = AtomicU64::new(0);
static API_RATE_LIMITER: Mutex<Option<Instant>> = Mutex::new(None);

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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserQuest {
    pub id: String,
    pub config: serde_json::Value,
    pub user_status: Option<serde_json::Value>,
    pub targeted_game: Option<DetectableApp>,
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

fn build_auth_client(token: &str) -> Result<reqwest::Client, String> {
    let mut headers = HeaderMap::new();
    let mut auth_value = HeaderValue::from_str(token).map_err(|e| e.to_string())?;
    auth_value.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_value);

    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .default_headers(headers)
        .build()
        .map_err(|e| e.to_string())
}

fn enforce_api_rate_limit(min_interval: Duration) -> Result<(), String> {
    let mut last = API_RATE_LIMITER.lock().map_err(|e| e.to_string())?;
    if let Some(previous) = *last {
        let elapsed = previous.elapsed();
        if elapsed < min_interval {
            let wait = min_interval - elapsed;
            drop(last);
            std::thread::sleep(wait);
            let mut retry_guard = API_RATE_LIMITER.lock().map_err(|e| e.to_string())?;
            *retry_guard = Some(Instant::now());
            return Ok(());
        }
    }

    *last = Some(Instant::now());
    Ok(())
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let seconds = value.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

// -----------------------------------------------------------------------------
// Process Management
// -----------------------------------------------------------------------------

struct RunningEntry {
    child: Child,
    name: String,
    started_at: Instant,
    duration: Duration,
    generation: u64,
    target_path: PathBuf,
}

pub struct ProcessManager {
    running: HashMap<String, RunningEntry>,
    next_generation: u64,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            running: HashMap::new(),
            next_generation: 0,
        }
    }

    pub fn launch(&mut self, game: &DetectableApp, games_dir: &Path, duration: Duration) -> std::io::Result<u64> {
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

        let generation = self.next_generation;
        self.next_generation += 1;

        self.running.insert(
            game.id.clone(),
            RunningEntry {
                child,
                name: game.name.clone(),
                started_at: Instant::now(),
                duration,
                generation,
                target_path: target,
            },
        );

        Ok(generation)
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

// -----------------------------------------------------------------------------
// Dummy Runner Windows API
// -----------------------------------------------------------------------------

#[cfg(target_os = "windows")]
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let dark_mode: i32 = 1;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as i32,
                &dark_mode as *const _ as _,
                std::mem::size_of::<i32>() as u32,
            );
            SetTimer(hwnd, 1, 1000, None);
            0
        }
        WM_TIMER => {
            let remaining = REMAINING_SECS.load(Ordering::SeqCst);
            if remaining > 0 {
                REMAINING_SECS.store(remaining - 1, Ordering::SeqCst);
                InvalidateRect(hwnd, ptr::null(), 0);
            } else {
                PostQuitMessage(0);
            }
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);

            let brush = CreateSolidBrush(0x001A1A1A);
            FillRect(hdc, &rect, brush);
            DeleteObject(brush as _);

            SetBkMode(hdc, TRANSPARENT as u32);
            SetTextColor(hdc, 0x00FFFFFF);

            let remaining = REMAINING_SECS.load(Ordering::SeqCst);
            let text = format!("✨ Quest Runner Active\nTime Remaining: {}s", remaining);
            let mut wide_text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

            DrawTextW(
                hdc,
                wide_text.as_mut_ptr(),
                -1,
                &mut rect,
                DT_CENTER | DT_WORDBREAK,
            );

            EndPaint(hdc, &ps);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, 1);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
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

async fn fetch_user_quests_inner(client: &reqwest::Client) -> Result<Vec<UserQuest>, String> {
    enforce_api_rate_limit(Duration::from_millis(250))?;

    let res = client
        .get(format!("{}/users/@me/quests", DISCORD_API_BASE))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if res.status() == StatusCode::TOO_MANY_REQUESTS {
        if let Some(delay) = parse_retry_after(res.headers()) {
            return Err(format!("Rate limited by Discord. Retry after {}s.", delay.as_secs()));
        }
        return Err("Rate limited by Discord. Please slow down requests.".to_string());
    }

    if !res.status().is_success() {
        return Err(format!("Failed to authenticate or fetch quests: {}", res.status()));
    }

    let quests: Vec<UserQuest> = res.json().await.map_err(|e| e.to_string())?;
    Ok(quests)
}

#[tauri::command]
async fn fetch_user_quests(token: String) -> Result<Vec<UserQuest>, String> {
    let client = build_auth_client(&token)?;
    fetch_user_quests_inner(&client).await
}

#[tauri::command]
async fn fetch_current_quest(token: String) -> Result<Option<UserQuest>, String> {
    let client = build_auth_client(&token)?;
    let quests = fetch_user_quests_inner(&client).await?;

    Ok(quests.into_iter().find(|quest| {
        quest
            .config
            .get("active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || quest.targeted_game.is_some()
    }))
}

#[tauri::command]
async fn claim_quest_reward(token: String, quest_id: String) -> Result<serde_json::Value, String> {
    let client = build_auth_client(&token)?;
    enforce_api_rate_limit(Duration::from_millis(500))?;

    let payload = serde_json::json!({ "platform": 0 });

    let res = client
        .post(format!("{}/quests/{}/claim", DISCORD_API_BASE, quest_id))
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        return Err(format!("Failed to claim quest reward: Status {}", res.status()));
    }

    let claim_data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    Ok(claim_data)
}

#[tauri::command]
async fn auto_watch_video(
    token: String,
    quest_id: String,
    target_seconds: Option<u64>,
    auto_claim: bool,
) -> Result<(), String> {
    if token.trim().is_empty() {
        return Err("No token provided.".to_string());
    }

    let client = build_auth_client(&token)?;

    tokio::spawn(async move {
        let total_duration = target_seconds.unwrap_or(120);
        let mut current_position = 0u64;

        while current_position <= total_duration {
            let payload = serde_json::json!({
                "timestamp": current_position
            });

            let req = client
                .post(format!("{}/quests/{}/video-progress", DISCORD_API_BASE, quest_id))
                .json(&payload)
                .send()
                .await;

            match req {
                Ok(res) => {
                    if !res.status().is_success() {
                        break;
                    }
                }
                Err(_) => break,
            }

            current_position += 10;

            let jitter = rand::random::<u64>() % 2000;
            tokio::time::sleep(Duration::from_millis(10000 + jitter)).await;
        }

        if auto_claim {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let claim_payload = serde_json::json!({ "platform": 0 });
            let _ = client
                .post(format!("{}/quests/{}/claim", DISCORD_API_BASE, quest_id))
                .json(&claim_payload)
                .send()
                .await;
        }
    });

    Ok(())
}

#[tauri::command]
fn start_quest(
    game: DetectableApp,
    minutes: u32,
    state: tauri::State<'_, Mutex<ProcessManager>>,
    app_handle: tauri::AppHandle,
) -> Result<u64, String> {
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

    let output = cmd.output();

    match output {
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

    let local_appdata = std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA not set".to_string())?;
    let update_exe = PathBuf::from(local_appdata).join("Discord").join("Update.exe");

    let mut cmd = std::process::Command::new(&update_exe);
    cmd.args(["--processStart", "Discord.exe"]);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    cmd.spawn()
        .map_err(|e| format!("failed to relaunch Discord: {e}"))?;

    Ok(())
}

#[tauri::command]
async fn auto_start_missing_quests(
    token: String,
    duration_minutes: u32,
    state: tauri::State<'_, Mutex<ProcessManager>>,
    app_handle: tauri::AppHandle,
) -> Result<Vec<String>, String> {
    if token.trim().is_empty() {
        return Err("No Discord token provided.".to_string());
    }

    let client = build_auth_client(&token)?;
    let quests = fetch_user_quests_inner(&client).await?;
    let mut started_games = Vec::new();

    let path = app_handle
        .path_resolver()
        .app_local_data_dir()
        .unwrap_or_else(|| PathBuf::from("."));

    let mut pm = state.lock().map_err(|e| e.to_string())?;
    let current_status = pm.snapshot();

    for quest in quests {
        let is_completed = quest
            .user_status
            .as_ref()
            .and_then(|status| status.get("completed_at"))
            .map_or(false, |val| !val.is_null());

        if is_completed {
            continue;
        }

        if let Some(game) = quest.targeted_game {
            let is_running = current_status.iter().any(|s| s.id == game.id && !s.done);

            if !is_running {
                let duration = Duration::from_secs(duration_minutes.max(1) as u64 * 60);
                if pm.launch(&game, &path, duration).is_ok() {
                    started_games.push(game.name.clone());
                }
            }
        }
    }

    Ok(started_games)
}

// -----------------------------------------------------------------------------
// Main Entry
// -----------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();

    if let Some(pos) = args.iter().position(|a| a == "--dummy-runner") {
        let duration_secs = args.get(pos + 1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(900);
        REMAINING_SECS.store(duration_secs, Ordering::SeqCst);

        let current_exe = std::env::current_exe().unwrap();
        let app_id = current_exe.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();

        #[cfg(target_os = "windows")]
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);

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
                        "✨ Created by Yuniku",
                        "https://github.com/uweu2848-prog",
                    )];

                    let payload = activity::Activity::new()
                        .details("⚡Running YuiOrb")
                        .state("✨ Created by Yuniku")
                        .timestamps(
                            activity::Timestamps::new()
                                .start(current_time)
                                .end(end_time),
                        )
                        .buttons(buttons);

                    let _ = client.set_activity(payload);
                    is_connected = true;
                }
            }

            let h_instance = GetModuleHandleW(ptr::null());
            let class_name: Vec<u16> = "YunikuRunnerClass\0".encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: h_instance,
                hIcon: 0,
                hCursor: 0,
                hbrBackground: 0,
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: 0,
            };

            RegisterClassExW(&wc);

            let window_title: Vec<u16> = format!("Quest Dummy Runner\0").encode_utf16().collect();
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                100, 100, 420, 220,
                0, 0, h_instance, ptr::null()
            );

            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(hwnd);

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if is_connected {
                if let Some(ref mut client) = client_opt {
                    let _ = client.close();
                }
            }

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
            fetch_user_quests,
            fetch_current_quest,
            auto_watch_video,
            claim_quest_reward,
            start_quest,
            stop_quest,
            quest_status,
            check_discord_running,
            restart_discord,
            auto_start_missing_quests
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