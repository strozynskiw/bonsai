use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use ignore::WalkBuilder;
use notify::{Event, RecursiveMode, Watcher};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

const RESCAN_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PathKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChoice {
    pub path: String,
    pub kind: PathKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IndexedPath {
    path: String,
    kind: PathKind,
}

#[derive(Debug)]
pub struct PathSearch {
    paths: Arc<RwLock<Vec<IndexedPath>>>,
}

impl PathSearch {
    pub fn start(project_root: PathBuf) -> Result<Self> {
        Self::start_with_options(project_root, true)
    }

    #[cfg(test)]
    fn unavailable(_project_root: PathBuf) -> Self {
        Self {
            paths: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<PathChoice> {
        if limit == 0 {
            return Vec::new();
        }
        let Ok(paths) = self.paths.read() else {
            return Vec::new();
        };
        ranked_choices(&paths, query, limit)
    }

    fn start_with_options(project_root: PathBuf, watch: bool) -> Result<Self> {
        if !project_root.is_dir() {
            anyhow::bail!(
                "Path search root is not a directory: {}",
                project_root.display()
            );
        }

        let paths = Arc::new(RwLock::new(index_paths(&project_root)));
        if watch {
            let (events_tx, events_rx) = mpsc::channel::<notify::Result<Event>>();
            let mut watcher = notify::recommended_watcher(move |event| {
                let _ = events_tx.send(event);
            })?;
            watcher.watch(&project_root, RecursiveMode::Recursive)?;
            spawn_index_watcher(project_root, Arc::clone(&paths), watcher, events_rx);
        }
        Ok(Self { paths })
    }

    #[cfg(test)]
    fn start_for_test(project_root: &Path) -> Result<Self> {
        Self::start_with_options(project_root.to_path_buf(), false)
    }
}

fn spawn_index_watcher(
    project_root: PathBuf,
    paths: Arc<RwLock<Vec<IndexedPath>>>,
    watcher: notify::RecommendedWatcher,
    events_rx: mpsc::Receiver<notify::Result<Event>>,
) {
    thread::spawn(move || {
        let _watcher = watcher;
        while events_rx.recv().is_ok() {
            while events_rx.recv_timeout(RESCAN_DEBOUNCE).is_ok() {}
            let Some(indexed) = try_index_paths(&project_root) else {
                continue;
            };
            if let Ok(mut current) = paths.write() {
                *current = indexed;
            }
        }
    });
}

fn try_index_paths(project_root: &Path) -> Option<Vec<IndexedPath>> {
    project_root.is_dir().then(|| index_paths(project_root))
}

fn index_paths(project_root: &Path) -> Vec<IndexedPath> {
    let mut paths = HashSet::new();
    let mut walker = WalkBuilder::new(project_root);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);

    for entry in walker.build().flatten() {
        let path = entry.path();
        let Ok(relative_path) = path.strip_prefix(project_root) else {
            continue;
        };
        if relative_path.as_os_str().is_empty() {
            continue;
        }
        let Some(path) = normalized_relative_path(relative_path) else {
            continue;
        };
        let kind = if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
        {
            PathKind::Directory
        } else {
            PathKind::File
        };
        paths.insert(IndexedPath { path, kind });
    }

    let mut paths = paths.into_iter().collect::<Vec<_>>();
    paths.sort_by(|left, right| left.path.cmp(&right.path).then(left.kind.cmp(&right.kind)));
    paths
}

fn normalized_relative_path(path: &Path) -> Option<String> {
    let path = path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let path = path.trim_end_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

fn ranked_choices(paths: &[IndexedPath], query: &str, limit: usize) -> Vec<PathChoice> {
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut match_buffer = Vec::new();
    let mut ranked = paths
        .iter()
        .filter_map(|entry| {
            pattern
                .score(Utf32Str::new(&entry.path, &mut match_buffer), &mut matcher)
                .map(|score| (score, entry))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then(left.path.cmp(&right.path))
            .then(left.kind.cmp(&right.kind))
    });

    let mut choices = Vec::with_capacity(limit);
    let mut seen = HashSet::new();
    for (_, entry) in ranked {
        if choices.len() >= limit {
            break;
        }
        let parent = immediate_parent_dir(&entry.path);
        push_path_choice(
            &mut choices,
            &mut seen,
            entry.path.clone(),
            entry.kind,
            limit,
        );
        if matches!(entry.kind, PathKind::File)
            && let Some(parent) = parent
        {
            push_path_choice(&mut choices, &mut seen, parent, PathKind::Directory, limit);
        }
    }
    choices
}

fn push_path_choice(
    choices: &mut Vec<PathChoice>,
    seen: &mut HashSet<(String, PathKind)>,
    path: String,
    kind: PathKind,
    limit: usize,
) {
    let path = path.trim_end_matches('/').to_string();
    if choices.len() >= limit || path.is_empty() {
        return;
    }
    if seen.insert((path.clone(), kind)) {
        choices.push(PathChoice { path, kind });
    }
}

fn immediate_parent_dir(path: &str) -> Option<String> {
    path.rsplit_once('/')
        .map(|(parent, _name)| parent)
        .filter(|parent| !parent.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, content).expect("write fixture file");
    }

    #[test]
    fn searches_files_and_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join("src/main.rs"), "fn main() {}\n");
        write_file(&temp.path().join("src/tool/read.rs"), "read\n");

        let search = PathSearch::start_for_test(temp.path()).expect("path search");
        let choices = search.search("src/ma", 10);

        assert!(choices.iter().any(|choice| {
            choice.path == "src/main.rs" && matches!(choice.kind, PathKind::File)
        }));
        assert!(
            choices.iter().any(|choice| {
                choice.path == "src" && matches!(choice.kind, PathKind::Directory)
            })
        );
    }

    #[test]
    fn ignores_gitignored_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join(".gitignore"), "target/\n");
        write_file(&temp.path().join("target/generated.rs"), "");
        write_file(&temp.path().join("src/main.rs"), "");

        let search = PathSearch::start_for_test(temp.path()).expect("path search");
        let choices = search.search("rs", 10);

        assert!(!choices.iter().any(|choice| choice.path.contains("target")));
        assert!(choices.iter().any(|choice| choice.path == "src/main.rs"));
    }

    #[test]
    fn typo_tolerant_search_finds_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join("src/path_search.rs"), "");

        let search = PathSearch::start_for_test(temp.path()).expect("path search");
        let choices = search.search("pthsrch", 10);

        assert!(
            choices
                .iter()
                .any(|choice| choice.path == "src/path_search.rs")
        );
    }

    #[test]
    fn ranks_exact_path_matches_before_fuzzy_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join("src/path_search.rs"), "");
        write_file(&temp.path().join("src/parse_huge_schema.rs"), "");

        let search = PathSearch::start_for_test(temp.path()).expect("path search");
        let choices = search.search("path_search", 10);

        assert_eq!(
            choices.first().map(|choice| choice.path.as_str()),
            Some("src/path_search.rs")
        );
    }

    #[test]
    fn indexing_synthesizes_parent_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join("src/tool/read.rs"), "");

        let paths = index_paths(temp.path());

        assert!(paths.contains(&IndexedPath {
            path: "src/tool".to_string(),
            kind: PathKind::Directory,
        }));
    }

    #[test]
    fn watcher_refreshes_the_index_after_a_path_is_added() {
        let temp = tempfile::tempdir().expect("tempdir");
        let search = PathSearch::start(temp.path().to_path_buf()).expect("path search");
        write_file(&temp.path().join("src/new_file.rs"), "");

        for _ in 0..20 {
            if search
                .search("new_file", 10)
                .iter()
                .any(|choice| choice.path == "src/new_file.rs")
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }

        panic!("watcher did not refresh the path index");
    }

    #[test]
    fn limits_ranked_results() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join("a.rs"), "");
        write_file(&temp.path().join("b.rs"), "");

        let search = PathSearch::start_for_test(temp.path()).expect("path search");
        let choices = search.search("rs", 1);

        assert_eq!(choices.len(), 1);
    }

    #[test]
    fn empty_query_returns_indexed_choices() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_file(&temp.path().join("Cargo.toml"), "");

        let search = PathSearch::start_for_test(temp.path()).expect("path search");
        let choices = search.search("", 10);

        assert!(!choices.is_empty());
    }

    #[test]
    fn push_path_choice_dedups_trailing_slash_directory_variants() {
        let mut choices = Vec::new();
        let mut seen = HashSet::new();
        push_path_choice(
            &mut choices,
            &mut seen,
            "task_gol/".to_string(),
            PathKind::Directory,
            10,
        );
        push_path_choice(
            &mut choices,
            &mut seen,
            "task_gol".to_string(),
            PathKind::Directory,
            10,
        );

        assert_eq!(
            choices,
            vec![PathChoice {
                path: "task_gol".to_string(),
                kind: PathKind::Directory,
            }],
            "a dir entry and a file's parent must collapse to one row"
        );
    }

    #[test]
    fn unavailable_index_returns_no_choices() {
        let temp = tempfile::tempdir().expect("tempdir");
        let search = PathSearch::unavailable(temp.path().to_path_buf());

        assert!(search.search("anything", 10).is_empty());
    }
}
