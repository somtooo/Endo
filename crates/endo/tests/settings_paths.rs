#![cfg(target_os = "linux")]

use smol::process::Command;
use std::{env, fs};

#[test]
fn settings_paths_are_isolated() -> anyhow::Result<()> {
    if let Some(root) = env::var_os("ENDO_TEST_SETTINGS_ROOT") {
        let root = std::path::PathBuf::from(root);
        if env::var_os("ENDO_TEST_CUSTOM_DATA").is_some() {
            let custom = root.join("custom");
            paths::set_custom_data_dir(
                custom
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid test path"))?,
            );
            endo::init_settings(true)?;
            assert_eq!(*paths::settings_file(), custom.join("config/settings.json"));
            assert!(!paths::settings_file().exists());
        } else {
            let zed = root.join("config/zed");
            fs::create_dir_all(&zed)?;
            fs::write(zed.join("settings.json"), "// copied from Zed\n{}")?;
            endo::init_settings(false)?;
            let endo = root.join("config/endo");
            assert_eq!(*paths::settings_file(), endo.join("settings.json"));
            assert_eq!(
                *paths::global_settings_file(),
                endo.join("global_settings.json")
            );
            assert_eq!(
                *paths::settings_backup_file(),
                endo.join("settings_backup.json")
            );
            assert_eq!(*paths::keymap_file(), zed.join("keymap.json"));
            fs::write(paths::settings_file(), "endo only")?;
            assert_eq!(
                fs::read_to_string(zed.join("settings.json"))?,
                "// copied from Zed\n{}"
            );
        }
        return Ok(());
    }
    let root = tempfile::tempdir()?;
    for custom in [false, true] {
        let mut command = Command::new(env::current_exe()?);
        command
            .args(["--exact", "settings_paths_are_isolated", "--nocapture"])
            .env("ENDO_TEST_SETTINGS_ROOT", root.path())
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env_remove("FLATPAK_XDG_CONFIG_HOME");
        if custom {
            command.env("ENDO_TEST_CUSTOM_DATA", "1");
        }
        anyhow::ensure!(
            smol::block_on(command.status())?.success(),
            "settings path child failed"
        );
    }
    Ok(())
}
