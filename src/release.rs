//! Verification of signed release manifests used by online diagnostics.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use futures::StreamExt as _;
use reqwest::header::{ACCEPT, USER_AGENT};
use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

const RELEASE_REPOSITORY: &str = "strozynskiw/bonsai";
const RELEASES_API: &str = "https://api.github.com/repos/strozynskiw/bonsai/releases?per_page=30";
const RELEASE_BY_TAG_API: &str = "https://api.github.com/repos/strozynskiw/bonsai/releases/tags";
const MANIFEST_ASSET: &str = "release-manifest.json";
const SIGNATURE_ASSET: &str = "release-manifest.json.sig";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const RELEASE_LIST_LIMIT: usize = 512 * 1024;
const MANIFEST_LIMIT: usize = 64 * 1024;
const SIGNATURE_LIMIT: usize = 512;
/// Streamed download cap for a release archive (self-update). Archives are
/// tens of megabytes today; the cap only guards against a runaway body.
pub(crate) const ARCHIVE_LIMIT: usize = 256 * 1024 * 1024;

/// Compile-time identity carried by this binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseIdentity {
    Development,
    Official { tag: &'static str },
    Invalid,
}

/// Secret- and path-free outcome consumed by `/doctor online`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseStatus {
    DevelopmentBuild,
    VerifiedCurrent {
        version: String,
    },
    UpdateAvailable {
        current: String,
        latest: String,
    },
    /// A newer verified binary is already staged on disk (self-update); the
    /// running image predates it. Produced only by
    /// [`crate::update::refine_release_status`], never by [`check`] itself.
    UpdateStaged {
        version: String,
    },
    LookupFailed,
    VerificationFailed,
}

#[derive(Debug, Error)]
pub(crate) enum ReleaseError {
    #[error("release lookup failed")]
    Network,
    #[error("release metadata is invalid")]
    InvalidMetadata,
    #[error("release signature verification failed")]
    InvalidSignature,
    #[error("installed executable could not be verified")]
    Executable,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseManifest {
    schema_version: u32,
    repository: String,
    version: String,
    tag: String,
    pub(crate) assets: Vec<ManifestAsset>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ManifestAsset {
    pub(crate) target: String,
    pub(crate) archive: String,
    pub(crate) archive_sha256: String,
    pub(crate) binary_sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ReleaseVerifier {
    public_key: [u8; 32],
    tag: &'static str,
}

impl ReleaseVerifier {
    pub(crate) fn embedded() -> Result<Option<Self>, ReleaseError> {
        let public_key = option_env!("BONSAI_RELEASE_PUBLIC_KEY")
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let tag = option_env!("BONSAI_RELEASE_TAG")
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match (public_key, tag) {
            (None, None) => Ok(None),
            (Some(encoded), Some(tag)) => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| ReleaseError::InvalidMetadata)?;
                let public_key = decoded
                    .try_into()
                    .map_err(|_| ReleaseError::InvalidMetadata)?;
                Ok(Some(Self { public_key, tag }))
            }
            _ => Err(ReleaseError::InvalidMetadata),
        }
    }
}

/// Return the offline release identity without touching the network or disk.
pub(crate) fn identity() -> ReleaseIdentity {
    match ReleaseVerifier::embedded() {
        Ok(None) => ReleaseIdentity::Development,
        Ok(Some(verifier)) if verifier.tag == format!("v{}", env!("CARGO_PKG_VERSION")) => {
            ReleaseIdentity::Official { tag: verifier.tag }
        }
        Ok(Some(_)) | Err(_) => ReleaseIdentity::Invalid,
    }
}

/// Verify the installed executable and look for a newer signed release.
pub(crate) async fn check() -> ReleaseStatus {
    let verifier = match ReleaseVerifier::embedded() {
        Ok(Some(verifier)) => verifier,
        Ok(None) => return ReleaseStatus::DevelopmentBuild,
        Err(_) => return ReleaseStatus::VerificationFailed,
    };
    match check_official(&verifier).await {
        Ok(status) => status,
        Err(ReleaseError::Network) => ReleaseStatus::LookupFailed,
        Err(
            ReleaseError::InvalidMetadata
            | ReleaseError::InvalidSignature
            | ReleaseError::Executable,
        ) => ReleaseStatus::VerificationFailed,
    }
}

async fn check_official(verifier: &ReleaseVerifier) -> Result<ReleaseStatus, ReleaseError> {
    let current = current_version()?;
    let client = release_client()?;
    verify_current_install(&client, verifier).await?;
    match find_verified_update(&client, verifier, &current).await? {
        Some(update) => Ok(ReleaseStatus::UpdateAvailable {
            current: current.to_string(),
            latest: update.version.to_string(),
        }),
        None => Ok(ReleaseStatus::VerifiedCurrent {
            version: current.to_string(),
        }),
    }
}

/// The version this binary was compiled as.
pub(crate) fn current_version() -> Result<Version, ReleaseError> {
    Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| ReleaseError::InvalidMetadata)
}

