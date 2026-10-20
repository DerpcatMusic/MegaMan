//! Single-step namespace operations: create, rename, move and trash with no-replace semantics.
//! Every operation either completes atomically or changes nothing; each returns an inverse.
use crate::{ErrorKind, OperationError, Result};
use rustix::{
    fs::{self, AtFlags, CWD, Mode, OFlags, RenameFlags},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

/// What was done, with enough identity to reverse it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Undo {
    /// Remove an empty directory or file this operation created.
    Remove(PathBuf),
    /// Rename back.
    Rename { from: PathBuf, to: PathBuf },
    /// Restore a trashed item.
    Untrash(Trashed),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trashed {
    pub original: PathBuf,
    pub trash_file: PathBuf,
    pub trash_info: PathBuf,
}

fn err(kind: ErrorKind, phase: &'static str, path: &Path, message: &str) -> OperationError {
    OperationError::new(kind, phase, path, message)
}
fn io(phase: &'static str, path: &Path, e: Errno) -> OperationError {
    let mut error = OperationError::io(phase, path, std::io::Error::from(e));
    if e == Errno::EXIST || e == Errno::NOTEMPTY {
        error.kind = ErrorKind::DestinationConflict;
    }
    error
}
/// A single path component that cannot escape its parent.
pub fn validate_name(name: &OsStr) -> Result<()> {
    let b = name.as_bytes();
    if b.is_empty()
        || b == b"."
        || b == b".."
        || b.contains(&b'/')
        || b.contains(&0)
        || b.len() > 255
    {
        return Err(err(
            ErrorKind::InvalidPath,
            "name",
            Path::new(name),
            "not a valid file name",
        ));
    }
    Ok(())
}
fn parent_and_name(path: &Path) -> Result<(&Path, &OsStr)> {
    match (path.parent(), path.file_name()) {
        (Some(p), Some(n)) if !p.as_os_str().is_empty() => Ok((p, n)),
        _ => Err(err(
            ErrorKind::InvalidPath,
            "path",
            path,
            "path has no parent directory",
        )),
    }
}

pub fn create_dir(parent: &Path, name: &OsStr) -> Result<Undo> {
    validate_name(name)?;
    let path = parent.join(name);
    fs::mkdirat(CWD, &path, Mode::from_raw_mode(0o777)).map_err(|e| io("create-dir", &path, e))?;
    Ok(Undo::Remove(path))
}
pub fn create_file(parent: &Path, name: &OsStr) -> Result<Undo> {
    validate_name(name)?;
    let path = parent.join(name);
    fs::openat(
        CWD,
        &path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o666),
    )
    .map_err(|e| io("create-file", &path, e))?;
    Ok(Undo::Remove(path))
}
/// Rename within the same directory; never replaces an existing entry.
pub fn rename(path: &Path, new_name: &OsStr) -> Result<Undo> {
    validate_name(new_name)?;
    let (parent, old) = parent_and_name(path)?;
    if old == new_name {
        return Err(err(
            ErrorKind::InvalidPath,
            "rename",
            path,
            "name is unchanged",
        ));
    }
    let to = parent.join(new_name);
    no_replace(path, &to, "rename")?;
    Ok(Undo::Rename {
        from: to,
        to: path.to_path_buf(),
    })
}
/// Move into another directory on the same filesystem; cross-device is reported, not emulated.
pub fn move_into(src: &Path, dest_dir: &Path) -> Result<Undo> {
    let (_, name) = parent_and_name(src)?;
    let to = if src.parent() == Some(dest_dir) {
        dest_dir.join(name) // same folder: stay a no-replace error, never a silent copy-rename
    } else {
        unique_in(dest_dir, name)
    };
    if to.starts_with(src) {
        return Err(err(
            ErrorKind::Containment,
            "move",
            src,
            "cannot move a folder into itself",
        ));
    }
    no_replace(src, &to, "move")?;
    Ok(Undo::Rename {
        from: to,
        to: src.to_path_buf(),
    })
}
/// `dir/name`, or "stem (2).ext", "stem (3).ext"… when something already lives there (keep both).
pub fn unique_in(dir: &Path, name: &OsStr) -> PathBuf {
    let first = dir.join(name);
    if first.symlink_metadata().is_err() {
        return first;
    }
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or(name).to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2u32..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| p.symlink_metadata().is_err())
        .unwrap()
}
fn no_replace(from: &Path, to: &Path, phase: &'static str) -> Result<()> {
    fs::renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(|e| io(phase, to, e))
}

