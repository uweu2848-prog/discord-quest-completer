use crate::discord::DetectableApp;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

struct RunningEntry {
    child: Child,
    name: String,
    started_at: Instant,
    duration: Duration,
    /// Bumped on every (re)launch of a given game id. Lets a stale
    /// auto-stop timer recognize it's no longer looking at the run it
    /// was scheduled for, so it doesn't kill a fresh manual restart.
    generation: u64,
}

#[derive(Serialize, Clone)]
pub struct QuestStatus {
    pub id: String,
    pub name: String,
    pub elapsed_secs: u64,
    pub total_secs: u64,
    pub done: bool,
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

    /// Copies the dummy runner into games/<app-id>/<expected-relative-path>
    /// and spawns it for `duration`. Returns the generation id the caller
    /// should hand back to `stop_if_current` when scheduling the auto-stop.
    pub fn launch(
        &mut self,
        game: &DetectableApp,
        games_dir: &Path,
        runner_src: &Path,
        duration: Duration,
    ) -> std::io::Result<u64> {
        // Fresh start: any previous run of this same game is replaced.
        self.stop(&game.id);

        let exe = game
            .executables
            .iter()
            .find(|e| e.os == "win32")
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no win32 executable for game")
            })?;

        let target: PathBuf = games_dir.join(&game.id).join(&exe.name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(runner_src, &target)?;

        let mut cmd = std::process::Command::new(&target);
        cmd.arg(&game.name)
            .current_dir(target.parent().unwrap());

        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW flag

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
            },
        );

        Ok(generation)
    }

    pub fn stop(&mut self, game_id: &str) {
        if let Some(mut entry) = self.running.remove(game_id) {
            let _ = entry.child.kill();
            let _ = entry.child.wait();
        }
    }

    /// Called by the scheduled auto-stop timer. Only actually stops the
    /// process if it's still the same run that was started (i.e. nobody
    /// cancelled and manually restarted it in the meantime).
    pub fn stop_if_current(&mut self, game_id: &str, generation: u64) {
        let matches = self
            .running
            .get(game_id)
            .map_or(false, |e| e.generation == generation);
        if matches {
            self.stop(game_id);
        }
    }

    pub fn stop_all(&mut self) {
        for (_, mut entry) in self.running.drain() {
            let _ = entry.child.kill();
            let _ = entry.child.wait();
        }
    }

    /// Reaps any runner that exited on its own, then returns progress for
    /// everything still active.
    pub fn snapshot(&mut self) -> Vec<QuestStatus> {
        self.running
            .retain(|_, entry| matches!(entry.child.try_wait(), Ok(None)));

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