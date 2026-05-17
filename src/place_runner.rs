use std::{
    collections::HashSet,
    path::PathBuf,
    process::{self, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use fs_err as fs;
use fs_err::File;
use roblox_install::RobloxStudio;

use crate::{
    message_receiver::{Message, MessageReceiver, MessageReceiverOptions, RobloxMessage},
    plugin::RunInRbxPlugin,
};

/// A wrapper for process::Child that force-kills the process on drop.
struct KillOnDrop(process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ignored = self.0.kill();
    }
}

/// Returns the Windows PIDs of all running RobloxStudioBeta.exe processes.
/// Uses tasklist.exe so the PIDs are in Windows PID space — valid for taskkill.
fn studio_pids() -> HashSet<String> {
    let output = Command::new("tasklist.exe")
        .args(&["/FI", "IMAGENAME eq RobloxStudioBeta.exe", "/FO", "CSV", "/NH"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                // CSV: "RobloxStudioBeta.exe","27460","Console","1","530,308 K"
                let fields: Vec<&str> = line.split(',').collect();
                fields.get(1).map(|pid| pid.trim_matches('"').to_owned())
            })
            .collect(),
        Err(_) => HashSet::new(),
    }
}

pub struct PlaceRunner {
    pub port: u16,
    pub place_path: PathBuf,
    pub server_id: String,
    pub lua_script: String,
}

impl PlaceRunner {
    pub fn run(&self, sender: mpsc::Sender<Option<RobloxMessage>>) -> Result<(), anyhow::Error> {
        let studio_install =
            RobloxStudio::locate().context("Could not locate a Roblox Studio installation.")?;

        let plugin_file_path = studio_install
            .plugins_path()
            .join(format!("run_in_roblox-{}.rbxmx", self.port));

        let plugin = RunInRbxPlugin {
            port: self.port,
            server_id: &self.server_id,
            lua_script: &self.lua_script,
        };

        let plugin_file = File::create(&plugin_file_path)?;
        plugin.write(plugin_file)?;

        let message_receiver = MessageReceiver::start(MessageReceiverOptions {
            port: self.port,
            server_id: self.server_id.to_owned(),
        });

        // Snapshot existing Studio PIDs so cleanup only kills the instance we spawn.
        let pids_before = studio_pids();

        let _studio_process = KillOnDrop(
            Command::new(studio_install.application_path())
                .arg(format!("{}", self.place_path.display()))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );

        let first_message = message_receiver
            .recv_timeout(Duration::from_secs(60))
            .ok_or_else(|| {
                anyhow!("Timeout reached while waiting for Roblox Studio to come online")
            })?;

        match first_message {
            Message::Start => {}
            _ => bail!("Invalid first message received from Roblox Studio plugin"),
        }

        loop {
            match message_receiver.recv() {
                Message::Start => {}
                Message::Stop => {
                    sender.send(None)?;
                    break;
                }
                Message::Messages(roblox_messages) => {
                    for message in roblox_messages.into_iter() {
                        sender.send(Some(message))?;
                    }
                }
            }
        }

        message_receiver.stop();
        fs::remove_file(&plugin_file_path)?;

        // Kill only the Studio instance(s) we spawned. tasklist/taskkill both use
        // Windows PIDs, avoiding the WSL2 Linux-PID mismatch from child.id().
        // /T also catches the RobloxCrashHandler child each instance spawns.
        let pids_after = studio_pids();
        for pid in pids_after.difference(&pids_before) {
            let _ = Command::new("taskkill.exe")
                .args(&["/F", "/T", "/PID", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output();
        }

        Ok(())
    }
}