/// Verify that the running binary matches its own signed release manifest and
/// return that manifest. `ReleaseError::Executable` means the binary on disk
/// does not hash to what the manifest recorded (locally rebuilt, tampered, or
/// already replaced by a staged update).
pub(crate) async fn verify_current_install(
    client: &reqwest::Client,
    verifier: &ReleaseVerifier,
) -> Result<ReleaseManifest, ReleaseError> {
    let current = current_version()?;
    if verifier.tag != format!("v{current}") {
        return Err(ReleaseError::InvalidMetadata);
    }
    let current_release: GitHubRelease = fetch_json(
        client,
        &format!("{RELEASE_BY_TAG_API}/{}", verifier.tag),
        RELEASE_LIST_LIMIT,
    )
    .await?;
    if current_release.draft || current_release.tag_name != verifier.tag {
        return Err(ReleaseError::InvalidMetadata);
    }
    let current_manifest = fetch_verified_manifest(client, &current_release, verifier).await?;
    validate_manifest(&current_manifest, verifier.tag)?;
    verify_installed_binary(&current_manifest).await?;
    Ok(current_manifest)
}

/// A release newer than `current` whose manifest passed signature and
/// consistency verification. The `release` is kept so asset downloads go
/// through the same repo-pinned URL validation as the manifest itself.
#[derive(Debug)]
pub(crate) struct VerifiedUpdate {
    pub(crate) version: Version,
    pub(crate) manifest: ReleaseManifest,
    pub(crate) release: GitHubRelease,
}

impl VerifiedUpdate {
    /// Repo-pinned download URL for one of this update's verified assets.
    pub(crate) fn asset_url(&self, asset_name: &str) -> Result<&str, ReleaseError> {
        release_asset_url(&self.release, asset_name)
    }
}

/// Find the newest signed release newer than `current`, or `None` when the
/// installed version is already the newest verified one.
pub(crate) async fn find_verified_update(
    client: &reqwest::Client,
    verifier: &ReleaseVerifier,
    current: &Version,
) -> Result<Option<VerifiedUpdate>, ReleaseError> {
    let mut releases: Vec<GitHubRelease> =
        fetch_json(client, RELEASES_API, RELEASE_LIST_LIMIT).await?;
    releases.retain(|release| !release.draft);
    releases.sort_by(|left, right| {
        release_version(right)
            .cmp(&release_version(left))
            .then_with(|| right.tag_name.cmp(&left.tag_name))
    });
    for release in releases {
        let Some(candidate) = release_version(&release) else {
            continue;
        };
        if candidate <= *current {
            continue;
        }
        let manifest = match fetch_verified_manifest(client, &release, verifier).await {
            Ok(manifest) => manifest,
            Err(ReleaseError::Network) => return Err(ReleaseError::Network),
            Err(
                ReleaseError::InvalidMetadata
                | ReleaseError::InvalidSignature
                | ReleaseError::Executable,
            ) => continue,
        };
        if validate_manifest(&manifest, &release.tag_name).is_ok()
            && Version::parse(&manifest.version).ok().as_ref() == Some(&candidate)
        {
            return Ok(Some(VerifiedUpdate {
                version: candidate,
                manifest,
                release,
            }));
        }
    }
    Ok(None)
}

pub(crate) fn release_client() -> Result<reqwest::Client, ReleaseError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if is_allowed_redirect(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|_| ReleaseError::Network)
}

