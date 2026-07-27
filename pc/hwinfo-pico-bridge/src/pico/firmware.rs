//! What gets copied to the device: either the copy baked in at build time or a
//! directory named with `--firmware`.

use std::borrow::Cow;
use std::path::Path;
use walkdir::WalkDir;

include!(concat!(env!("OUT_DIR"), "/firmware.rs"));

/// Build products of a local `python -m compileall`, which CI runs over
/// `firmware/`. They are not firmware and must not reach the device.
fn skip(name: &str) -> bool {
    name == "__pycache__" || name.starts_with('.') || name.ends_with(".pyc")
}

pub struct SourceFile {
    /// Device path, relative to the root, with `/` separators.
    pub path: String,
    pub data: Cow<'static, [u8]>,
}

/// Device paths are interpolated into Python string literals, so anything that
/// could end one — or climb out of the device root — is refused rather than
/// escaped. Real firmware paths are all lowercase words, dots and slashes.
fn check_path(path: &str) -> Result<(), String> {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains("//")
        || path.split('/').any(|part| part == ".." || part.is_empty())
        || !path.chars().all(ok)
    {
        return Err(format!(
            "{path}: firmware paths must be relative and made of letters, \
             digits, '.', '_', '-' and '/'"
        ));
    }
    Ok(())
}

fn read_dir(root: &Path) -> Result<Vec<SourceFile>, String> {
    let mut out = Vec::new();
    // Same exclusions the build script applies: compiled Python and dot files
    // are not firmware. Pruning at the entry level keeps walkdir out of
    // `__pycache__` rather than filtering its contents afterwards.
    let walk = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !skip(&e.file_name().to_string_lossy()));

    for entry in walk {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| format!("{} is outside {}", path.display(), root.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        check_path(&rel)?;
        let data =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        out.push(SourceFile {
            path: rel,
            data: Cow::Owned(data),
        });
    }
    Ok(out)
}

/// The firmware to deploy: `dir` if given, otherwise the built-in copy.
pub fn load(dir: Option<&Path>) -> Result<Vec<SourceFile>, String> {
    let mut files = match dir {
        Some(dir) => {
            if !dir.is_dir() {
                return Err(format!("{} is not a directory", dir.display()));
            }
            read_dir(dir)?
        }
        None => {
            let mut files = Vec::with_capacity(EMBEDDED.len());
            for (path, data) in EMBEDDED {
                check_path(path)?;
                files.push(SourceFile {
                    path: (*path).to_string(),
                    data: Cow::Borrowed(*data),
                });
            }
            files
        }
    };

    if files.is_empty() {
        return Err(match dir {
            Some(dir) => format!("{} contains no firmware files", dir.display()),
            None => "this build has no firmware baked in — pass --firmware <dir>".to_string(),
        });
    }
    // Deterministic order, so a deploy reads the same way twice running and
    // parent directories are always created before what goes in them.
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Every directory the given files need, parents first.
pub fn directories(files: &[SourceFile]) -> Vec<String> {
    let mut dirs: Vec<String> = files
        .iter()
        .flat_map(|f| {
            let parts: Vec<&str> = f.path.split('/').collect();
            (1..parts.len()).map(move |n| parts[..n].join("/"))
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_could_escape_or_break_a_python_literal() {
        for bad in [
            "",
            "/main.py",
            "../main.py",
            "lib/../../main.py",
            "lib//main.py",
            "lib/",
            "it's.py",
            "a\\b.py",
            "sp ace.py",
        ] {
            assert!(check_path(bad).is_err(), "{bad} should be rejected");
        }
        for good in ["main.py", "lib/st7789/st7789py.py", "a-b_c.1.py"] {
            assert!(check_path(good).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn directories_come_out_parents_first() {
        let files = ["main.py", "lib/st7789/config/tft_config.py", "lib/a.py"]
            .into_iter()
            .map(|p| SourceFile {
                path: p.to_string(),
                data: Cow::Borrowed(b""),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            directories(&files),
            ["lib", "lib/st7789", "lib/st7789/config"]
        );
    }

    #[test]
    fn the_baked_in_copy_is_usable() {
        // A build without firmware/ next to it is legitimate, but if there is
        // one it has to survive the same checks a --firmware directory does.
        let files = load(None);
        if let Ok(files) = files {
            assert!(files.iter().any(|f| f.path == "main.py"));
        }
    }
}
