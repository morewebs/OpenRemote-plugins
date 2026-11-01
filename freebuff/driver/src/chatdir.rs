//! Finding the chat directory Freebuff is writing for this session.
//!
//! Freebuff keeps chats at
//! `~/.config/manicode/projects/<project>/chats/<chat-id>/`, where `<project>`
//! is the working directory's own name and `<chat-id>` is a timestamp such as
//! `2026-10-31T20-33-15.026Z` (the id `--continue` takes). The layout is
//! internal to the CLI, so every lookup degrades to "not found" rather than an
//! error, and the session keeps working without a transcript.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A chat directory and the id that address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDir {
    pub dir: PathBuf,
    pub id: String,
}

/// `~/.config/manicode/projects`.
pub fn projects_root() -> Option<PathBuf> {
    crate::probe::manicode_dir().map(|dir| dir.join("projects"))
}

/// The project directory Freebuff uses for a working directory: its basename,
/// when a directory with that name exists.
///
/// The comparison is case-insensitive on Windows, where the store is not
/// guaranteed to keep the case the caller typed.
pub fn project_dir(projects: &Path, cwd: &Path) -> Option<PathBuf> {
    let name = cwd.file_name()?.to_str()?;
    let direct = projects.join(name);
    if direct.is_dir() {
        return Some(direct);
    }
    if cfg!(windows) {
        let wanted = name.to_ascii_lowercase();
        if let Ok(entries) = fs::read_dir(projects) {
            for entry in entries.flatten() {
                let candidate = entry.path();
                if !candidate.is_dir() {
                    continue;
                }
                let matches = candidate
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.to_ascii_lowercase() == wanted)
                    .unwrap_or(false);
                if matches {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Every chat directory across every project, as `(chat-id, directory)`.
pub fn all_chats(projects: &Path) -> Vec<ChatDir> {
    let mut chats = Vec::new();
    let projects_iter = match fs::read_dir(projects) {
        Ok(iter) => iter,
        Err(_) => return chats,
    };
    for project in projects_iter.flatten() {
        let chats_dir = project.path().join("chats");
        let entries = match fs::read_dir(&chats_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let id = match dir.file_name().and_then(|n| n.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            chats.push(ChatDir { dir, id });
        }
    }
    chats
}

/// The ids currently on disk, so a chat started after spawn can be told apart
/// from the ones that were already there.
pub fn known_ids(projects: &Path) -> HashSet<String> {
    all_chats(projects)
        .into_iter()
        .map(|chat| chat.id)
        .collect()
}

fn modified(dir: &Path) -> Option<SystemTime> {
    fs::metadata(dir).ok().and_then(|meta| meta.modified().ok())
}

/// Freebuff's chat ids sort by creation time as plain strings (`2` sorts before
/// `1` only across years, and the format is fixed), so the greatest id is the
/// newest chat.
fn newest_by_id(chats: &mut [ChatDir]) -> Option<ChatDir> {
    chats.sort_by(|a, b| b.id.cmp(&a.id));
    chats.first().cloned()
}

/// Timing slack for the "created after spawn" test: the chat directory's
/// timestamp comes from the CLI's clock and ours comes from ours, and Windows
/// can hand back a coarser modification time.
const SLACK: Duration = Duration::from_secs(5);

/// Find the chat Freebuff opened for this session.
///
/// `spawned_at` is when the driver started the CLI and `known` is the set of
/// chat ids that existed then. Preference order, exactly as the design says:
/// the resumed id; else the newest new chat in the working directory's project;
/// else the newest new chat in any project.
pub fn find_started_chat(
    projects: &Path,
    cwd: &Path,
    spawned_at: SystemTime,
    known: &HashSet<String>,
    resume: Option<&str>,
) -> Option<ChatDir> {
    if let Some(id) = resume {
        if let Some(project) = project_dir(projects, cwd) {
            let dir = project.join("chats").join(id);
            if dir.is_dir() {
                return Some(ChatDir {
                    dir,
                    id: id.to_string(),
                });
            }
        }
        // A resumed chat can belong to another project when the same id is
        // resumed from a different directory.
        return all_chats(projects).into_iter().find(|chat| chat.id == id);
    }

    let mut fresh: Vec<ChatDir> = all_chats(projects)
        .into_iter()
        .filter(|chat| !known.contains(&chat.id))
        .filter(|chat| {
            modified(&chat.dir)
                .map(|when| when + SLACK >= spawned_at)
                .unwrap_or(false)
        })
        .collect();

    if let Some(project) = project_dir(projects, cwd) {
        let chats_dir = project.join("chats");
        let mut in_project: Vec<ChatDir> = fresh
            .iter()
            .filter(|chat| chat.dir.parent() == Some(chats_dir.as_path()))
            .cloned()
            .collect();
        if let Some(found) = newest_by_id(&mut in_project) {
            return Some(found);
        }
    }
    newest_by_id(&mut fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_chat(root: &Path, project: &str, id: &str) -> PathBuf {
        let dir = root.join(project).join("chats").join(id);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("freebuff-driver-chatdir-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_project_is_the_working_directorys_name() {
        let root = scratch("project");
        fs::create_dir_all(root.join("my-app")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();

        let found = project_dir(&root, Path::new("C:\\src\\my-app"));
        assert_eq!(found, Some(root.join("my-app")));

        // A directory with no matching project is not guessed at.
        assert_eq!(project_dir(&root, Path::new("C:\\src\\nowhere")), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_project_lookup_falls_back_to_a_case_insensitive_match() {
        let root = scratch("project-case");
        let canonical = root.join("My-App");
        fs::create_dir_all(&canonical).unwrap();
        if cfg!(windows) {
            // NTFS resolves the name case-insensitively, so either path shape
            // is a correct answer here; what matters is that it points at the
            // store's own directory.
            let found = project_dir(&root, Path::new("C:\\src\\my-app")).unwrap();
            assert_eq!(
                found.canonicalize().unwrap(),
                canonical.canonicalize().unwrap(),
                "the project path must resolve to the store's directory"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_chat_in_the_projects_own_directory_wins() {
        let root = scratch("newest");
        fs::create_dir_all(root.join("my-app")).unwrap();
        let _ = touch_chat(&root, "my-app", "2026-10-31T20-00-00.000Z");
        let started = SystemTime::now();
        let known = known_ids(&root);

        let newer = touch_chat(&root, "my-app", "2026-10-31T20-33-15.026Z");
        let found = find_started_chat(&root, Path::new("C:\\src\\my-app"), started, &known, None);
        assert_eq!(found.map(|c| c.dir), Some(newer));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn chats_that_existed_before_spawn_are_never_claimed() {
        let root = scratch("known");
        fs::create_dir_all(root.join("my-app")).unwrap();
        let _ = touch_chat(&root, "my-app", "2026-10-31T20-33-15.026Z");
        let known = known_ids(&root);

        let found = find_started_chat(
            &root,
            Path::new("C:\\src\\my-app"),
            SystemTime::now(),
            &known,
            None,
        );
        assert_eq!(found, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn another_projects_chat_is_found_when_the_named_project_has_none() {
        let root = scratch("other");
        fs::create_dir_all(root.join("unrelated")).unwrap();
        let started = SystemTime::now();
        let known = known_ids(&root);

        let created = touch_chat(&root, "unrelated", "2026-10-31T21-00-00.000Z");
        let found = find_started_chat(&root, Path::new("C:\\src\\my-app"), started, &known, None);
        assert_eq!(found.map(|c| c.dir), Some(created));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_resumed_chat_is_used_as_it_stands() {
        let root = scratch("resume");
        fs::create_dir_all(root.join("my-app")).unwrap();
        let resumed = touch_chat(&root, "my-app", "2026-10-30T01-00-00.000Z");
        let known = known_ids(&root);

        let found = find_started_chat(
            &root,
            Path::new("C:\\src\\my-app"),
            SystemTime::now(),
            &known,
            Some("2026-10-30T01-00-00.000Z"),
        );
        assert_eq!(found.map(|c| c.dir), Some(resumed));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_resumed_id_from_another_project_still_resolves() {
        let root = scratch("resume-other");
        fs::create_dir_all(root.join("my-app")).unwrap();
        let elsewhere = touch_chat(&root, "unrelated", "2026-10-30T01-00-00.000Z");
        let known = known_ids(&root);

        let found = find_started_chat(
            &root,
            Path::new("C:\\src\\my-app"),
            SystemTime::now(),
            &known,
            Some("2026-10-30T01-00-00.000Z"),
        );
        assert_eq!(found.map(|c| c.dir), Some(elsewhere));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_absent_projects_root_is_not_an_error() {
        let missing = std::env::temp_dir().join("freebuff-driver-chatdir-does-not-exist");
        let found = find_started_chat(
            &missing,
            Path::new("C:\\src\\my-app"),
            SystemTime::now(),
            &HashSet::new(),
            None,
        );
        assert_eq!(found, None);
    }
}
