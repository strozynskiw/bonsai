use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{CredentialBackend, CredentialDeleteOutcome};

const CREDENTIAL_DIRECTORY: &str = "credentials";

#[derive(Debug, Serialize, Deserialize)]
struct FileCredential {
    version: u32,
    provider_id: String,
    secret: String,
}

#[derive(Debug)]
pub(super) struct FileCredentialBackend {
    directory: PathBuf,
}

impl FileCredentialBackend {
    pub(super) fn new(bonsai_home: &Path) -> Self {
        Self {
            directory: bonsai_home.join(CREDENTIAL_DIRECTORY),
        }
    }

    fn credential_path(&self, provider_id: &str) -> PathBuf {
        let digest = blake3::hash(provider_id.as_bytes()).to_hex();
        self.directory.join(format!("{digest}.credential"))
    }

    fn ensure_directory(&self) -> Result<()> {
        std::fs::create_dir_all(&self.directory).with_context(|| {
            format!(
                "Failed to create credential directory {}",
                self.directory.display()
            )
        })?;
        let metadata = std::fs::symlink_metadata(&self.directory).with_context(|| {
            format!(
                "Failed to inspect credential directory {}",
                self.directory.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!(
                "Credential directory {} is not a real directory",
                self.directory.display()
            );
        }
        set_owner_only_directory_permissions(&self.directory)
    }
}

impl CredentialBackend for FileCredentialBackend {
    fn set(&self, provider_id: &str, secret: &str) -> Result<()> {
        self.ensure_directory()?;
        let path = self.credential_path(provider_id);
        let credential = FileCredential {
            version: 1,
            provider_id: provider_id.to_string(),
            secret: secret.to_string(),
        };
        let serialized = serde_json::to_vec_pretty(&credential)
            .context("Failed to serialize provider credential")?;
        let protected = protect_file_payload(&serialized)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".credential-")
            .tempfile_in(&self.directory)
            .with_context(|| {
                format!(
                    "Failed to create temporary credential file in {}",
                    self.directory.display()
                )
            })?;
        set_owner_only_file_permissions(temporary.path())?;
        temporary
            .write_all(&protected)
            .context("Failed to write provider credential")?;
        temporary
            .as_file()
            .sync_all()
            .context("Failed to sync provider credential")?;
        temporary.persist(&path).map_err(|error| {
            anyhow::anyhow!(
                "Failed to replace credential file {}: {}",
                path.display(),
                error.error
            )
        })?;
        set_owner_only_file_permissions(&path)
    }

    fn get(&self, provider_id: &str) -> Result<Option<String>> {
        self.ensure_directory()?;
        let path = self.credential_path(provider_id);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to inspect credential file {}", path.display())
                });
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("Credential path {} is not a real file", path.display());
        }
        set_owner_only_file_permissions(&path)?;
        let content = std::fs::read(&path)
            .with_context(|| format!("Failed to read credential file {}", path.display()))?;
        let content = unprotect_file_payload(&content)?;
        let credential: FileCredential = serde_json::from_slice(&content)
            .with_context(|| format!("Failed to parse credential file {}", path.display()))?;
        if credential.version != 1 || credential.provider_id != provider_id {
            anyhow::bail!("Credential file {} has invalid identity", path.display());
        }
        Ok(Some(credential.secret))
    }

    fn delete(&self, provider_id: &str) -> Result<CredentialDeleteOutcome> {
        let path = self.credential_path(provider_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(CredentialDeleteOutcome::Deleted),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(CredentialDeleteOutcome::NotFound)
            }
            Err(error) => Err(error)
                .with_context(|| format!("Failed to delete credential file {}", path.display())),
        }
    }
}

#[cfg(unix)]
fn set_owner_only_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to protect credential directory {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to protect credential file {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn protect_file_payload(payload: &[u8]) -> Result<Vec<u8>> {
    Ok(payload.to_vec())
}

#[cfg(not(windows))]
fn unprotect_file_payload(payload: &[u8]) -> Result<Vec<u8>> {
    Ok(payload.to_vec())
}

#[cfg(windows)]
fn protect_file_payload(payload: &[u8]) -> Result<Vec<u8>> {
    windows_file_protection::protect(payload)
}

#[cfg(windows)]
fn unprotect_file_payload(payload: &[u8]) -> Result<Vec<u8>> {
    windows_file_protection::unprotect(payload)
}

#[cfg(windows)]
mod windows_file_protection {
    #![allow(
        unsafe_code,
        reason = "thin FFI over Windows DPAPI (CryptProtectData/CryptUnprotectData/LocalFree); every block carries a local SAFETY comment"
    )]
    use anyhow::{Context, Result};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    pub(super) fn protect(payload: &[u8]) -> Result<Vec<u8>> {
        transform(payload, Transform::Protect)
    }

    pub(super) fn unprotect(payload: &[u8]) -> Result<Vec<u8>> {
        transform(payload, Transform::Unprotect)
    }

    #[derive(Debug, Clone, Copy)]
    enum Transform {
        Protect,
        Unprotect,
    }

    fn transform(payload: &[u8], transform: Transform) -> Result<Vec<u8>> {
        let size = u32::try_from(payload.len()).context("Credential payload is too large")?;
        let input = CRYPT_INTEGER_BLOB {
            cbData: size,
            pbData: payload.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points to `payload` for `cbData` bytes for the duration
        // of the call. Optional pointers are null, UI is forbidden, and `output`
        // is initialized by DPAPI on success. Its allocation is copied before
        // being released with the documented `LocalFree` allocator.
        let succeeded = unsafe {
            match transform {
                Transform::Protect => CryptProtectData(
                    &input,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                ),
                Transform::Unprotect => CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                ),
            }
        };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error()).context("Windows DPAPI failed");
        }
        // SAFETY: a successful DPAPI call returns an allocation containing
        // exactly `cbData` initialized bytes at `pbData`.
        let bytes = unsafe {
            std::slice::from_raw_parts(output.pbData.cast_const(), output.cbData as usize).to_vec()
        };
        // SAFETY: DPAPI documents that `pbData` must be released with
        // `LocalFree`; ownership has not been transferred elsewhere.
        unsafe {
            LocalFree(output.pbData.cast());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_file_store_round_trips_with_owner_only_permissions() {
        let home = tempfile::TempDir::new().unwrap();
        let store = FileCredentialBackend::new(home.path());
        store.set("custom/provider", "secret-value").unwrap();

        assert_eq!(
            store.get("custom/provider").unwrap().as_deref(),
            Some("secret-value")
        );
        let files = std::fs::read_dir(home.path().join(CREDENTIAL_DIRECTORY))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].file_name().to_string_lossy().contains("custom"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                std::fs::metadata(home.path().join(CREDENTIAL_DIRECTORY))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                files[0].metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        store.delete("custom/provider").unwrap();
        assert!(store.get("custom/provider").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn protected_file_store_rejects_symlinked_credential() {
        use std::os::unix::fs::symlink;

        let home = tempfile::TempDir::new().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let store = FileCredentialBackend::new(home.path());
        store.ensure_directory().unwrap();
        symlink(outside.path(), store.credential_path("anthropic")).unwrap();

        let error = store.get("anthropic").unwrap_err();
        assert!(error.to_string().contains("not a real file"));
    }
}
