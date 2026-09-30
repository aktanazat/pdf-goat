//! Path expansion, canonicalization, output naming, and atomic output files.
//! Expansion and filename splitting retain pathlib semantics.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::error::GoatError;

/// `resolve(path)`: the absolute, symlink-free path of an existing file, or
/// `file not found: {path}` naming the argument as given.
pub fn resolve(path: &str) -> Result<PathBuf, GoatError> {
    let expanded = expanduser(path);
    if !exists(&expanded)? {
        return Err(GoatError::message(format!("file not found: {path}")));
    }
    resolve_lenient(&expanded)
}

/// pathlib `Path.exists()`: `false` for the errors Python treats as absence, the error for
/// any other failure to stat.
pub fn exists(path: &Path) -> Result<bool, GoatError> {
    // `ErrorKind::FilesystemLoop` is unstable, so ELOOP is matched by number.
    const EBADF: i32 = 9;
    const ELOOP: i32 = if cfg!(target_os = "linux") { 40 } else { 62 };
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) || matches!(error.raw_os_error(), Some(EBADF | ELOOP)) =>
        {
            Ok(false)
        }
        Err(error) => Err(GoatError::os(&error, path)),
    }
}

/// pathlib `Path(path).expanduser()`: a leading `~` or `~/` becomes the home directory.
///
/// `~user` stays as written; Python looks the user up instead.
pub fn expanduser(path: &str) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return PathBuf::from(normalize(path)),
    };
    let Some(home) = std::env::home_dir() else {
        return PathBuf::from(normalize(path));
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    // `os.path.expanduser` falls back to the root when the home directory is empty.
    let joined = format!("{home}{rest}");
    PathBuf::from(normalize(if joined.is_empty() { "/" } else { &joined }))
}

/// pathlib `Path(path).resolve()`: absolute, with every symlink that exists resolved,
/// components that do not exist kept as written, and `..` applied after resolution.
pub fn resolve_lenient(path: &Path) -> Result<PathBuf, GoatError> {
    let mut seen = HashMap::new();
    let (resolved, _) = join_realpath(Vec::new(), path.as_os_str().as_bytes(), &mut seen)?;
    let absolute = if resolved.starts_with(b"/") {
        resolved
    } else {
        let cwd = std::env::current_dir().map_err(|error| GoatError::os_unnamed(&error))?;
        let mut joined = cwd.into_os_string().into_vec();
        if !resolved.is_empty() {
            if !joined.ends_with(b"/") {
                joined.push(b'/');
            }
            joined.extend_from_slice(&resolved);
        }
        joined
    };
    Ok(PathBuf::from(OsString::from_vec(normpath(&absolute))))
}

type Seen = HashMap<Vec<u8>, Option<Vec<u8>>>;

