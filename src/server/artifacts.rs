//! Explicit trusted-backend publication only. No model/HTTP registration path.
use crate::storage::Storage;
use anyhow::{Context, Result, ensure};
use nix::{
    fcntl::{OFlag, open, openat},
    sys::stat::Mode,
};
use serde::Serialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    sync::Arc,
};
const MAX: u64 = 64 * 1024 * 1024;
const DIR: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
const READ: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_NONBLOCK)
    .union(OFlag::O_CLOEXEC);
#[derive(Clone, Debug)]
pub(crate) struct Artifacts {
    directory: Arc<File>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Artifact {
    pub id: String,
    pub size: u64,
    pub digest: String,
    pub name: String,
}
impl Artifacts {
    pub fn new(home: &Path) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let path = home.join("server-artifacts");
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let directory = File::from(open(&path, DIR, Mode::empty())?);
        let meta = directory.metadata()?;
        ensure!(
            meta.uid() == nix::unistd::geteuid().as_raw() && meta.mode() & 0o077 == 0,
            "Artifact store must be private"
        );
        Ok(Self {
            directory: Arc::new(directory),
        })
    }
    /// Caller must explicitly qualify a generated deliverable under the existing
    /// read policy. This conservative adapter rejects config/secret names and
    /// unsupported outputs; a tool trace alone is never qualification.
    pub(crate) async fn register(
        &self,
        storage: &Storage,
        workspace: &Path,
        session: i64,
        relative: &Path,
    ) -> Result<Artifact> {
        let snapshot = storage
            .load_runtime_session_snapshot(crate::storage::SessionId::from_raw(session))
            .await?
            .context("Unknown session")?;
        let workspace = workspace.canonicalize()?;
        ensure!(
            Path::new(&snapshot.summary.project_path) == workspace,
            "Wrong workspace"
        );
        for part in relative.components() {
            let Component::Normal(part) = part else {
                anyhow::bail!("Invalid artifact path")
            };
            let part = part.to_string_lossy().to_ascii_lowercase();
            ensure!(
                !part.starts_with('.')
                    && !["secret", "credential", "config", "token", "key", "password"]
                        .iter()
                        .any(|s| part.contains(s)),
                "Sensitive artifact name"
            );
        }
        ensure!(
            matches!(
                relative.extension().and_then(|e| e.to_str()),
                Some("txt" | "md" | "csv" | "pdf" | "png" | "jpg" | "svg" | "zip")
            ),
            "Unsupported artifact type"
        );
        let source = open_below(&workspace, relative)?;
        let (mut copy, size, digest) = copy_checked(source)?;
        let id = super::auth::random_id()?;
        let mut target = File::from(openat(
            &*self.directory,
            id.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o400),
        )?);
        std::io::copy(&mut copy, &mut target)?;
        target.sync_all()?;
        let record = Artifact {
            id,
            size,
            digest,
            name: relative
                .file_name()
                .context("Missing filename")?
                .to_string_lossy()
                .into_owned(),
        };
        storage
            .api_artifact_insert(&workspace.to_string_lossy(), session, &record)
            .await?;
        Ok(record)
    }
    /// Publish only a backend-generated completion report, never model-selected
    /// workspace bytes. The report is already persisted as session evidence.
    pub(crate) async fn publish_completion(
        &self,
        storage: &Storage,
        session: i64,
        run: &str,
        report: &crate::completion_report::CompletionReport,
    ) -> Result<Artifact> {
        let snapshot = storage
            .load_runtime_session_snapshot(crate::storage::SessionId::from_raw(session))
            .await?
            .context("Unknown session")?;
        let bytes = completion_bytes(report)?;
        ensure!(
            bytes.len() as u64 <= MAX,
            "Completion report exceeds artifact limit"
        );
        let id = super::auth::random_id()?;
        let mut target = File::from(openat(
            &*self.directory,
            id.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o400),
        )?);
        target.write_all(&bytes)?;
        target.sync_all()?;
        let record = Artifact {
            id,
            size: bytes.len() as u64,
            digest: blake3::hash(&bytes).to_hex().to_string(),
            name: format!("completion-{run}.json"),
        };
        storage
            .api_artifact_insert(&snapshot.summary.project_path, session, &record)
            .await?;
        Ok(record)
    }

    /// Remove private copies after their metadata has been committed as deleted.
    /// Missing files are harmless; failed cleanup leaves no downloadable record.
    pub(crate) fn remove(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            ensure!(
                id.len() == 43
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                "Invalid artifact ID"
            );
            match nix::unistd::unlinkat(
                &*self.directory,
                id.as_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            ) {
                Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub async fn list(
        &self,
        storage: &Storage,
        workspace: &str,
        session: i64,
    ) -> Result<Vec<Artifact>> {
        storage.api_artifacts(workspace, session, None).await
    }
    pub async fn download(
        &self,
        storage: &Storage,
        workspace: &str,
        session: i64,
        id: &str,
    ) -> Result<(Artifact, File)> {
        ensure!(
            id.len() == 43
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "Invalid artifact ID"
        );
        let record = storage
            .api_artifacts(workspace, session, Some(id))
            .await?
            .into_iter()
            .next()
            .context("Unknown artifact")?;
        let source = File::from(openat(&*self.directory, id, READ, Mode::empty())?);
        // Return an anonymous validated private copy, never a live replaceable file.
        let (file, size, digest) = copy_checked(source)?;
        ensure!(
            record.size == size && record.digest == digest,
            "Artifact integrity failure"
        );
        Ok((record, file))
    }
}
/// Artifact scope does not grant transcript scope. Keep this DTO allowlisted:
/// no commands, paths, caveats, review text, bindings or tool-derived payloads.
#[derive(Debug, serde::Serialize)]
struct CompletionArtifact<'a> {
    status: crate::completion_report::CompletionStatus,
    authorization: &'a crate::completion_report::CompletionAuthorization,
    prompt_tokens: u64,
    completion_tokens: u64,
    cost_micros: Option<u64>,
    verification_status: Option<crate::verification::VerificationRunStatus>,
    review_status: Option<crate::self_review::SelfReviewRunStatus>,
}

