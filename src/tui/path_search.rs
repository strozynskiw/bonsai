use std::collections::HashSet;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, MixedItemRef, MixedSearchConfig,
    PaginationArgs, QueryParser, SharedFilePicker, SharedFrecency,
};

const STARTUP_SCAN_WAIT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChoice {
    pub path: String,
    pub kind: PathKind,
}

#[derive(Debug)]
pub struct PathSearch {
    project_root: PathBuf,
    picker: SharedFilePicker,
    parser: QueryParser<MixedSearchConfig>,
}

impl PathSearch {
    pub fn start(project_root: PathBuf) -> Result<Self> {
        Self::start_with_options(project_root, true, STARTUP_SCAN_WAIT)
    }

    #[cfg(test)]
    fn unavailable(project_root: PathBuf) -> Self {
        Self {
            project_root,
            picker: SharedFilePicker::default(),
            parser: QueryParser::new(MixedSearchConfig),
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<PathChoice> {
        if limit == 0 {
            return Vec::new();
        }
        let Ok(guard) = self.picker.read() else {
            return Vec::new();
        };
        let Some(picker) = guard.as_ref() else {
            return Vec::new();
        };
        let parsed = self.parser.parse(query);
        let results = picker.fuzzy_search_mixed(
            &parsed,
            None,
            FuzzySearchOptions {
                max_threads: 0,
                project_path: Some(&self.project_root),
                pagination: PaginationArgs { offset: 0, limit },
                ..Default::default()
            },
        );

        let mut choices = Vec::with_capacity(limit);
        let mut seen = HashSet::new();
        for item in results.items {
            if choices.len() >= limit {
                break;
            }
            match item {
                MixedItemRef::File(file) => {
                    let path = file.relative_path(picker);
                    let parent = immediate_parent_dir(&path);
                    push_path_choice(&mut choices, &mut seen, path, PathKind::File, limit);
                    if let Some(parent) = parent {
                        push_path_choice(
                            &mut choices,
                            &mut seen,
                            parent,
                            PathKind::Directory,
                            limit,
                        );
                    }
                }
                MixedItemRef::Dir(dir) => {
                    push_path_choice(
                        &mut choices,
                        &mut seen,
                        dir.relative_path(picker),
                        PathKind::Directory,
                        limit,
                    );
                }
            }
        }
        choices
    }

    fn start_with_options(project_root: PathBuf, watch: bool, wait: Duration) -> Result<Self> {
        let picker = SharedFilePicker::default();
        let frecency = SharedFrecency::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: project_root.to_string_lossy().to_string(),
                mode: FFFMode::Ai,
                watch,
                ..Default::default()
            },
        )?;
        let _ = picker.wait_for_scan(wait);

        Ok(Self {
            project_root,
            picker,
            parser: QueryParser::new(MixedSearchConfig),
        })
    }

    #[cfg(test)]
    fn start_for_test(project_root: &Path) -> Result<Self> {
        Self::start_with_options(project_root.to_path_buf(), false, Duration::from_secs(5))
    }
}

fn push_path_choice(
    choices: &mut Vec<PathChoice>,
    seen: &mut HashSet<(String, PathKind)>,
    path: String,
    kind: PathKind,
    limit: usize,
) {
    // Normalize away trailing slashes before dedup: a directory can arrive
    // both as an indexed dir entry ("dir/") and as a file's parent ("dir"),
    // and the popover re-adds the slash for display.
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
