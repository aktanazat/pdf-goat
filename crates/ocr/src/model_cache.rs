//! The recognizer models macOS compiles for this program, behind
//! [`crate::repair_model_cache`] and [`crate::damaged_models`].
//!
//! The first time a program runs Vision's text recognizer, macOS compiles
//! the recognizer's models for the Neural Engine into
//! `~/Library/Caches/<program>/com.apple.e5rt.e5bundlecache`, one folder per
//! model inside one per system build, `<program>` being the name of the file
//! the program was started from. A model's descriptor, an `.e5` file, names
//! each input width the model is compiled for, `main_128` to `main_2816`,
//! and the function compiled for it, `main_ane_128` to `main_ane_2816`. The
//! compile sometimes writes a function's name a digit short, `main_ane_256`
//! where `main_ane_2560` belongs: that name then appears twice, the width
//! has no function, and Vision skips or misreads lines of that width on
//! every run until the model is compiled again. macOS compiles a model
//! again when its folder is gone.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::autoreleasepool;
use objc2_foundation::{
    NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains,
    NSTemporaryDirectory,
};

use crate::ModelRepair;

/// Folder, in a program's cache folder, macOS compiles the models into.
const MODEL_CACHE: &str = "com.apple.e5rt.e5bundlecache";
/// Start of the name of a width a model is compiled for; the width follows.
const WIDTH: &[u8] = b"main_";
/// Start of the name of a width's compiled function; the width follows.
const FUNCTION: &[u8] = b"main_ane_";
/// Folders deep below a model's folder that a descriptor is looked for.
const MAX_DEPTH: usize = 4;
/// Entries one check looks at, over the whole cache.
const MAX_ENTRIES: usize = 4096;
/// Bytes read of one descriptor; one runs to tens of kilobytes.
const MAX_DESCRIPTOR: u64 = 1 << 20;

/// [`repair`] on this program's own cache, into the temporary folder.
pub(crate) fn repair_own() -> ModelRepair {
    let Some(cache) = own_cache() else {
        return ModelRepair::default();
    };
    let temporary = autoreleasepool(|_| PathBuf::from(NSTemporaryDirectory().to_string()));
    let program = cache.parent().map(name).unwrap_or_default();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let tag = format!(
        "{program}-damaged-recognizer-{}-{stamp}",
        std::process::id()
    );
    repair(&cache, &temporary, &tag)
}

/// [`damaged`] on this program's own cache.
pub(crate) fn damaged_own() -> Vec<PathBuf> {
    own_cache().map(|cache| damaged(&cache)).unwrap_or_default()
}

/// `<caches>/<program>/com.apple.e5rt.e5bundlecache`, `<program>` being the
/// name of the file this process was started from, symbolic link or not,
/// as macOS names the cache.
fn own_cache() -> Option<PathBuf> {
    let program = std::env::current_exe().ok()?.file_name()?.to_owned();
    let caches = autoreleasepool(|_| {
        NSSearchPathForDirectoriesInDomains(
            NSSearchPathDirectory::CachesDirectory,
            NSSearchPathDomainMask::UserDomainMask,
            true,
        )
        .firstObject()
        .map(|path| PathBuf::from(path.to_string()))
    })?;
    Some(caches.join(program).join(MODEL_CACHE))
}

/// Moves each model folder of `cache` that [`damaged`] finds into `aside`,
/// named `<tag>-<build>-<model>`. Nothing else in `cache`, and nothing
/// outside it, is touched.
fn repair(cache: &Path, aside: &Path, tag: &str) -> ModelRepair {
    let mut repair = ModelRepair::default();
    for model in damaged(cache) {
        let build = model.parent().map(name).unwrap_or_default();
        let to = aside.join(format!("{tag}-{build}-{}", name(&model)));
        match fs::rename(&model, &to) {
            Ok(()) => repair.moved.push((model, to)),
            Err(error) => repair.stuck.push((model, error.to_string())),
        }
    }
    repair
}

/// The model folders of `cache`, `<build>/<model>`, holding a descriptor
/// that lost a function.
fn damaged(cache: &Path) -> Vec<PathBuf> {
    let mut budget = MAX_ENTRIES;
    let mut found = Vec::new();
    for build in folders(cache, &mut budget) {
        for model in folders(&build, &mut budget) {
            if holds_damaged_descriptor(&model, &mut budget) {
                found.push(model);
            }
        }
    }
    found
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The folders directly in `dir`, symbolic links left out, each entry
/// looked at spending one of `budget`.
fn folders(dir: &Path, budget: &mut usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            found.push(entry.path());
        }
    }
    found
}

/// Whether a descriptor at most [`MAX_DEPTH`] folders below `model` lost a
/// function. Symbolic links are not followed.
fn holds_damaged_descriptor(model: &Path, budget: &mut usize) -> bool {
    let mut pending = vec![(model.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < MAX_DEPTH {
                pending.push((path, depth + 1));
            } else if kind.is_file()
                && path.extension().is_some_and(|extension| extension == "e5")
                && descriptor_damaged(&path)
            {
                return true;
            }
        }
    }
    false
}

fn descriptor_damaged(path: &Path) -> bool {
    let mut descriptor = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_DESCRIPTOR).read_to_end(&mut descriptor))
        .is_ok()
        && lost_a_function(&descriptor)
}