fn completion_bytes(report: &crate::completion_report::CompletionReport) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(&CompletionArtifact {
        status: report.status,
        authorization: &report.authorization,
        prompt_tokens: report.usage.prompt_tokens,
        completion_tokens: report.usage.completion_tokens,
        cost_micros: report.usage.cost_micros,
        verification_status: report.verification.as_ref().map(|run| run.status),
        review_status: report.review.as_ref().map(|run| run.status),
    })?)
}

fn open_below(root: &Path, relative: &Path) -> Result<File> {
    let mut directory = open(root, DIR, Mode::empty())?;
    let mut parts = relative.components().peekable();
    while let Some(component) = parts.next() {
        let Component::Normal(name) = component else {
            anyhow::bail!("Unsafe path")
        };
        let fd = openat(
            &directory,
            name,
            if parts.peek().is_none() { READ } else { DIR },
            Mode::empty(),
        )?;
        if parts.peek().is_none() {
            return Ok(File::from(fd));
        }
        directory = fd;
    }
    anyhow::bail!("Empty artifact path")
}
fn copy_checked(source: File) -> Result<(File, u64, String)> {
    copy_checked_with(source, || {})
}

fn copy_checked_with(mut source: File, after_copy: impl FnOnce()) -> Result<(File, u64, String)> {
    use std::os::unix::fs::MetadataExt;
    let before = source.metadata()?;
    ensure!(
        before.is_file() && before.nlink() == 1 && before.len() <= MAX,
        "Artifact must be a bounded non-linked regular file"
    );
    let mut copy = tempfile::tempfile()?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= MAX, "Artifact too large");
        hasher.update(&buffer[..n]);
        copy.write_all(&buffer[..n])?;
    }
    after_copy();
    let after = source.metadata()?;
    ensure!(
        before.len() == total
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && after.nlink() == 1,
        "Artifact changed during copy"
    );
    copy.seek(SeekFrom::Start(0))?;
    Ok((copy, total, hasher.finalize().to_hex().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_changed_linked_oversized_and_replaced_sources() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("report.txt");
        std::fs::write(&path, "original").unwrap();
        let source = open_below(root.path(), Path::new("report.txt")).unwrap();
        assert!(
            copy_checked_with(source, || std::fs::write(&path, "concurrent change")
                .unwrap())
            .is_err()
        );
        let source = open_below(root.path(), Path::new("report.txt")).unwrap();
        assert!(
            copy_checked_with(source, || {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
            })
            .is_err()
        );
        assert!(open_below(root.path(), Path::new("report.txt")).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "original").unwrap();
        std::fs::hard_link(&path, root.path().join("linked.txt")).unwrap();
        assert!(copy_checked(open_below(root.path(), Path::new("report.txt")).unwrap()).is_err());
        let big = File::create(root.path().join("large.txt")).unwrap();
        big.set_len(MAX + 1).unwrap();
        assert!(copy_checked(open_below(root.path(), Path::new("large.txt")).unwrap()).is_err());
        assert!(open_below(root.path(), Path::new("missing.txt")).is_err());
    }
}
