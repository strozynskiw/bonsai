//! Descriptor-relative, no-follow filesystem commits for mutation tools.
//!
//! Path policy resolves a canonical root-relative target first. Commit then
//! walks that relative path from an open project-root descriptor with
//! `O_NOFOLLOW` on every component. A directory entry swapped to a symlink
//! between policy validation and commit is rejected instead of redirecting the
//! write or delete outside the approved root.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat, renameat};
use nix::sys::stat::{Mode, SFlag, fchmod, fstat, mkdirat};
use nix::unistd::{UnlinkatFlags, fsync, unlinkat};

use crate::storage::WorkspaceLeaseFence;

use super::file_mutation::WritePrecondition;

const DIRECTORY_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_CLOEXEC)
    .union(OFlag::O_NOFOLLOW);
const READ_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_CLOEXEC)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_NONBLOCK);

/// Atomically replace one root-relative file without resolving any path
/// component through a symlink during the commit.
pub(super) fn atomic_write(
    project_root: &Path,
    relative_path: &Path,
    content: &[u8],
    precondition: WritePrecondition<'_>,
    create_parents: bool,
    temp_name: &OsStr,
    lease_fence: Option<&WorkspaceLeaseFence>,
) -> Result<()> {
    let (parent, file_name) = open_parent(project_root, relative_path, create_parents)?;
    let existing_mode = existing_regular_mode(&parent, &file_name)?;
    let temp_fd = openat(
        &parent,
        temp_name,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(0o666),
    )
    .with_context(|| {
        format!("failed to create descriptor-relative temporary file {temp_name:?}")
    })?;
    let mut temp = File::from(temp_fd);

    let result = (|| {
        if let Some(mode) = existing_mode {
            fchmod(&temp, mode).context("failed to preserve destination permissions")?;
        }
        temp.write_all(content)
            .context("failed to write descriptor-relative temporary file")?;
        temp.flush()
            .context("failed to flush descriptor-relative temporary file")?;
        temp.sync_all()
            .context("failed to sync descriptor-relative temporary file")?;
        verify_precondition(&parent, &file_name, precondition)?;
        if let Some(fence) = lease_fence {
            fence.ensure_held()?;
        }
        renameat(&parent, temp_name, &parent, file_name.as_os_str())
            .context("failed to commit descriptor-relative file replacement")?;
        fsync(&parent).context("failed to sync destination directory")?;
        Ok(())
    })();

    if result.is_err() {
        let _ = unlinkat(&parent, temp_name, UnlinkatFlags::NoRemoveDir);
    }
    result
}

/// Delete one root-relative file through its already-open parent directory.
pub(super) fn remove_file(
    project_root: &Path,
    relative_path: &Path,
    precondition: WritePrecondition<'_>,
    lease_fence: Option<&WorkspaceLeaseFence>,
) -> Result<()> {
    let (parent, file_name) = open_parent(project_root, relative_path, false)?;
    verify_precondition(&parent, &file_name, precondition)?;
    if let Some(fence) = lease_fence {
        fence.ensure_held()?;
    }
    unlinkat(&parent, file_name.as_os_str(), UnlinkatFlags::NoRemoveDir)
        .context("failed to delete descriptor-relative file")?;
    fsync(&parent).context("failed to sync destination directory")?;
    Ok(())
}

fn open_parent(
    project_root: &Path,
    relative_path: &Path,
    create_parents: bool,
) -> Result<(std::os::fd::OwnedFd, OsString)> {
    let mut components = normalized_components(relative_path)?;
    let file_name = components
        .pop()
        .context("mutation target must name a file below the project root")?;
    let mut directory = open(project_root, DIRECTORY_FLAGS, Mode::empty())
        .with_context(|| format!("failed to open project root {}", project_root.display()))?;

    for component in components {
        let next = match openat(
            &directory,
            component.as_os_str(),
            DIRECTORY_FLAGS,
            Mode::empty(),
        ) {
            Ok(next) => next,
            Err(Errno::ENOENT) if create_parents => {
                match mkdirat(
                    &directory,
                    component.as_os_str(),
                    Mode::from_bits_truncate(0o755),
                ) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to create mutation parent {component:?}")
                        });
                    }
                }
                openat(
                    &directory,
                    component.as_os_str(),
                    DIRECTORY_FLAGS,
                    Mode::empty(),
                )
                .with_context(|| {
                    format!("failed to open newly created mutation parent {component:?}")
                })?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("unsafe or unavailable mutation parent {component:?}")
                });
            }
        };
        directory = next;
    }

    Ok((directory, file_name))
}

