//! Bearer material is generated locally, stored only in an exclusive 0600 file.
use crate::storage::{Storage, server_api::ApiToken};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use std::path::Path;

pub(crate) fn digest(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}
pub(crate) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("OS randomness unavailable"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
pub(crate) async fn mint(
    storage: &Storage,
    root: &Path,
    output: &Path,
    ttl: u64,
    sessions: Vec<i64>,
    scopes: Vec<String>,
) -> Result<String> {
    ensure!(ttl > 0 && ttl <= 86400, "Invalid token TTL");
    ensure!(
        scopes
            .iter()
            .all(|s| matches!(s.as_str(), "read" | "control" | "respond" | "artifacts")),
        "Invalid scopes"
    );
    let root = root.canonicalize()?;
    let parent = output
        .parent()
        .context("Token output requires a parent directory")?
        .canonicalize()?;
    ensure!(
        !parent.starts_with(&root),
        "Token file must be outside workspace"
    );
    let output = parent.join(output.file_name().context("Missing token filename")?);
    for id in &sessions {
        let snapshot = storage
            .load_runtime_session_snapshot(crate::storage::SessionId::from_raw(*id))
            .await?
            .context("Unknown session")?;
        ensure!(
            snapshot.summary.project_path == root,
            "Session belongs to another workspace"
        );
    }
    let secret = random_id()?;
    let id = random_id()?;
    let token = ApiToken {
        id: id.clone(),
        workspace: root.to_string_lossy().into_owned(),
        sessions,
        scopes,
        expires_at_ms: crate::util::time::now_ms().saturating_add((ttl * 1000) as i64),
    };
    // Persist hash first: failure never leaves usable untracked bearer material.
    storage.api_insert_token(&token, &digest(&secret)).await?;
    let result = private_write(&output, secret.as_bytes());
    if result.is_err() {
        storage.api_revoke(&token.workspace, &id).await?;
    }
    result?;
    Ok(id)
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        anyhow::bail!("Private server credentials are not supported on this platform")
    }
}