fn is_allowed_redirect(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
            )
        )
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
) -> Result<T, ReleaseError> {
    let response = client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(USER_AGENT, concat!("bonsai/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .map_err(|_| ReleaseError::Network)?
        .error_for_status()
        .map_err(|_| ReleaseError::Network)?;
    let body = bounded_body(response, limit).await?;
    serde_json::from_slice(&body).map_err(|_| ReleaseError::InvalidMetadata)
}

async fn fetch_verified_manifest(
    client: &reqwest::Client,
    release: &GitHubRelease,
    verifier: &ReleaseVerifier,
) -> Result<ReleaseManifest, ReleaseError> {
    let manifest_url = release_asset_url(release, MANIFEST_ASSET)?;
    let signature_url = release_asset_url(release, SIGNATURE_ASSET)?;
    let manifest = fetch_asset(client, manifest_url, MANIFEST_LIMIT).await?;
    let encoded_signature = fetch_asset(client, signature_url, SIGNATURE_LIMIT).await?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(trim_ascii(&encoded_signature))
        .map_err(|_| ReleaseError::InvalidSignature)?;
    verify_signature(&verifier.public_key, &manifest, &signature)?;
    serde_json::from_slice(&manifest).map_err(|_| ReleaseError::InvalidMetadata)
}

fn release_asset_url<'a>(release: &'a GitHubRelease, name: &str) -> Result<&'a str, ReleaseError> {
    let mut matches = release.assets.iter().filter(|asset| asset.name == name);
    let url = matches
        .next()
        .map(|asset| asset.browser_download_url.as_str())
        .ok_or(ReleaseError::InvalidMetadata)?;
    if matches.next().is_some() {
        return Err(ReleaseError::InvalidMetadata);
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| ReleaseError::InvalidMetadata)?;
    let expected_prefix = format!("/{RELEASE_REPOSITORY}/releases/download/");
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || !parsed.path().starts_with(&expected_prefix)
    {
        return Err(ReleaseError::InvalidMetadata);
    }
    Ok(url)
}

pub(crate) async fn fetch_asset(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
) -> Result<Vec<u8>, ReleaseError> {
    let response = client
        .get(url)
        .header(USER_AGENT, concat!("bonsai/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .map_err(|_| ReleaseError::Network)?
        .error_for_status()
        .map_err(|_| ReleaseError::Network)?;
    bounded_body(response, limit).await
}

async fn bounded_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ReleaseError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ReleaseError::InvalidMetadata);
    }
    let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ReleaseError::Network)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(ReleaseError::InvalidMetadata);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let Some(start) = bytes.iter().position(|byte| !byte.is_ascii_whitespace()) else {
        return &[];
    };
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn verify_signature(
    public_key: &[u8; 32],
    message: &[u8],
    signature: &[u8],
) -> Result<(), ReleaseError> {
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message, signature)
        .map_err(|_| ReleaseError::InvalidSignature)
}

fn validate_manifest(manifest: &ReleaseManifest, expected_tag: &str) -> Result<(), ReleaseError> {
    if manifest.schema_version != 1
        || manifest.repository != RELEASE_REPOSITORY
        || manifest.tag != expected_tag
        || manifest.tag != format!("v{}", manifest.version)
        || Version::parse(&manifest.version).is_err()
        || manifest.assets.is_empty()
    {
        return Err(ReleaseError::InvalidMetadata);
    }
    let mut targets = BTreeSet::new();
    let mut archives = BTreeSet::new();
    for asset in &manifest.assets {
        if asset.target.is_empty()
            || asset.archive != format!("bonsai-{}-{}.tar.gz", manifest.tag, asset.target)
            || !is_sha256(&asset.archive_sha256)
            || !is_sha256(&asset.binary_sha256)
            || !targets.insert(asset.target.as_str())
            || !archives.insert(asset.archive.as_str())
        {
            return Err(ReleaseError::InvalidMetadata);
        }
    }
    Ok(())
}

async fn verify_installed_binary(manifest: &ReleaseManifest) -> Result<(), ReleaseError> {
    let target = target_triple().ok_or(ReleaseError::Executable)?;
    let expected = manifest
        .assets
        .iter()
        .find(|asset| asset.target == target)
        .map(|asset| asset.binary_sha256.as_str())
        .ok_or(ReleaseError::Executable)?;
    let executable = std::env::current_exe().map_err(|_| ReleaseError::Executable)?;
    let actual = sha256_file(&executable).await?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(ReleaseError::Executable)
    }
}

