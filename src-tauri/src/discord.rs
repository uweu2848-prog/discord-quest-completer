use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Executable {
    /// Relative path Discord expects the process to run from,
    /// e.g. "SomeGame/Binaries/Win64/Game.exe".
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

const DETECTABLE_URL: &str = "https://discord.com/api/v10/applications/detectable";

/// Pulls the full list of games Discord's client can auto-detect for
/// Rich Presence / Quests, and keeps only ones with a win32 executable
/// (the only OS the dummy runner currently targets).
pub async fn fetch_detectable_games() -> Result<Vec<DetectableApp>, reqwest::Error> {
    let client = reqwest::Client::builder()
        .user_agent("discord-quest-completer/0.1 (+https://github.com/)")
        .build()?;

    let mut apps: Vec<DetectableApp> = match client.get(DETECTABLE_URL).send().await {
        Ok(resp) => resp.json().await.unwrap_or_default(),
        Err(_) => Vec::new(), // Fallback if offline or blocked
    };

    // --- YOUR CUSTOM GAMES LIBRARY ---
    let manual_games = vec![
        DetectableApp {
            id: "1168655587958063205".to_string(),
            name: "Dragonstride: Legacy Reborn".to_string(),
            icon: None,
            executables: vec![Executable {
                name: "Dragonstride.exe".to_string(),
                os: "win32".to_string(),
                is_launcher: false,
            }],
        },
        // Add any other missing games here following the exact same format:
        /*
        DetectableApp {
            id: "DISCORD_APPLICATION_ID".to_string(),
            name: "Game Name Here".to_string(),
            icon: None,
            executables: vec![Executable {
                name: "executable_name.exe".to_string(),
                os: "win32".to_string(),
                is_launcher: false,
            }],
        },
        */
    ];

    // Inject manual games, avoiding duplicates if Discord ever adds them natively
    for manual_app in manual_games {
        if !apps.iter().any(|a| a.id == manual_app.id || a.name.to_lowercase() == manual_app.name.to_lowercase()) {
            apps.push(manual_app);
        }
    }
    // ---------------------------------

    apps.retain(|a| a.executables.iter().any(|e| e.os == "win32"));
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    Ok(apps)
}
