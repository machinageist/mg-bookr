use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use rusqlite::OpenFlags;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

#[cfg(unix)]
static OWNER_PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

// Linux exposes O_NOFOLLOW through libc; keep the value named and scoped to the supported target
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400000;

// Prepare the database leaf and its owned private directory before SQLite opens either one
pub(crate) fn prepare_database_path(path: &Path) -> io::Result<()> {
    let parent = ensure_private_database_parent(path)?;
    ensure_private_file(path, true)?;
    ensure_database_sidecars_in(&parent, path)
}

// Recheck SQLite's database and auxiliary files after a journal-mode change creates them
pub(crate) fn ensure_database_sidecars(path: &Path) -> io::Result<()> {
    let parent = ensure_private_database_parent(path)?;
    ensure_private_file(path, true)?;
    ensure_database_sidecars_in(&parent, path)
}

// Open SQLite with its Unix no-follow protection when the platform provides it
pub(crate) fn open_database(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    #[cfg(unix)]
    {
        rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
    }
    #[cfg(not(unix))]
    {
        rusqlite::Connection::open(path)
    }
}

// Walk each parent component without accepting symlinks, creating missing directories privately.
fn ensure_private_database_parent(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid_path("database path must be absolute"));
    }
    if path.file_name().filter(|name| !name.is_empty()).is_none() {
        return Err(invalid_path("database path must name a file"));
    }
    let parent = path
        .parent()
        .filter(|parent| parent.is_absolute())
        .ok_or_else(|| invalid_path("database path must have an absolute parent"))?;
    let components = parent
        .components()
        .map(|component| match component {
            Component::RootDir => Ok(None),
            Component::Normal(name) => Ok(Some(name.to_os_string())),
            _ => Err(invalid_path(
                "database path contains a non-normal component",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut current = PathBuf::from("/");
    let components = components.into_iter().flatten().collect::<Vec<_>>();
    for component in &components {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            Ok(_) => return Err(insecure_path("database directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_directory(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    validate_private_directory(&current)?;
    Ok(current)
}

// Create only the final application-owned directory with private permissions
fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        builder.mode(PRIVATE_DIRECTORY_MODE);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_private_directory(path)
}

// Require an owned private directory and never chmod an arbitrary existing parent
fn validate_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(target_os = "linux")]
        options.custom_flags(O_NOFOLLOW);
        let directory = options.open(path)?;
        let metadata = directory.metadata()?;

        if !metadata.is_dir() || metadata.uid() != effective_uid(path)? {
            return Err(insecure_path("database directory"));
        }
        if metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE {
            directory.set_permissions(fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(insecure_path("database directory"));
        }
        Ok(())
    }
}

// Open the database leaf without following a symlink and repair only that held file descriptor
fn ensure_private_file(path: &Path, create: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(target_os = "linux")]
        options.custom_flags(O_NOFOLLOW);
        if create {
            options.create_new(true).mode(PRIVATE_FILE_MODE);
        }
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if create && error.kind() == io::ErrorKind::AlreadyExists => {
                return ensure_private_file(path, false);
            }
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid()
                != effective_uid(path.parent().expect("database file has validated parent"))?
        {
            return Err(insecure_path("database file"));
        }
        if metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE {
            file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => Ok(()),
            Ok(_) => Err(insecure_path("database file")),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                Ok(())
            }
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

// Check existing SQLite sidecars in the same owned directory
fn ensure_database_sidecars_in(parent: &Path, path: &Path) -> io::Result<()> {
    for suffix in SIDECAR_SUFFIXES {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if sidecar.parent() != Some(parent) {
            return Err(invalid_path("database sidecar escaped its parent"));
        }
        ensure_private_file(&sidecar, false)?;
    }
    Ok(())
}

// Derive the process euid by creating and immediately removing a private probe in the held directory.
// This uses only portable Unix std APIs and avoids a dependency or Linux-only /proc access.
#[cfg(unix)]
fn effective_uid(directory: &Path) -> io::Result<u32> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    for _ in 0..8 {
        let nonce = OWNER_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let probe_path = directory.join(format!(
            ".mg-bookr-owner-probe-{}-{nonce}",
            std::process::id()
        ));
        let probe = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&probe_path)
        {
            Ok(probe) => probe,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let uid = probe.metadata().map(|metadata| metadata.uid());
        drop(probe);
        let cleanup = fs::remove_file(&probe_path);
        return match (uid, cleanup) {
            (Ok(uid), Ok(())) => Ok(uid),
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate owner probe",
    ))
}

fn invalid_path(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn insecure_path(kind: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("insecure {kind}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::set_permissions(
            directory.path(),
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .expect("private temporary directory");
        directory
    }

    #[test]
    fn bare_relative_database_paths_are_rejected() {
        let error = prepare_database_path(Path::new("database.sqlite")).expect_err("relative path");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn intermediate_symlinked_directory_is_rejected() {
        let directory = private_tempdir();
        let target = directory.path().join("target");
        let alias = directory.path().join("alias");
        fs::create_dir(&target).expect("target directory");
        symlink(&target, &alias).expect("symlink");
        assert!(prepare_database_path(&alias.join("database.sqlite")).is_err());
    }

    #[test]
    fn missing_ancestor_directories_are_created_for_first_run() {
        let directory = private_tempdir();
        let database = directory
            .path()
            .join("xdg")
            .join("data")
            .join("mg-bookr")
            .join("catalog.sqlite");
        prepare_database_path(&database).expect("first-run database hierarchy");
        assert!(database.is_file());
        assert_eq!(
            fs::metadata(database.parent().expect("database parent"))
                .expect("database parent metadata")
                .permissions()
                .mode()
                & 0o777,
            PRIVATE_DIRECTORY_MODE
        );
    }

    #[test]
    fn existing_database_parent_is_repaired_on_its_held_descriptor() {
        let directory = private_tempdir();
        let shared = directory.path().join("shared");
        fs::create_dir(&shared).expect("shared directory");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("shared mode");
        prepare_database_path(&shared.join("database.sqlite")).expect("repair database parent");
        assert_eq!(
            fs::metadata(&shared)
                .expect("shared metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
