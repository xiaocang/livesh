use std::{fs, path::Path};

use crate::paths::RuntimePaths;

pub fn startup_gc(paths: &RuntimePaths) -> anyhow::Result<()> {
    remove_entries(&paths.states)?;
    remove_entries(&paths.snapshots)?;
    remove_entries(&paths.scrollback)?;
    remove_entries(&paths.events)?;
    remove_entries(&paths.tmp)?;
    Ok(())
}

pub fn remove_shell_files(paths: &RuntimePaths, id: &livesh_protocol::ShellId) {
    let _ = fs::remove_file(paths.metadata(id));
    let _ = fs::remove_file(paths.snapshot(id));
    let _ = fs::remove_file(paths.scrollback(id));
    let _ = fs::remove_file(paths.events(id));
}

pub fn runtime_size(paths: &RuntimePaths) -> u64 {
    dir_size(&paths.base).unwrap_or(0)
}

pub fn remove_entries(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut size = 0;
    if !path.exists() {
        return Ok(size);
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            size += dir_size(&entry.path())?;
        } else {
            size += meta.len();
        }
    }
    Ok(size)
}