/// posixpath `_joinrealpath(path, rest, strict=False, seen)`.
fn join_realpath(
    mut path: Vec<u8>,
    rest: &[u8],
    seen: &mut Seen,
) -> Result<(Vec<u8>, bool), GoatError> {
    let mut rest = rest;
    if let Some(relative) = rest.strip_prefix(b"/") {
        rest = relative;
        path = b"/".to_vec();
    }
    while !rest.is_empty() {
        let (name, remainder) = match rest.iter().position(|byte| *byte == b'/') {
            Some(slash) => (&rest[..slash], &rest[slash + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = remainder;
        if name.is_empty() || name == b"." {
            continue;
        }
        if name == b".." {
            if path.is_empty() {
                path = b"..".to_vec();
            } else {
                let (head, last) = posix_split(&path);
                path = if last == b".." {
                    join(&join(&head, b".."), b"..")
                } else {
                    head
                };
            }
            continue;
        }
        let candidate = join(&path, name);
        let candidate_path = Path::new(std::ffi::OsStr::from_bytes(&candidate));
        let is_link = fs::symlink_metadata(candidate_path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        if !is_link {
            path = candidate;
            continue;
        }
        match seen.get(&candidate) {
            Some(Some(resolved)) => {
                path = resolved.clone();
                continue;
            }
            // The link is being resolved further up: a loop, returned unresolved.
            Some(None) => return Ok((join(&candidate, rest), false)),
            None => {}
        }
        seen.insert(candidate.clone(), None);
        let target =
            fs::read_link(candidate_path).map_err(|error| GoatError::os(&error, candidate_path))?;
        let (resolved, complete) = join_realpath(path, target.as_os_str().as_bytes(), seen)?;
        if !complete {
            return Ok((join(&resolved, rest), false));
        }
        seen.insert(candidate, Some(resolved.clone()));
        path = resolved;
    }
    Ok((path, true))
}

/// posixpath `join(a, b)` for a relative `b`.
fn join(base: &[u8], name: &[u8]) -> Vec<u8> {
    if name.starts_with(b"/") {
        return name.to_vec();
    }
    let mut joined = base.to_vec();
    if !joined.is_empty() && !joined.ends_with(b"/") && !name.is_empty() {
        joined.push(b'/');
    }
    joined.extend_from_slice(name);
    joined
}

/// posixpath `split(p)`.
fn posix_split(path: &[u8]) -> (Vec<u8>, &[u8]) {
    let cut = path
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(0, |slash| slash + 1);
    let (head, tail) = path.split_at(cut);
    let mut head = head.to_vec();
    if !head.is_empty() && head.iter().any(|byte| *byte != b'/') {
        while head.ends_with(b"/") {
            head.pop();
        }
    }
    (head, tail)
}

/// posixpath `normpath(p)`.
fn normpath(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let leading = if path.starts_with(b"//") && !path.starts_with(b"///") {
        2
    } else {
        usize::from(path.starts_with(b"/"))
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|byte| *byte == b'/') {
        if part.is_empty() || part == b"." {
            continue;
        }
        let keep = part != b".."
            || (leading == 0 && parts.is_empty())
            || parts.last().is_some_and(|last| *last == b"..");
        if keep {
            parts.push(part);
        } else {
            parts.pop();
        }
    }
    let mut normal = vec![b'/'; leading];
    normal.extend_from_slice(&parts.join(&b'/'));
    if normal.is_empty() {
        normal.push(b'.');
    }
    normal
}

/// pathlib `str(PurePosixPath(path))`: repeated slashes and `.` components dropped, and a
/// lone `.` for an empty path.
pub fn normalize(path: &str) -> String {
    let (root, parts) = split_root(path);
    let joined = format!("{root}{}", parts.join("/"));
    if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// pathlib's anchor and parts of a POSIX path.
fn split_root(path: &str) -> (&str, Vec<&str>) {
    let root = if !path.starts_with('/') {
        ""
    } else if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else {
        "/"
    };
    let parts = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    (root, parts)
}

/// pathlib `PurePath(path).name`: the last component, or `""`.
pub fn name(path: &str) -> &str {
    path.split('/')
        .rev()
        .find(|part| !part.is_empty() && *part != ".")
        .unwrap_or("")
}

/// pathlib `PurePath(path).stem` (Python 3.12): the name without its last suffix.
pub fn stem(path: &str) -> &str {
    let name = name(path);
    match suffix_start(name) {
        Some(dot) => &name[..dot],
        None => name,
    }
}

/// pathlib `PurePath(path).suffix` (Python 3.12): the last `.ext` of the name, or `""`.
pub fn suffix(path: &str) -> &str {
    let name = name(path);
    suffix_start(name).map_or("", |dot| &name[dot..])
}

fn suffix_start(name: &str) -> Option<usize> {
    name.rfind('.')
        .filter(|dot| *dot > 0 && *dot + 1 < name.len())
}

/// `default_out(src, suffix, ext)`: `{parent}/{stem}.{suffix}.{ext}` beside the source.
pub fn default_out(source: &str, suffix: &str, ext: &str) -> Result<String, GoatError> {
    let (root, mut parts) = split_root(source);
    let Some(last) = parts.last_mut() else {
        return Err(GoatError::value_error(format!(
            "PosixPath({}) has an empty name",
            crate::py::repr_str(&normalize(source))
        )));
    };
    let renamed = format!("{}.{suffix}.{ext}", stem(last));
    *last = &renamed;
    Ok(format!("{root}{}", parts.join("/")))
}

/// `ensure_parent(path)`: the resolved output path, with its parent directory created.
pub fn ensure_parent(path: &str) -> Result<PathBuf, GoatError> {
    let resolved = resolve_lenient(&expanduser(path))?;
    if let Some(parent) = resolved.parent() {
        mkdir_parents(parent)?;
    }
    Ok(resolved)
}

/// `out_dir(a, src, suffix)`: the requested directory, or `{stem}_{suffix}` in the working
/// directory, resolved and created.
pub fn out_dir(requested: Option<&str>, source: &str, suffix: &str) -> Result<PathBuf, GoatError> {
    let directory = match requested.filter(|requested| !requested.is_empty()) {
        Some(requested) => requested.to_owned(),
        None => format!("{}_{suffix}", stem(source)),
    };
    let resolved = resolve_lenient(&expanduser(&directory))?;
    mkdir_parents(&resolved)?;
    Ok(resolved)
}

/// pathlib `Path(path).mkdir(parents=True, exist_ok=True)`, naming the directory whose
/// creation failed. An empty path is pathlib's `.`.
pub fn mkdir_parents(path: &Path) -> Result<(), GoatError> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match path.parent() {
                Some(parent) if parent != path && !parent.as_os_str().is_empty() => {
                    mkdir_parents(parent)?;
                }
                _ => return Err(GoatError::os(&error, path)),
            }
            match fs::create_dir(path) {
                Ok(()) => Ok(()),
                Err(_) if path.is_dir() => Ok(()),
                Err(error) => Err(GoatError::os(&error, path)),
            }
        }
        Err(_) if path.is_dir() => Ok(()),
        Err(error) => Err(GoatError::os(&error, path)),
    }
}

/// `AtomicOutput`: write the sibling `{path}.part`, then [`commit`](Self::commit) renames it
/// over `path`. The partial file is removed whether or not the write succeeded.
#[derive(Debug)]
pub struct AtomicOutput {
    path: PathBuf,
    partial: PathBuf,
}

impl AtomicOutput {
    /// Plans a write of `path` through `{path}.part`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut partial = path.clone().into_os_string();
        partial.push(".part");
        Self {
            path,
            partial: PathBuf::from(partial),
        }
    }

    /// The file to write.
    pub fn partial(&self) -> &Path {
        &self.partial
    }

    /// The file the partial becomes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Renames the finished partial over the output.
    pub fn commit(self) -> Result<(), GoatError> {
        fs::rename(&self.partial, &self.path)
            .map_err(|error| GoatError::os_pair(&error, &self.partial, &self.path))
    }
}

impl Drop for AtomicOutput {
    fn drop(&mut self) {
        // `unlink(missing_ok=True)`: after a commit there is nothing left to remove, and a
        // failed cleanup must not hide the verb's own result.
        let _ = fs::remove_file(&self.partial);
    }
}
