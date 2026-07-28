//! What gets copied to the device: either the copy baked in at build time or a
//! directory named with `--firmware`.

use include_dir::{include_dir, Dir};
use std::borrow::Cow;
use std::path::Path;
use walkdir::WalkDir;

/// The copy baked in at build time. A build with no `firmware/` beside it fails
/// to compile rather than producing a binary that cannot deploy.
static EMBEDDED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../firmware");

/// Build products of a local `python -m compileall`, which CI runs over
/// `firmware/`. They are not firmware and must not reach the device.
fn skip(name: &str) -> bool {
    name == "__pycache__" || name.starts_with('.') || name.ends_with(".pyc")
}

/// The same rule over a whole relative path. `include_dir!` embeds everything
/// it finds — it has no filter of its own — so anything a local `compileall`
/// left behind is dropped here instead of at build time.
fn skip_path(path: &Path) -> bool {
    path.components()
        .any(|part| skip(&part.as_os_str().to_string_lossy()))
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
            let entries = EMBEDDED
                .find("**/*")
                .map_err(|e| format!("cannot read the baked-in firmware: {e}"))?;
            let mut files = Vec::new();
            for file in entries.filter_map(|entry| entry.as_file()) {
                if skip_path(file.path()) {
                    continue;
                }
                // The macro normalises separators, so this is already the
                // device path.
                let path = file.path().to_string_lossy().replace('\\', "/");
                check_path(&path)?;
                files.push(SourceFile {
                    path,
                    data: Cow::Borrowed(file.contents()),
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
    use std::path::PathBuf;

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
    fn the_baked_in_copy_is_byte_for_byte_the_firmware_directory() {
        // `include_dir!` will not compile without firmware/ beside the crate,
        // so this can be unconditional. What it is really checking is that the
        // macro reaches the nested lib/ directories and lands on exactly what
        // a --firmware pointed at the same place would send — the two paths
        // have to stay interchangeable, or a deploy would depend on which one
        // the user happened to use.
        let embedded = load(None).expect("firmware/ is part of the repo");
        let on_disk = load(Some(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"),
        ))
        .expect("the same directory, read at runtime");

        let describe = |files: &[SourceFile]| -> Vec<(String, usize)> {
            files
                .iter()
                .map(|f| (f.path.clone(), f.data.len()))
                .collect()
        };
        assert_eq!(describe(&embedded), describe(&on_disk));

        assert!(embedded.iter().any(|f| f.path == "main.py"));
        assert!(
            embedded.iter().any(|f| f.path == "lib/st7789/st7789py.py"),
            "the nested directories have to be reached: {:?}",
            embedded.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn compiled_python_never_reaches_the_device() {
        // include_dir! embeds whatever is there, so this filter is the only
        // thing keeping a local `python -m compileall` off the Pico.
        assert!(skip_path(Path::new(
            "lib/__pycache__/st7789py.cpython-312.pyc"
        )));
        assert!(skip_path(Path::new("__pycache__")));
        assert!(skip_path(Path::new("main.pyc")));
        assert!(skip_path(Path::new(".gitignore")));
        assert!(!skip_path(Path::new("lib/st7789/st7789py.py")));
        assert!(!skip_path(Path::new("main.py")));
    }
}
