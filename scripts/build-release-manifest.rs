//! Build the canonical signed-release manifest from release artifacts.
//!
//! This is deliberately a standalone, std-only `rustc` tool so publishing a
//! release does not depend on Python, jq, or the Bonsai application graph.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
struct Args {
    tag: String,
    repository: String,
    dist: PathBuf,
    output: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct Asset {
    target: String,
    archive: String,
    archive_sha256: String,
    binary_sha256: String,
}

fn main() -> ExitCode {
    match run(env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("release manifest: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let args = parse_args(args)?;
    let version = release_version(&args.tag)?;
    validate_repository(&args.repository)?;
    let assets = collect_assets(&args.dist, &args.tag)?;
    let manifest = render_manifest(&args.repository, &args.tag, version, &assets);
    fs::write(&args.output, manifest)
        .map_err(|error| format!("could not write {}: {error}", args.output.display()))
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut tag = None;
    let mut repository = None;
    let mut dist = None;
    let mut output = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        let slot = match flag.as_str() {
            "--tag" => &mut tag,
            "--repository" => &mut repository,
            "--dist" => &mut dist,
            "--output" => &mut output,
            _ => return Err(format!("unknown argument: {flag}")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("{flag} was provided more than once"));
        }
    }
    Ok(Args {
        tag: tag.ok_or("--tag is required")?,
        repository: repository.ok_or("--repository is required")?,
        dist: PathBuf::from(dist.ok_or("--dist is required")?),
        output: PathBuf::from(output.ok_or("--output is required")?),
    })
}

fn release_version(tag: &str) -> Result<&str, String> {
    let version = tag
        .strip_prefix('v')
        .ok_or_else(|| format!("invalid release tag: {tag}"))?;
    let (version_without_build, build) = version
        .split_once('+')
        .map_or((version, None), |(left, right)| (left, Some(right)));
    let (core, prerelease) = version_without_build
        .split_once('-')
        .map_or((version_without_build, None), |(left, right)| {
            (left, Some(right))
        });
    let numbers = core.split('.').collect::<Vec<_>>();
    let valid_core = numbers.len() == 3 && numbers.iter().all(|part| valid_number(part));
    let valid_prerelease = prerelease.is_none_or(|value| valid_identifiers(value, true));
    let valid_build = build.is_none_or(|value| valid_identifiers(value, false));
    if valid_core && valid_prerelease && valid_build {
        Ok(version)
    } else {
        Err(format!("invalid release tag: {tag}"))
    }
}

fn valid_number(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn valid_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !(reject_numeric_leading_zero
                    && identifier.len() > 1
                    && identifier.starts_with('0')
                    && identifier.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

fn validate_repository(repository: &str) -> Result<(), String> {
    let mut parts = repository.split('/');
    let valid = parts.by_ref().take(2).all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }) && parts.next().is_none()
        && repository.contains('/');
    if valid {
        Ok(())
    } else {
        Err(format!("invalid repository: {repository}"))
    }
}

fn collect_assets(dist: &Path, tag: &str) -> Result<Vec<Asset>, String> {
    let prefix = format!("bonsai-{tag}-");
    let mut archives = fs::read_dir(dist)
        .map_err(|error| format!("could not read {}: {error}", dist.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".tar.gz"))
        })
        .collect::<Vec<_>>();
    archives.sort();
    if archives.is_empty() {
        return Err("no release archives found".to_string());
    }

    archives
        .into_iter()
        .map(|path| {
            let archive = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("release archive name is not UTF-8")?
                .to_string();
            let target = archive
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".tar.gz"))
                .filter(|target| {
                    !target.is_empty()
                        && target.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                        })
                })
                .ok_or_else(|| format!("invalid release target in {archive}"))?
                .to_string();
            let archive_sha256 = checksum(
                &path.with_file_name(format!("{archive}.sha256")),
                Some(&archive),
            )?;
            let binary_sha256 = checksum(
                &path.with_file_name(format!("{archive}.binary-sha256")),
                None,
            )?;
            Ok(Asset {
                target,
                archive,
                archive_sha256,
                binary_sha256,
            })
        })
        .collect()
}

