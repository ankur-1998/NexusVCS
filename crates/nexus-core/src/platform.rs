//! Everything that differs between operating systems lives here (spec §1, rule 5).

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Where the global config file lives: `%APPDATA%\nexus\config.toml` on Windows,
/// `~/Library/Application Support/nexus/config.toml` on macOS, and
/// `$XDG_CONFIG_HOME/nexus/config.toml` (default `~/.config`) elsewhere.
pub fn global_config_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("nexus").join("config.toml"))
}

#[cfg(windows)]
fn config_dir() -> Option<PathBuf> {
    absolute_env("APPDATA")
}

#[cfg(target_os = "macos")]
fn config_dir() -> Option<PathBuf> {
    absolute_env("HOME").map(|home| home.join("Library").join("Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn config_dir() -> Option<PathBuf> {
    absolute_env("XDG_CONFIG_HOME")
        .or_else(|| absolute_env("HOME").map(|home| home.join(".config")))
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Whether the file is executable, or `None` where files have no executable
/// bit (Windows). There, the index keeps whatever kind it already recorded.
#[cfg(unix)]
pub fn executable_bit(meta: &Metadata) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub fn executable_bit(_meta: &Metadata) -> Option<bool> {
    None
}

/// Whether this OS's filesystems have an executable bit at all.
pub fn executable_bits_exist() -> bool {
    cfg!(unix)
}

/// Whether files in `dir` keep the executable bit they're given. Some Unix
/// filesystems report every file as executable and ignore `chmod` (FAT and
/// exFAT, or Windows drives mounted in WSL); there the bit means nothing.
/// Always true on Windows, which has no executable bit to probe.
#[cfg(unix)]
pub fn exec_bit_is_reliable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(probe) = tempfile::Builder::new()
        .prefix(".tmp-mode-")
        .tempfile_in(dir)
    else {
        return true;
    };
    let mode_after = |mode: u32| {
        std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(mode))
            .and_then(|()| std::fs::metadata(probe.path()))
            .map(|meta| meta.permissions().mode() & 0o111)
            .ok()
    };
    mode_after(0o755) == Some(0o111) && mode_after(0o644) == Some(0)
}

#[cfg(not(unix))]
pub fn exec_bit_is_reliable(_dir: &Path) -> bool {
    true
}

/// Sets or clears the executable bits of a file nexus just created, the way
/// Git does: the file was created with the user's umask applied, and an
/// executable gets an execute bit wherever it has a read bit. Does nothing
/// on Windows.
#[cfg(unix)]
pub fn set_file_mode(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
    let wanted = if executable {
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    if wanted == mode {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted))
}

#[cfg(not(unix))]
pub fn set_file_mode(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

/// Adds the executable bit wherever the read bit is set, like `chmod +x`.
/// Does nothing on Windows.
#[cfg(unix)]
pub fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    permissions.set_mode(mode | ((mode & 0o444) >> 2));
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
pub fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// The modification time in nanoseconds since the Unix epoch, if the
/// filesystem reports one.
pub fn mtime_ns(meta: &Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).ok(),
        Err(before) => i64::try_from(before.duration().as_nanos())
            .ok()
            .map(|nanos| -nanos),
    }
}

/// Whether a failed rename hit the brief lock that antivirus scanners and
/// search indexers take on Windows, so retrying shortly will likely succeed.
#[cfg(windows)]
pub fn is_transient_lock(err: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
    matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(not(windows))]
pub fn is_transient_lock(_err: &io::Error) -> bool {
    false
}

/// Whether the filesystem holding the repository at `root` treats names that
/// differ only in letter case as the same (the Windows and macOS defaults).
/// Probed by looking up its `.nexus` directory as `.NEXUS`.
pub fn ignores_case(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".NEXUS")).is_ok()
}

/// Clears a file's read-only attribute, returning whether it was set. Only
/// Windows refuses to replace a read-only file (Unix asks the directory).
#[cfg(windows)]
pub fn clear_read_only(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let mut permissions = meta.permissions();
    if !permissions.readonly() {
        return false;
    }
    // On Windows this clears one attribute; it doesn't widen access the way
    // the lint warns about on Unix.
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).is_ok()
}

#[cfg(not(windows))]
pub fn clear_read_only(_path: &Path) -> bool {
    false
}
