//! Moves an install across from the old Voxtray name. Voxtray ran as
//! `dev.harshal.voxtray`, so on the first launch as Vox its settings,
//! downloads and API keys sit under that id. This moves them to the current
//! one and removes Voxtray's login item, so both apps don't start at login.
//!
//! It runs on every launch but costs one directory check once nothing is left
//! to move. Anything it can't move yet (say, a file the old app still holds
//! open) stays put and is tried again next launch.

use std::fs;
use std::path::Path;

use crate::settings::{self, Provider};

struct Legacy {
    /// The current identifier.
    id: &'static str,
    /// The identifier and product name the same build had as Voxtray.
    old_id: &'static str,
    old_name: &'static str,
}

const LEGACY: &[Legacy] = &[
    Legacy { id: "dev.harshal.vox", old_id: "dev.harshal.voxtray", old_name: "Voxtray" },
    Legacy { id: "dev.harshal.vox.dev", old_id: "dev.harshal.voxtray.dev", old_name: "Voxtray Dev" },
];

/// Run before anything creates this build's folders. Returns what happened,
/// for the log, which isn't open yet.
pub fn from_voxtray() -> Vec<String> {
    let mut report = Vec::new();
    let Some(legacy) = LEGACY.iter().find(|l| l.id == settings::app_id()) else {
        return report;
    };
    let config = settings::dirs_config_dir();
    let had_config = config.as_ref().is_some_and(|c| c.join(legacy.old_id).is_dir());

    // On macOS both bases are the same folder; the second pass finds it gone.
    for base in [config, crate::download::base_data_dir()].into_iter().flatten() {
        move_contents(&base.join(legacy.old_id), &base.join(legacy.id), &mut report);
    }
    if had_config {
        move_keys(legacy, &mut report);
        remove_login_item(legacy, &mut report);
    }
    report
}

/// Moves each entry of `old` into `new`, then removes `old` once it's empty.
/// Never overwrites: an entry `new` already has stays in `old`.
fn move_contents(old: &Path, new: &Path, report: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(old) else { return };
    if let Err(e) = fs::create_dir_all(new) {
        report.push(format!("could not create {}: {e}", new.display()));
        return;
    }
    for entry in entries.flatten() {
        let from = entry.path();
        // The old log is replaced on every start anyway.
        if entry.file_name() == "voxtray.log" {
            let _ = fs::remove_file(&from);
            continue;
        }
        let to = new.join(entry.file_name());
        if to.exists() {
            continue;
        }
        match fs::rename(&from, &to) {
            Ok(()) => report.push(format!("moved {} to {}", from.display(), to.display())),
            Err(e) => report.push(format!("could not move {}: {e}", from.display())),
        }
    }
    let _ = fs::remove_dir(old);
}

/// Copies each API key from the old keychain service to the new one, unless
/// the new one already has it, then deletes the old copy.
fn move_keys(legacy: &Legacy, report: &mut Vec<String>) {
    for provider in [Provider::Openai, Provider::Elevenlabs] {
        let Some(account) = provider.key_account() else { continue };
        let Ok(old) = keyring::Entry::new(legacy.old_id, account) else { continue };
        let Ok(key) = old.get_password() else { continue };
        if settings::get_api_key(provider).is_none() {
            if let Err(e) = settings::set_api_key(provider, &key) {
                report.push(format!("could not move {account}: {e}"));
                continue;
            }
            report.push(format!("moved {account} to the new keychain entry"));
        }
        let _ = old.delete_credential();
    }
}

/// Removes Voxtray's login item. Vox adds its own from the moved settings.
fn remove_login_item(legacy: &Legacy, report: &mut Vec<String>) {
    let Ok(old) = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(legacy.old_name)
        .set_app_path("")
        .set_use_launch_agent(true)
        .build()
    else {
        return;
    };
    if old.is_enabled().unwrap_or(false) {
        match old.disable() {
            Ok(()) => report.push(format!("removed the {} login item", legacy.old_name)),
            Err(e) => report.push(format!("could not remove the {} login item: {e}", legacy.old_name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vox-migrate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn moves_everything_and_drops_the_old_folder_and_log() {
        let root = temp("all");
        let (old, new) = (root.join("old"), root.join("new"));
        fs::create_dir_all(old.join("kokoro")).unwrap();
        fs::write(old.join("settings.json"), "{}").unwrap();
        fs::write(old.join("voxtray.log"), "x").unwrap();
        fs::write(old.join("kokoro/model.onnx"), "m").unwrap();

        let mut report = Vec::new();
        move_contents(&old, &new, &mut report);

        assert!(!old.exists());
        assert_eq!(fs::read_to_string(new.join("settings.json")).unwrap(), "{}");
        assert_eq!(fs::read_to_string(new.join("kokoro/model.onnx")).unwrap(), "m");
        assert!(!new.join("voxtray.log").exists());
        assert_eq!(report.len(), 2, "{report:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn never_overwrites_what_the_new_folder_has() {
        let root = temp("keep");
        let (old, new) = (root.join("old"), root.join("new"));
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("settings.json"), "old").unwrap();
        fs::write(new.join("settings.json"), "new").unwrap();

        move_contents(&old, &new, &mut Vec::new());

        assert_eq!(fs::read_to_string(new.join("settings.json")).unwrap(), "new");
        assert_eq!(fs::read_to_string(old.join("settings.json")).unwrap(), "old");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn does_nothing_without_an_old_folder() {
        let root = temp("none");
        let mut report = Vec::new();
        move_contents(&root.join("old"), &root.join("new"), &mut report);
        assert!(report.is_empty());
        assert!(!root.join("new").exists());
    }
}