fn checksum(path: &Path, expected_name: Option<&str>) -> Result<String, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut fields = content.split_whitespace();
    let digest = fields
        .next()
        .filter(|value| is_sha256(value))
        .ok_or_else(|| format!("invalid SHA-256 sidecar: {}", path.display()))?;
    if let Some(expected_name) = expected_name
        && fields.next() != Some(expected_name)
    {
        return Err(format!(
            "checksum sidecar does not name {expected_name}: {}",
            path.display()
        ));
    }
    Ok(digest.to_string())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn render_manifest(repository: &str, tag: &str, version: &str, assets: &[Asset]) -> String {
    let mut json = String::from("{\"assets\":[");
    for (index, asset) in assets.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        json.push_str("{\"archive\":\"");
        json.push_str(&json_string(&asset.archive));
        json.push_str("\",\"archive_sha256\":\"");
        json.push_str(&asset.archive_sha256);
        json.push_str("\",\"binary_sha256\":\"");
        json.push_str(&asset.binary_sha256);
        json.push_str("\",\"target\":\"");
        json.push_str(&json_string(&asset.target));
        json.push_str("\"}");
    }
    json.push_str("],\"repository\":\"");
    json.push_str(&json_string(repository));
    json.push_str("\",\"schema_version\":1,\"tag\":\"");
    json.push_str(&json_string(tag));
    json.push_str("\",\"version\":\"");
    json.push_str(&json_string(version));
    json.push_str("\"}\n");
    json
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_release_versions_and_rejects_ambiguous_tags() {
        assert_eq!(release_version("v1.2.3"), Ok("1.2.3"));
        assert_eq!(
            release_version("v1.2.3-rc.1+build.2"),
            Ok("1.2.3-rc.1+build.2")
        );
        assert!(release_version("1.2.3").is_err());
        assert!(release_version("v1.2").is_err());
        assert!(release_version("v1.2.3/asset").is_err());
        assert!(release_version("v1.2.3-").is_err());
        assert!(release_version("v1.2.3+").is_err());
        assert!(release_version("v01.2.3").is_err());
        assert!(release_version("v1.2.3-01").is_err());
    }

    #[test]
    fn renders_canonical_json_and_escapes_strings() {
        let assets = [Asset {
            target: "x86_64-unknown-linux-gnu".to_string(),
            archive: "bonsai-v1.2.3-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            archive_sha256: "a".repeat(64),
            binary_sha256: "b".repeat(64),
        }];
        let json = render_manifest("owner/repo", "v1.2.3", "1.2.3", &assets);
        assert!(json.starts_with("{\"assets\":[{"));
        assert!(json.contains("\"repository\":\"owner/repo\""));
        assert!(json.ends_with("\"version\":\"1.2.3\"}\n"));
        assert_eq!(json_string("a\"b\\c\n"), "a\\\"b\\\\c\\n");
    }

    #[test]
    fn collects_archive_and_binary_checksums() {
        let root = std::env::temp_dir().join(format!(
            "bonsai-release-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let archive = "bonsai-v1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        fs::write(root.join(archive), []).unwrap();
        fs::write(
            root.join(format!("{archive}.sha256")),
            format!("{}  {archive}\n", "a".repeat(64)),
        )
        .unwrap();
        fs::write(
            root.join(format!("{archive}.binary-sha256")),
            format!("{}\n", "b".repeat(64)),
        )
        .unwrap();

        let assets = collect_assets(&root, "v1.2.3").unwrap();

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].archive_sha256, "a".repeat(64));
        assert_eq!(assets[0].binary_sha256, "b".repeat(64));
        fs::remove_dir_all(root).unwrap();
    }
}
