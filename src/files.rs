use std::{
    fs,
    io,
    path::{Component, Path, PathBuf},
};

use crate::state::get_project_root;

pub fn storage_root() -> PathBuf {
    get_project_root().join("storage")
}

pub fn uploads_root() -> PathBuf {
    storage_root().join("uploads")
}

pub fn ensure_storage() -> io::Result<()> {
    fs::create_dir_all(uploads_root())
}

pub fn upload_path(relative_path: impl AsRef<Path>) -> Option<PathBuf> {
    let relative = relative_path.as_ref();

    if relative.is_absolute() {
        return None;
    }

    if relative.components().any(|component| {
        matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    }) {
        return None;
    }

    Some(uploads_root().join(relative))
}

pub fn exists(relative_path: impl AsRef<Path>) -> bool {
    upload_path(relative_path)
        .map(|path| path.is_file())
        .unwrap_or(false)
}

pub fn save(relative_path: impl AsRef<Path>, data: &[u8]) -> io::Result<PathBuf> {
    let path = upload_path(relative_path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid upload path"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&path, data)?;

    Ok(path)
}


pub fn delete(relative_path: impl AsRef<Path>) -> io::Result<()> {
    let path = upload_path(relative_path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid upload path"))?;

    if path.exists() {
        fs::remove_file(path)?;
    }

    Ok(())
}
