//! The record of what was last deployed, kept on the device itself.
//!
//! Without it every update means pushing all of `firmware/` — about 165 KB,
//! most of it font tables that change roughly never — down a REPL that can only
//! take a kilobyte per round trip. With it, changing `main.py` uploads
//! `main.py`.
//!
//! It lives on the Pico rather than on the PC because the device is the thing
//! whose contents are in question: any PC can then update any Pico correctly,
//! and reinstalling Windows does not cause a spurious full deploy.
//!
//! One line per file, so parsing it needs nothing but `split_whitespace` — a
//! JSON dependency for ten lines of `hash size path` would be silly, and the
//! format stays readable if anyone cats it on the device.

use std::collections::BTreeMap;

/// Device path of the manifest. The leading dot keeps it out of the way; it is
/// still perfectly visible to `--deploy-list`.
pub const PATH: &str = ".pico-deploy";

const HEADER: &str = "# hwinfo-pico-bridge deploy manifest v1";

#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    pub sha256: String,
    pub size: u64,
}

#[derive(Default)]
pub struct Manifest {
    files: BTreeMap<String, Entry>,
}

impl Manifest {
    /// Junk is dropped rather than refused: a corrupt or half-written manifest
    /// should cost an unnecessary upload, not a failed deploy.
    pub fn parse(text: &str) -> Manifest {
        let mut files = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Path last, so it is whatever remains — it may not contain spaces
            // today, but this way the format does not care.
            let mut parts = line.splitn(3, char::is_whitespace);
            let (Some(sha256), Some(size), Some(path)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let path = path.trim();
            let (Ok(size), false) = (size.parse::<u64>(), path.is_empty()) else {
                continue;
            };
            files.insert(
                path.to_string(),
                Entry {
                    sha256: sha256.to_string(),
                    size,
                },
            );
        }
        Manifest { files }
    }

    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        out.push('\n');
        for (path, entry) in &self.files {
            out.push_str(&format!("{} {} {}\n", entry.sha256, entry.size, path));
        }
        out
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.files.get(path)
    }

    pub fn insert(&mut self, path: &str, entry: Entry) {
        self.files.insert(path.to_string(), entry);
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut manifest = Manifest::default();
        manifest.insert(
            "main.py",
            Entry {
                sha256: "abc".into(),
                size: 12,
            },
        );
        manifest.insert(
            "lib/a.py",
            Entry {
                sha256: "def".into(),
                size: 3,
            },
        );
        let parsed = Manifest::parse(&manifest.render());
        assert_eq!(parsed.get("main.py").unwrap().size, 12);
        assert_eq!(parsed.get("lib/a.py").unwrap().sha256, "def");
        assert_eq!(parsed.paths().collect::<Vec<_>>(), ["lib/a.py", "main.py"]);
    }

    #[test]
    fn survives_a_damaged_file() {
        let manifest = Manifest::parse(
            "# comment\n\
             \n\
             abc 12 main.py\n\
             not-a-size lib/a.py\n\
             abc notanumber lib/b.py\n\
             truncated\n\
             def 7 lib/c.py",
        );
        assert_eq!(
            manifest.paths().collect::<Vec<_>>(),
            ["lib/c.py", "main.py"]
        );
    }
}