/// Whether `descriptor`, read as runs of printable bytes, names one
/// function twice while a width it is compiled for has no function: a
/// function's name lost a digit and became another's.
fn lost_a_function(descriptor: &[u8]) -> bool {
    let mut functions = HashSet::new();
    let mut named_twice = false;
    let mut widths = HashSet::new();
    for run in descriptor.split(|&byte| !(byte == b'\t' || (b' '..=b'~').contains(&byte))) {
        if let Some(width) = number_after(run, FUNCTION) {
            named_twice |= !functions.insert(width);
        } else if let Some(width) = number_after(run, WIDTH) {
            widths.insert(width);
        }
    }
    named_twice && widths.iter().any(|width| !functions.contains(width))
}

/// The digits after `prefix` when `run` is `prefix` and nothing but digits.
fn number_after<'a>(run: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    run.strip_prefix(prefix)
        .filter(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widths a recognizer model is compiled for.
    const WIDTHS: [u32; 22] = [
        128, 192, 256, 320, 384, 448, 512, 576, 640, 704, 768, 832, 896, 960, 1024, 1280, 1536,
        1792, 2048, 2304, 2560, 2816,
    ];

    /// A descriptor's names as macOS writes them, each a 4-byte length, its
    /// bytes and a zero, padded to 4 bytes: one for each width the model is
    /// compiled for, then `functions`.
    fn descriptor(functions: &[String]) -> Vec<u8> {
        let widths = WIDTHS.map(|width| format!("main_{width}"));
        let mut bytes = vec![0u8; 16];
        for name in widths.iter().chain(functions) {
            let length = u32::try_from(name.len()).expect("a short name");
            bytes.extend(length.to_le_bytes());
            bytes.extend(name.as_bytes());
            bytes.push(0);
            bytes.resize(bytes.len().next_multiple_of(4), 0);
        }
        bytes
    }

    /// A function for each width.
    fn sound() -> Vec<String> {
        WIDTHS
            .iter()
            .map(|width| format!("main_ane_{width}"))
            .collect()
    }

    /// The function for width 2560 named a digit short, as macOS wrote it.
    fn damaged() -> Vec<String> {
        sound()
            .into_iter()
            .map(|name| {
                if name == "main_ane_2560" {
                    "main_ane_256".to_owned()
                } else {
                    name
                }
            })
            .collect()
    }

    /// A program's model cache holding `models`, each with one descriptor,
    /// as macOS lays it out; beside the cache, a file of the program's own.
    fn program_cache(root: &Path, models: &[(&str, Vec<u8>)]) -> PathBuf {
        let cache = root.join("program").join(MODEL_CACHE);
        for (model, bytes) in models {
            let bundle = cache
                .join("26A428")
                .join(model)
                .join("D5D9.bundle")
                .join("H16S.bundle");
            fs::create_dir_all(&bundle).expect("model folder");
            fs::write(bundle.join("H16S.e5"), bytes).expect("descriptor");
        }
        fs::write(root.join("program").join("settings.plist"), b"kept").expect("own file");
        cache
    }

    #[test]
    fn a_descriptor_is_damaged_when_a_function_name_lost_a_digit() {
        assert!(!lost_a_function(&descriptor(&sound())));
        assert!(lost_a_function(&descriptor(&damaged())));
        let mut repeated = sound();
        repeated.push("main_ane_256".to_owned());
        assert!(
            !lost_a_function(&descriptor(&repeated)),
            "a name written twice while every width keeps its function is no damage"
        );
    }

    #[test]
    fn only_a_damaged_model_is_moved_out_of_the_cache() {
        let root = tempfile::tempdir().expect("temp dir");
        let cache = program_cache(
            root.path(),
            &[
                ("572C", descriptor(&damaged())),
                ("131707B5", descriptor(&sound())),
            ],
        );
        let aside = root.path().join("aside");
        fs::create_dir(&aside).expect("aside folder");

        let report = repair(&cache, &aside, "run");
        let moved = aside.join("run-26A428-572C");
        assert_eq!(
            report,
            ModelRepair {
                moved: vec![(cache.join("26A428").join("572C"), moved.clone())],
                stuck: Vec::new(),
            }
        );
        assert!(
            moved.join("D5D9.bundle/H16S.bundle/H16S.e5").is_file(),
            "the damaged model is set aside whole"
        );
        assert!(
            !cache.join("26A428").join("572C").exists()
                && cache
                    .join("26A428/131707B5/D5D9.bundle/H16S.bundle/H16S.e5")
                    .is_file()
                && root.path().join("program/settings.plist").is_file(),
            "the sound model and the program's own file stay"
        );
        assert_eq!(
            repair(&cache, &aside, "next"),
            ModelRepair::default(),
            "nothing is left to repair"
        );
    }

    #[test]
    fn a_damaged_model_that_cannot_be_moved_is_reported_and_kept() {
        let root = tempfile::tempdir().expect("temp dir");
        let cache = program_cache(root.path(), &[("572C", descriptor(&damaged()))]);

        let report = repair(&cache, &root.path().join("missing"), "run");
        let model = cache.join("26A428").join("572C");
        assert!(report.moved.is_empty(), "{report:?}");
        assert_eq!(
            report
                .stuck
                .iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            [&model]
        );
        assert!(model.is_dir(), "the model stays where it was");
    }
}
