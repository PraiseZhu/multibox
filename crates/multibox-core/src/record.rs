//! What a worktree was given when Multibox made it.
//!
//! `add` writes this into the worktree's own git directory, such as
//! `.git/worktrees/feature/multibox.json`, so it goes when git removes the
//! worktree. It is what the window shows as space and work saved: on a volume
//! that clones, the filesystem cannot say afterwards which data is shared, and
//! nothing on disk says which caches arrived rather than were built.

use crate::git::Repo;
use crate::plan::Plan;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const FILE: &str = "multibox.json";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    /// When the worktree was made, in seconds since the Unix epoch.
    pub created: u64,
    /// How long making it took.
    pub seconds: f64,
    /// Everything put in it, tracked files and caches alike.
    pub files: u64,
    pub bytes: u64,
    /// Of which caches.
    pub cached_bytes: u64,
    /// Of which hard-linked to the source rather than copied.
    pub shared_bytes: u64,
    /// Whether the volume cloned the data.
    pub reflink: bool,
    /// How much free space the volume lost while the worktree was made, when
    /// that could be read.
    pub disk_used: Option<u64>,
    /// Dependency crates whose compiled outputs arrived with the worktree.
    pub crates: u64,
    /// Assets of Unity projects whose import results arrived with it.
    pub assets: u64,
}

impl Record {
    /// What a native worktree would have had to write: the checkout, and
    /// the caches once its tools had rebuilt them.
    pub fn saved_bytes(&self) -> u64 {
        match self.disk_used {
            Some(used) => self.bytes.saturating_sub(used),
            None => self.shared_bytes,
        }
    }

    pub fn path(git_dir: &Path) -> PathBuf {
        git_dir.join(FILE)
    }

    pub fn read(git_dir: &Path) -> Option<Record> {
        let text = fs::read_to_string(Record::path(git_dir)).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let number = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        Some(Record {
            created: number("created"),
            seconds: value.get("seconds").and_then(Value::as_f64).unwrap_or(0.),
            files: number("files"),
            bytes: number("bytes"),
            cached_bytes: number("cached_bytes"),
            shared_bytes: number("shared_bytes"),
            reflink: value
                .get("reflink")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            disk_used: value.get("disk_used").and_then(Value::as_u64),
            crates: number("crates"),
            assets: number("assets"),
        })
    }

    pub fn write(&self, git_dir: &Path) -> std::io::Result<()> {
        let value = json!({
            "version": 1,
            "created": self.created,
            "seconds": (self.seconds * 10.).round() / 10.,
            "files": self.files,
            "bytes": self.bytes,
            "cached_bytes": self.cached_bytes,
            "shared_bytes": self.shared_bytes,
            "reflink": self.reflink,
            "disk_used": self.disk_used,
            "crates": self.crates,
            "assets": self.assets,
        });
        fs::write(
            Record::path(git_dir),
            serde_json::to_string_pretty(&value)? + "\n",
        )
    }
}

/// The dependency crates whose builds `plan` carries: each one Cargo
/// compiled has a directory under a target's `.fingerprint/`, named for the
/// crate and a 16-digit hash. A crate built for more than one configuration
/// counts once. The workspace's own units are not carried, so all of these
/// are dependencies.
pub(crate) fn carried_crates(plan: &Plan) -> u64 {
    let mut names = BTreeSet::new();
    for file in &plan.manifest.files {
        let Some((_, rest)) = file.rel.split_once("/.fingerprint/") else {
            continue;
        };
        let unit = rest.split('/').next().unwrap_or(rest);
        if let Some(name) = crate_name(unit) {
            names.insert(name);
        }
    }
    names.len() as u64
}

fn crate_name(unit: &str) -> Option<&str> {
    let (name, hash) = unit.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty())
        .then_some(name)
}

/// Assets of the Unity projects whose `Library/` `plan` carries, counted by
/// their `.meta` files in the new worktree's index: every asset and folder
/// the editor imports has one, and with the import results carried, none of
/// them is imported again.
pub(crate) fn carried_assets(plan: &Plan, new: &Repo) -> u64 {
    let projects: Vec<String> = plan
        .projects
        .iter()
        .filter(|project| project.engine == "unity")
        .map(|project| project.path(""))
        .filter(|root| {
            let library = format!("{root}Library/");
            plan.manifest
                .files
                .iter()
                .any(|file| file.rel.starts_with(&library))
        })
        .collect();
    if projects.is_empty() {
        return 0;
    }
    let Ok(index) = crate::index::Index::read(&new.index_path(), new.hash_len) else {
        return 0;
    };
    index
        .entries
        .iter()
        .filter(|entry| entry.path.ends_with(".meta"))
        .filter(|entry| projects.iter().any(|root| entry.path.starts_with(root)))
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_names_come_from_fingerprint_directories() {
        assert_eq!(
            crate_name("serde_json-0123456789abcdef"),
            Some("serde_json")
        );
        assert_eq!(
            crate_name("build-script-build-0123456789abcdef"),
            Some("build-script-build")
        );
        assert_eq!(crate_name("serde_json-0123"), None);
        assert_eq!(crate_name("lib-serde.json"), None);
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let temp = tempfile::tempdir().unwrap();
        let record = Record {
            created: 1_794_820_000,
            seconds: 12.3,
            files: 4_200,
            bytes: 9 << 30,
            cached_bytes: 8 << 30,
            shared_bytes: 3 << 30,
            reflink: false,
            disk_used: Some(6 << 30),
            crates: 36,
            assets: 56_961,
        };
        record.write(temp.path()).unwrap();
        assert_eq!(Record::read(temp.path()), Some(record.clone()));
        assert_eq!(record.saved_bytes(), 3 << 30);
        assert_eq!(Record::read(&temp.path().join("missing")), None);
    }

    #[test]
    fn without_a_reading_of_the_disk_the_savings_are_what_was_shared() {
        let record = Record {
            bytes: 10,
            shared_bytes: 4,
            ..Record::default()
        };
        assert_eq!(record.saved_bytes(), 4);
    }
}
