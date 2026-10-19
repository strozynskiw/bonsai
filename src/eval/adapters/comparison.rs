//! Offline TB4 provenance validation shared by the result-import boundary.

use std::collections::HashSet;

use anyhow::{Result, ensure};
use serde_json::Value;

fn fields(value: &Value, names: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Expected provenance object"))?;
    ensure!(
        object.len() == names.len() && names.iter().all(|name| object.contains_key(*name)),
        "Unexpected provenance fields"
    );
    Ok(())
}

fn text(value: &Value) -> Result<&str> {
    let text = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Expected provenance text"))?;
    ensure!(!text.trim().is_empty(), "Empty provenance text");
    Ok(text)
}

fn content_digest(value: &Value) -> Result<()> {
    let digest = text(value)?
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("Expected SHA256 provenance"))?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Invalid SHA256 provenance"
    );
    Ok(())
}

fn task_id(value: &Value) -> Result<&str> {
    let id = text(value)?;
    ensure!(
        id.len() <= 200
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "Unsafe task identity"
    );
    Ok(id)
}

/// Validate the entire frozen preparation schema and bind a pilot task.
///
/// # Errors
/// Rejects missing/unknown fields, invalid pins, resources, content identities,
/// duplicate tasks, and task results outside the frozen scored set.
pub(super) fn validate_manifest(manifest: &Value, task: &str) -> Result<()> {
    // Execution bindings are digest-covered local-runner inputs, not execution
    // permission or score qualification. Legacy offline manifests omit them.
    let mut names = vec![
        "schema_version",
        "dataset",
        "harbor_commit",
        "dataset_digest",
        "dependencies_digest",
        "node",
        "artifacts",
        "profiles",
        "model",
        "resources",
        "tasks",
        "selection",
        "exclusions",
    ];
    if let Some(execution) = manifest.get("execution") {
        ensure!(execution.is_object(), "Expected execution bindings object");
        names.push("execution");
    }
    fields(manifest, &names)?;
    ensure!(
        manifest["schema_version"] == 1
            && manifest["dataset"] == "terminal-bench/terminal-bench@4.0.0"
            && manifest["harbor_commit"] == super::HARBOR_TB4_HARNESS_COMMIT,
        "Unsupported comparison identity"
    );
    for name in ["dataset_digest", "dependencies_digest"] {
        content_digest(&manifest[name])?;
    }
    fields(&manifest["node"], &["version", "artifact_digest"])?;
    text(&manifest["node"]["version"])?;
    content_digest(&manifest["node"]["artifact_digest"])?;
    fields(&manifest["artifacts"], &["bonsai", "dsh"])?;
    fields(&manifest["profiles"], &["bonsai", "dsh"])?;
    for name in ["bonsai", "dsh"] {
        let artifact = &manifest["artifacts"][name];
        fields(
            artifact,
            &[
                "revision",
                "source_digest",
                "dirty",
                "artifact_digest",
                "version",
            ],
        )?;
        text(&artifact["revision"])?;
        text(&artifact["version"])?;
        content_digest(&artifact["source_digest"])?;
        content_digest(&artifact["artifact_digest"])?;
        ensure!(
            artifact["dirty"].is_boolean(),
            "Dirty provenance must be boolean"
        );
        if name == "dsh" {
            ensure!(
                artifact["revision"] == "687f282e8625c6b8d11ff58211a90524be2e4317"
                    && artifact["dirty"] == false,
                "DSH source must be clean and pinned"
            );
        }
        let profile = &manifest["profiles"][name];
        fields(profile, &["overlay_digest", "native_limits"])?;
        content_digest(&profile["overlay_digest"])?;
        text(&profile["native_limits"])?;
    }
    let model = &manifest["model"];
    fields(
        model,
        &[
            "id",
            "endpoint",
            "reasoning",
            "alias_limitation",
            "fallback",
        ],
    )?;
    text(&model["id"])?;
    ensure!(
        matches!(text(&model["reasoning"])?, "off" | "low" | "high" | "max")
            && model["fallback"] == false
            && model["alias_limitation"].is_string(),
        "Unsupported shared model settings"
    );
    let endpoint = reqwest::Url::parse(text(&model["endpoint"])?)?;
    ensure!(
        endpoint.scheme() == "https"
            && endpoint.host_str().is_some()
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none(),
        "Endpoint must be credential-free HTTPS"
    );
    let resources = &manifest["resources"];
    fields(
        resources,
        &[
            "cpu",
            "ram_mb",
            "disk_mb",
            "timeout_seconds",
            "network",
            "working_directory",
            "confinement",
        ],
    )?;
    for name in ["cpu", "ram_mb", "disk_mb", "timeout_seconds"] {
        ensure!(
            resources[name].as_u64().is_some_and(|n| n > 0),
            "Resources must be positive integers"
        );
    }
    for name in ["network", "working_directory", "confinement"] {
        text(&resources[name])?;
    }
    let selection = &manifest["selection"];
    fields(
        selection,
        &["seed", "rule", "inventory_digest", "repetitions"],
    )?;
    text(&selection["seed"])?;
    content_digest(&selection["inventory_digest"])?;
    ensure!(
        selection["rule"] == "sha256-seed-task-v1" && selection["repetitions"] == 3,
        "Unsupported selection rule"
    );
    let tasks = manifest["tasks"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Expected task inventory"))?;
    let mut seen = HashSet::new();
    let mut smoke = 0;
    let mut pilot = 0;
    let mut selected = false;
    for item in tasks {
        fields(
            item,
            &[
                "id",
                "content_digest",
                "resource_digest",
                "image_digest",
                "phase",
            ],
        )?;
        let id = task_id(&item["id"])?;
        ensure!(seen.insert(id), "Duplicate task identity");
        for name in ["content_digest", "resource_digest", "image_digest"] {
            content_digest(&item[name])?;
        }
        match text(&item["phase"])? {
            "smoke" => smoke += 1,
            "pilot" => {
                pilot += 1;
                selected |= id == task;
            }
            _ => anyhow::bail!("Unknown task phase"),
        }
    }
    ensure!(
        smoke == 3 && pilot == 10 && selected,
        "Requires 3 smoke/10 pilot tasks and selected pilot membership"
    );
    for item in manifest["exclusions"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Expected exclusions"))?
    {
        fields(item, &["id", "reason", "resource_digest"])?;
        ensure!(seen.insert(task_id(&item["id"])?), "Duplicate exclusion");
        text(&item["reason"])?;
        content_digest(&item["resource_digest"])?;
    }
    Ok(())
}