fn trash_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| Path::new(v).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")))
        .ok_or_else(|| {
            err(
                ErrorKind::UnsupportedPlatform,
                "trash",
                Path::new(""),
                "no home data directory",
            )
        })?;
    Ok(base.join("Trash"))
}
fn ensure_dir(path: &Path) -> Result<()> {
    match fs::mkdirat(CWD, path, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(io("trash", path, e)),
    }
}
fn percent_encode(path: &Path) -> String {
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
fn now_local_iso() -> String {
    // ponytail: UTC stamp; spec wants local time, upgrade with a tz crate if it matters.
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (s / 86400, s % 86400);
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Move to the trash (freedesktop.org Trash spec): the home trash, or `$topdir/.Trash-$uid`
/// when the item lives on another filesystem.
pub fn trash(path: &Path) -> Result<Undo> {
    match trash_in(path, &trash_root()?) {
        Err(e) if e.raw_os_error == Some(Errno::XDEV.raw_os_error()) => {
            let abs =
                std::path::absolute(path).map_err(|e| OperationError::io("trash", path, e))?;
            let top = mount_top(&abs)?;
            let uid = std::fs::metadata("/proc/self")
                .map_err(|e| OperationError::io("trash", &abs, e))?
                .uid();
            trash_with(&abs, &top.join(format!(".Trash-{uid}")), Some(&top))
        }
        other => other,
    }
}
/// The root directory of the filesystem containing `path`.
fn mount_top(path: &Path) -> Result<PathBuf> {
    let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev());
    let first = path
        .parent()
        .ok_or_else(|| err(ErrorKind::InvalidPath, "trash", path, "path has no parent"))?;
    let want = dev(first).map_err(|e| OperationError::io("trash", first, e))?;
    let mut top = first;
    for ancestor in first.ancestors().skip(1) {
        if dev(ancestor).ok() != Some(want) {
            break;
        }
        top = ancestor;
    }
    Ok(top.to_path_buf())
}
/// Trash into an explicit trash directory (testable; `trash` uses the home trash).
pub fn trash_in(path: &Path, root: &Path) -> Result<Undo> {
    trash_with(path, root, None)
}
/// `relative_to`: topdir trashes record paths relative to their topdir, per the spec.
fn trash_with(path: &Path, root: &Path, relative_to: Option<&Path>) -> Result<Undo> {
    let path = std::path::absolute(path).map_err(|e| OperationError::io("trash", path, e))?;
    let (_, name) = parent_and_name(&path)?;
    if let Some(parent) = root.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (files, info) = (root.join("files"), root.join("info"));
    for d in [root, &files, &info] {
        ensure_dir(d)?;
    }
    for n in 0..1000u32 {
        let mut candidate = OsString::from(name);
        if n > 0 {
            candidate.push(format!(".{n}"));
        }
        let (info_path, file_path) = (
            info.join({
                let mut i = candidate.clone();
                i.push(".trashinfo");
                i
            }),
            files.join(&candidate),
        );
        let fd = match fs::openat(
            CWD,
            &info_path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => fd,
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(io("trash", &info_path, e)),
        };
        let mut file = std::fs::File::from(fd);
        let body = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            percent_encode(
                relative_to
                    .and_then(|t| path.strip_prefix(t).ok())
                    .unwrap_or(&path)
            ),
            now_local_iso()
        );
        let written = file
            .write_all(body.as_bytes())
            .and_then(|()| file.sync_all());
        let moved = written
            .map_err(|e| OperationError::io("trash", &info_path, e))
            .and_then(|()| no_replace(&path, &file_path, "trash"));
        match moved {
            Ok(()) => {
                return Ok(Undo::Untrash(Trashed {
                    original: path,
                    trash_file: file_path,
                    trash_info: info_path,
                }));
            }
            Err(e) => {
                let _ = fs::unlinkat(CWD, &info_path, AtFlags::empty());
                // A name taken in files/ by a stray entry: try the next suffix.
                if e.kind == ErrorKind::DestinationConflict {
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(err(
        ErrorKind::DestinationConflict,
        "trash",
        &path,
        "no free trash name",
    ))
}

/// Reverse an operation. Never replaces; fails if the world changed.
pub fn undo(op: &Undo) -> Result<()> {
    match op {
        Undo::Remove(path) => {
            let md =
                std::fs::symlink_metadata(path).map_err(|e| OperationError::io("undo", path, e))?;
            if md.is_dir() {
                fs::unlinkat(CWD, path, AtFlags::REMOVEDIR).map_err(|e| io("undo", path, e))
            } else if md.len() == 0 {
                fs::unlinkat(CWD, path, AtFlags::empty()).map_err(|e| io("undo", path, e))
            } else {
                Err(err(
                    ErrorKind::SourceChanged,
                    "undo",
                    path,
                    "file now has content; not removed",
                ))
            }
        }
        Undo::Rename { from, to } => no_replace(from, to, "undo"),
        Undo::Untrash(t) => {
            no_replace(&t.trash_file, &t.original, "untrash")?;
            let _ = fs::unlinkat(CWD, &t.trash_info, AtFlags::empty());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fsops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn create_rename_move_undo_never_replace() {
        let d = tmp("basic");
        let u = create_file(&d, OsStr::new("a")).unwrap();
        assert!(create_file(&d, OsStr::new("a")).is_err());
        create_dir(&d, OsStr::new("sub")).unwrap();
        std::fs::write(d.join("b"), b"x").unwrap();
        assert_eq!(
            rename(&d.join("a"), OsStr::new("b")).unwrap_err().kind,
            ErrorKind::DestinationConflict
        );
        let r = rename(&d.join("a"), OsStr::new("c")).unwrap();
        assert!(d.join("c").exists() && !d.join("a").exists());
        undo(&r).unwrap();
        let m = move_into(&d.join("a"), &d.join("sub")).unwrap();
        assert!(d.join("sub/a").exists());
        undo(&m).unwrap();
        assert!(move_into(&d.join("sub"), &d.join("sub")).is_err());
        undo(&u).unwrap();
        assert!(!d.join("a").exists());
        assert!(validate_name(OsStr::new("../x")).is_err());
        assert!(validate_name(OsStr::new("")).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn topdir_trash_records_relative_paths_and_restores() {
        let d = tmp("topdir");
        std::fs::write(d.join("f"), b"1").unwrap();
        let top = d.join(".Trash-test");
        let t = trash_with(&d.join("f"), &top, Some(&d)).unwrap();
        let Undo::Untrash(a) = &t else { panic!() };
        let info = std::fs::read_to_string(&a.trash_info).unwrap();
        assert!(info.contains("Path=f\n"), "{info}");
        assert!(mount_top(&d.join("f")).unwrap().is_absolute());
        undo(&t).unwrap();
        assert!(d.join("f").exists());
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn trash_round_trip_with_collision() {
        let d = tmp("trash");
        let data = d.join("xdg");
        std::fs::write(d.join("f"), b"1").unwrap();
        let t1 = trash_in(&d.join("f"), &data.join("Trash")).unwrap();
        std::fs::write(d.join("f"), b"2").unwrap();
        let t2 = trash_in(&d.join("f"), &data.join("Trash")).unwrap();
        let (Undo::Untrash(a), Undo::Untrash(b)) = (&t1, &t2) else {
            panic!()
        };
        assert_ne!(a.trash_file, b.trash_file);
        let info = std::fs::read_to_string(&a.trash_info).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/") && info.contains("DeletionDate="));
        undo(&t2).unwrap();
        assert_eq!(std::fs::read(d.join("f")).unwrap(), b"2");
        assert!(undo(&t1).is_err(), "restore must not replace");
        std::fs::remove_file(d.join("f")).unwrap();
        undo(&t1).unwrap();
        assert_eq!(std::fs::read(d.join("f")).unwrap(), b"1");
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn unique_names_keep_extensions_and_skip_taken_slots() {
        let d = std::env::temp_dir().join(format!("fsops-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(unique_in(&d, OsStr::new("a.txt")), d.join("a.txt"));
        std::fs::write(d.join("a.txt"), b"").unwrap();
        std::fs::write(d.join("a (2).txt"), b"").unwrap();
        assert_eq!(unique_in(&d, OsStr::new("a.txt")), d.join("a (3).txt"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