pub(crate) async fn sha256_file(path: &Path) -> Result<String, ReleaseError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ReleaseError::Executable)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|_| ReleaseError::Executable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn release_version(release: &GitHubRelease) -> Option<Version> {
    Version::parse(release.tag_name.strip_prefix('v')?).ok()
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};

    use super::*;

    fn manifest_json(repository: &str) -> Vec<u8> {
        format!(
            r#"{{"schema_version":1,"repository":"{repository}","version":"1.2.3","tag":"v1.2.3","assets":[{{"target":"x86_64-unknown-linux-gnu","archive":"bonsai-v1.2.3-x86_64-unknown-linux-gnu.tar.gz","archive_sha256":"{}","binary_sha256":"{}"}}]}}"#,
            "a".repeat(64),
            "b".repeat(64)
        )
        .into_bytes()
    }

    #[test]
    fn signed_manifest_accepts_exact_bytes_and_rejects_tampering() {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[7_u8; 32]).unwrap();
        let manifest = manifest_json(RELEASE_REPOSITORY);
        let signature = pair.sign(&manifest);
        let public_key: [u8; 32] = pair.public_key().as_ref().try_into().unwrap();

        verify_signature(&public_key, &manifest, signature.as_ref()).unwrap();
        let mut tampered = manifest.clone();
        tampered.push(b' ');
        assert!(verify_signature(&public_key, &tampered, signature.as_ref()).is_err());
    }

    #[test]
    fn manifest_validation_binds_repository_tag_assets_and_hashes() {
        let valid: ReleaseManifest =
            serde_json::from_slice(&manifest_json(RELEASE_REPOSITORY)).unwrap();
        validate_manifest(&valid, "v1.2.3").unwrap();

        let wrong_repository: ReleaseManifest =
            serde_json::from_slice(&manifest_json("attacker/example")).unwrap();
        assert!(validate_manifest(&wrong_repository, "v1.2.3").is_err());
        assert!(validate_manifest(&valid, "v9.9.9").is_err());

        let mut duplicate: ReleaseManifest =
            serde_json::from_slice(&manifest_json(RELEASE_REPOSITORY)).unwrap();
        let second: ReleaseManifest =
            serde_json::from_slice(&manifest_json(RELEASE_REPOSITORY)).unwrap();
        duplicate.assets.extend(second.assets);
        assert!(validate_manifest(&duplicate, "v1.2.3").is_err());

        let mut uppercase_hash: ReleaseManifest =
            serde_json::from_slice(&manifest_json(RELEASE_REPOSITORY)).unwrap();
        uppercase_hash.assets[0].archive_sha256 = "A".repeat(64);
        assert!(validate_manifest(&uppercase_hash, "v1.2.3").is_err());
    }

    #[test]
    fn embedded_public_key_wire_format_is_raw_ed25519_base64() {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[3_u8; 32]).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref());
        let decoded: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(decoded.as_slice(), pair.public_key().as_ref());
    }

    #[test]
    fn asset_urls_are_pinned_to_this_repository() {
        let release = GitHubRelease {
            tag_name: "v1.2.3".to_string(),
            draft: false,
            assets: vec![GitHubAsset {
                name: MANIFEST_ASSET.to_string(),
                browser_download_url: format!(
                    "https://github.com/{RELEASE_REPOSITORY}/releases/download/v1.2.3/{MANIFEST_ASSET}"
                ),
            }],
        };
        assert!(release_asset_url(&release, MANIFEST_ASSET).is_ok());

        let duplicate = GitHubRelease {
            tag_name: "v1.2.3".to_string(),
            draft: false,
            assets: vec![
                GitHubAsset {
                    name: MANIFEST_ASSET.to_string(),
                    browser_download_url: format!(
                        "https://github.com/{RELEASE_REPOSITORY}/releases/download/v1.2.3/{MANIFEST_ASSET}"
                    ),
                },
                GitHubAsset {
                    name: MANIFEST_ASSET.to_string(),
                    browser_download_url: format!(
                        "https://github.com/{RELEASE_REPOSITORY}/releases/download/v1.2.3/copy-{MANIFEST_ASSET}"
                    ),
                },
            ],
        };
        assert!(release_asset_url(&duplicate, MANIFEST_ASSET).is_err());

        let attacker = GitHubRelease {
            tag_name: "v1.2.3".to_string(),
            draft: false,
            assets: vec![GitHubAsset {
                name: MANIFEST_ASSET.to_string(),
                browser_download_url:
                    "https://example.test/strozynskiw/bonsai/releases/download/v1.2.3/release-manifest.json"
                        .to_string(),
            }],
        };
        assert!(release_asset_url(&attacker, MANIFEST_ASSET).is_err());
    }
}