fn normalized_components(path: &Path) -> Result<Vec<OsString>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => out.push(value.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "descriptor-relative mutation target must stay below the project root: {}",
                    path.display()
                )
            }
        }
    }
    Ok(out)
}

fn existing_regular_mode<Fd: std::os::fd::AsFd>(
    parent: &Fd,
    file_name: &OsStr,
) -> Result<Option<Mode>> {
    match openat(parent, file_name, READ_FLAGS, Mode::empty()) {
        Ok(fd) => {
            let stat = fstat(&fd).context("failed to inspect destination file")?;
            ensure_regular_file(&stat)?;
            Ok(Some(Mode::from_bits_truncate(
                stat.st_mode as nix::libc::mode_t,
            )))
        }
        Err(Errno::ENOENT) => Ok(None),
        Err(error) => Err(error).context("failed to inspect destination file"),
    }
}

fn verify_precondition<Fd: std::os::fd::AsFd>(
    parent: &Fd,
    file_name: &OsStr,
    precondition: WritePrecondition<'_>,
) -> Result<()> {
    match precondition {
        WritePrecondition::None => Ok(()),
        WritePrecondition::Absent => match openat(parent, file_name, READ_FLAGS, Mode::empty()) {
            Err(Errno::ENOENT) => Ok(()),
            Ok(_) | Err(_) => anyhow::bail!(
                "File changed while preparing this mutation: {:?} was created. Read it and retry.",
                file_name
            ),
        },
        WritePrecondition::Exact(expected) => {
            let fd = openat(parent, file_name, READ_FLAGS, Mode::empty())
                .context("failed to open destination for the final precondition check")?;
            let stat = fstat(&fd).context("failed to inspect destination precondition")?;
            ensure_regular_file(&stat)?;
            let mut current = Vec::new();
            File::from(fd)
                .read_to_end(&mut current)
                .context("failed to read destination for the final precondition check")?;
            if current == expected.as_bytes() {
                Ok(())
            } else {
                anyhow::bail!(
                    "File changed while preparing this mutation: {:?} no longer matches the content you read. Read it again and retry.",
                    file_name
                )
            }
        }
    }
}

fn ensure_regular_file(stat: &nix::libc::stat) -> Result<()> {
    let kind = SFlag::from_bits_truncate(stat.st_mode);
    if kind.contains(SFlag::S_IFREG) {
        Ok(())
    } else {
        anyhow::bail!("mutation target is not a regular file")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_symlink_swap_cannot_redirect_create() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("project");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(root.join("safe")).unwrap();
        std::fs::create_dir(&outside).unwrap();

        std::fs::rename(root.join("safe"), root.join("safe-old")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("safe")).unwrap();

        let error = atomic_write(
            &root,
            Path::new("safe/new.txt"),
            b"blocked",
            WritePrecondition::Absent,
            true,
            OsStr::new(".bonsai-test-temp"),
            None,
        )
        .unwrap_err();

        assert!(!outside.join("new.txt").exists());
        assert!(error.to_string().contains("mutation parent"));
    }

    #[test]
    fn leaf_symlink_is_replaced_not_followed() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("project");
        let outside = fixture.path().join("outside.txt");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("target.txt")).unwrap();

        let error = atomic_write(
            &root,
            Path::new("target.txt"),
            b"replacement",
            WritePrecondition::None,
            false,
            OsStr::new(".bonsai-test-temp"),
            None,
        )
        .unwrap_err();

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
        assert!(error.to_string().contains("destination file"));
    }

    #[test]
    fn project_root_symlink_swap_is_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("project");
        let moved_root = fixture.path().join("project-old");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::rename(&root, &moved_root).unwrap();
        std::os::unix::fs::symlink(&outside, &root).unwrap();

        let error = atomic_write(
            &root,
            Path::new("new.txt"),
            b"blocked",
            WritePrecondition::Absent,
            false,
            OsStr::new(".bonsai-test-temp"),
            None,
        )
        .unwrap_err();

        assert!(!outside.join("new.txt").exists());
        assert!(error.to_string().contains("failed to open project root"));
    }
}
