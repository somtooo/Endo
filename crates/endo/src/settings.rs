use std::{
    fs,
    io::{self, Write},
    path::Path,
};

pub fn init_settings(custom_data_dir: bool) -> io::Result<()> {
    if custom_data_dir {
        return Ok(());
    }
    let source = paths::config_dir();
    let destination = source.with_file_name(if cfg!(windows) { "Endo" } else { "endo" });
    initialize_settings(source, &destination)?;
    paths::set_settings_dir(destination)
        .map_err(|_| io::Error::other("settings paths were already initialized"))
}

fn initialize_settings(source_dir: &Path, destination_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(destination_dir)?;
    if let (Ok(source), Ok(destination)) =
        (source_dir.canonicalize(), destination_dir.canonicalize())
        && source == destination
    {
        return Err(io::Error::other(format!(
            "{} points to Zed's configuration folder",
            destination_dir.display()
        )));
    }
    let marker = destination_dir.join(".settings-imported-from-zed");
    let import_from_zed = !marker.try_exists()?;
    for name in ["settings.json", "global_settings.json"] {
        let destination = destination_dir.join(name);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                let resolved_destination = destination.canonicalize()?;
                if !fs::metadata(&destination)?.is_file() {
                    return Err(io::Error::other(format!(
                        "{} is not a file",
                        destination.display()
                    )));
                }
                if source_dir.join(name).canonicalize().ok().as_ref() == Some(&resolved_destination)
                {
                    return Err(io::Error::other(format!(
                        "{} points to Zed's settings",
                        destination.display()
                    )));
                }
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        // Publish only complete files. Another launch must never read a partial
        // copy or have its newly created settings overwritten by this launch.
        let mut temporary = tempfile::NamedTempFile::new_in(destination_dir)?;
        let source = if import_from_zed {
            fs::File::open(source_dir.join(name))
        } else {
            Err(io::Error::from(io::ErrorKind::NotFound))
        };
        match source {
            Ok(mut source) => {
                io::copy(&mut source, &mut temporary)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                temporary.write_all(b"{}\n")?;
            }
            Err(error) => return Err(error),
        }
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error),
        }
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(marker)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_copied_once() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("zed");
        let destination = root.path().join("endo");
        fs::create_dir(&source)?;
        fs::write(
            source.join("settings.json"),
            b"// comment\n{\"theme\": \"One Dark\"}",
        )?;
        fs::write(source.join("global_settings.json"), b"zed global")?;
        initialize_settings(&source, &destination)?;
        assert_eq!(
            fs::read(destination.join("settings.json"))?,
            fs::read(source.join("settings.json"))?
        );
        fs::write(destination.join("settings.json"), b"endo settings")?;
        fs::write(source.join("global_settings.json"), b"updated zed")?;
        initialize_settings(&source, &destination)?;
        assert_eq!(
            fs::read(destination.join("settings.json"))?,
            b"endo settings"
        );
        assert_eq!(
            fs::read(destination.join("global_settings.json"))?,
            b"zed global"
        );
        assert_eq!(
            fs::read(source.join("settings.json"))?,
            b"// comment\n{\"theme\": \"One Dark\"}"
        );
        Ok(())
    }

    #[test]
    fn settings_start_empty_without_zed() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("zed");
        let destination = root.path().join("endo");
        initialize_settings(&source, &destination)?;
        assert_eq!(fs::read(destination.join("settings.json"))?, b"{}\n");
        assert_eq!(fs::read(destination.join("global_settings.json"))?, b"{}\n");
        Ok(())
    }

    #[test]
    fn unreadable_source_does_not_become_default_settings() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("zed");
        let destination = root.path().join("endo");
        fs::create_dir_all(source.join("settings.json"))?;
        assert!(initialize_settings(&source, &destination).is_err());
        assert!(!destination.join("settings.json").exists());
        Ok(())
    }

    #[test]
    fn deleting_settings_does_not_reimport_zed() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("zed");
        let destination = root.path().join("endo");
        fs::create_dir(&source)?;
        fs::write(source.join("settings.json"), b"zed settings")?;
        initialize_settings(&source, &destination)?;
        fs::remove_file(destination.join("settings.json"))?;
        initialize_settings(&source, &destination)?;
        assert_eq!(fs::read(destination.join("settings.json"))?, b"{}\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_become_independent_files() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("zed");
        let destination = root.path().join("endo");
        fs::create_dir(&source)?;
        let dotfiles = root.path().join("dotfiles.json");
        fs::write(&dotfiles, b"zed settings")?;
        std::os::unix::fs::symlink(&dotfiles, source.join("settings.json"))?;
        initialize_settings(&source, &destination)?;
        fs::write(destination.join("settings.json"), b"endo settings")?;
        assert_eq!(fs::read(dotfiles)?, b"zed settings");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_settings_linked_to_zed() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("zed");
        let destination = root.path().join("endo");
        fs::create_dir(&source)?;
        fs::write(source.join("settings.json"), b"zed settings")?;
        std::os::unix::fs::symlink(&source, &destination)?;
        assert!(initialize_settings(&source, &destination).is_err());
        fs::remove_file(&destination)?;
        fs::create_dir(&destination)?;
        std::os::unix::fs::symlink(
            source.join("settings.json"),
            destination.join("settings.json"),
        )?;
        assert!(initialize_settings(&source, &destination).is_err());
        assert_eq!(fs::read(source.join("settings.json"))?, b"zed settings");
        Ok(())
    }
}
