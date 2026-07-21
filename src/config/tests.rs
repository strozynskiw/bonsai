use super::*;

struct Layers {
    project_root: tempfile::TempDir,
    bonsai_home: tempfile::TempDir,
}

impl Layers {
    fn new() -> Self {
        Self {
            project_root: tempfile::tempdir().unwrap(),
            bonsai_home: tempfile::tempdir().unwrap(),
        }
    }

    fn write_global(&self, contents: impl AsRef<str>) {
        std::fs::write(
            self.bonsai_home.path().join("config.toml"),
            contents.as_ref(),
        )
        .unwrap();
    }

    fn write_project(&self, contents: impl AsRef<str>) {
        let dir = self.project_root.path().join(".bonsai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents.as_ref()).unwrap();
    }

    /// `load()`, without touching the real `BONSAI_CONFIG` process env var —
    /// see [`Self::load_with_env_override`] for why.
    fn load(&self) -> Config {
        self.load_with_env_override(None)
    }

    /// [`load_with_env_override`](super::load_with_env_override) against
    /// these layers. Exercises the `BONSAI_CONFIG` override deterministically
    /// instead of mutating the real (process-global, so cross-test-racy) env
    /// var.
    fn load_with_env_override(&self, env_override: Option<std::path::PathBuf>) -> Config {
        load_with_env_override(
            self.project_root.path(),
            self.bonsai_home.path(),
            env_override,
        )
    }
}

#[test]
fn project_layer_overrides_global_scalar() {
    let layers = Layers::new();
    layers.write_global("schema_version = 1\n[sandbox]\ndeny_network = false\n");
    layers.write_project("schema_version = 1\n[sandbox]\ndeny_network = true\n");

    let config = layers.load();

    assert_eq!(config.sandbox.deny_network, Some(true));
}

#[test]
fn global_scalar_applies_without_a_project_override() {
    let layers = Layers::new();
    layers.write_global("schema_version = 1\n[sandbox]\ndeny_network = true\n");

    let config = layers.load();

    assert_eq!(config.sandbox.deny_network, Some(true));
}

#[test]
fn malformed_project_layer_is_diagnostic_without_discarding_global_config() {
    let layers = Layers::new();
    layers.write_global("schema_version = 1\n[sandbox]\ndeny_network = true\n");
    layers.write_project("schema_version = 1\n[sandbox\ndeny_network = false\n");

    let config = layers.load();

    assert_eq!(config.sandbox.deny_network, Some(true));
    let diagnostic = config
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.source == ConfigSource::Project && diagnostic.scope == "<file>"
        })
        .expect("malformed project layer should emit a file diagnostic");
    assert!(diagnostic.path.ends_with(".bonsai/config.toml"));
    assert!(diagnostic.message.contains("failed to parse TOML"));
}

#[test]
fn update_defaults_to_auto_with_no_pin() {
    let layers = Layers::new();

    let config = layers.load();

    assert_eq!(config.update.mode, UpdateMode::Auto);
    assert_eq!(config.update.pin, None);
}

#[test]
fn update_project_layer_overrides_global_scalars() {
    let layers = Layers::new();
    layers.write_global("schema_version = 1\n[update]\nmode = \"off\"\npin = \"0.2.0\"\n");
    layers.write_project("schema_version = 1\n[update]\nmode = \"notify\"\n");

    let config = layers.load();

    assert_eq!(config.update.mode, UpdateMode::Notify);
    // The pin scalar is independent: only the global layer set one.
    assert_eq!(config.update.pin, Some(semver::Version::new(0, 2, 0)));
}

#[test]
fn update_invalid_values_degrade_to_diagnostics() {
    let layers = Layers::new();
    layers.write_global("schema_version = 1\n[update]\nmode = \"sometimes\"\n");
    layers.write_project("schema_version = 1\n[update]\npin = \"latest\"\n");

    let config = layers.load();

    // Bad mode drops the whole malformed section to defaults; bad pin is
    // dropped field-wise. Both leave a diagnostic naming the section.
    assert_eq!(config.update.mode, UpdateMode::Auto);
    assert_eq!(config.update.pin, None);
    assert!(config.diagnostics.iter().any(
        |diagnostic| diagnostic.source == ConfigSource::Global && diagnostic.scope == "update"
    ));
    let pin_diagnostic = config
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.source == ConfigSource::Project && diagnostic.scope == "update"
        })
        .expect("invalid pin should emit a diagnostic");
    assert!(pin_diagnostic.message.contains("semver"));
}

#[test]
fn verification_is_off_without_an_explicit_policy() {
    let layers = Layers::new();

    let config = layers.load();

    assert_eq!(
        config.verification.after_edit,
        crate::verification::VerifyAfterEdit::Off
    );
}

#[test]
fn project_verification_profile_overrides_lanes_independently() {
    let layers = Layers::new();
    layers.write_global(
        r#"
        schema_version = 1
        [verification]
        test = ["cargo test --workspace"]
        build = ["cargo build --release"]
        after_edit = "ask"
        "#,
    );
    layers.write_project(
        r#"
        schema_version = 1
        [verification]
        test = ["cargo test --locked unit", "cargo test --locked integration"]
        after_edit = "on"
        "#,
    );

    let config = layers.load();

    assert_eq!(
        config.verification.test.as_deref(),
        Some(
            [
                "cargo test --locked unit".to_string(),
                "cargo test --locked integration".to_string(),
            ]
            .as_slice()
        )
    );
    assert_eq!(
        config.verification.build.as_deref(),
        Some(["cargo build --release".to_string()].as_slice())
    );
    assert_eq!(
        config.verification.after_edit,
        crate::verification::VerifyAfterEdit::On
    );
    assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);
}

#[test]
fn invalid_verification_policy_is_diagnostic_and_defaults_safely() {
    let layers = Layers::new();
    layers.write_project("schema_version = 1\n[verification]\nafter_edit = \"always\"\n");

    let config = layers.load();

    assert_eq!(
        config.verification.after_edit,
        crate::verification::VerifyAfterEdit::Off
    );
    assert!(
        config
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.scope == "verification")
    );
}

#[test]
fn restricted_workspace_ignores_project_config_but_keeps_global_config() {
    let layers = Layers::new();
    layers.write_global(
        r#"
        schema_version = 1
        [sandbox]
        deny_network = true

        [mcp.servers.global_docs]
        transport = "http"
        url = "https://docs.example.test/mcp"
        "#,
    );
    layers.write_project(
        r#"
        schema_version = 1
        [sandbox]
        deny_network = false

        [mcp.servers.project_exec]
        transport = "stdio"
        command = "should-not-spawn"
        "#,
    );

    let config = load_with_env_override_and_project_trust(
        layers.project_root.path(),
        layers.bonsai_home.path(),
        None,
        false,
    );

    assert_eq!(config.sandbox.deny_network, Some(true));
    assert!(config.mcp_servers.contains_key("global_docs"));
    assert!(!config.mcp_servers.contains_key("project_exec"));
}

#[test]
fn legacy_agent_read_isolation_is_accepted_and_ignored() {
    let layers = Layers::new();
    layers.write_project("schema_version = 1\nagent_read_isolation = \"fast\"\n");

    let config = layers.load();

    assert!(
        config
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.to_string().contains("agent_read_isolation")),
        "{:?}",
        config.diagnostics
    );
}

#[test]
fn mcp_server_entry_replaces_global_entry_wholesale() {
    let layers = Layers::new();
    layers.write_global(
        r#"
        schema_version = 1
        [mcp.servers.github]
        transport = "stdio"
        command = "global-cmd"
        allow_tools = ["a", "b"]
        "#,
    );
    layers.write_project(
        r#"
        schema_version = 1
        [mcp.servers.github]
        transport = "stdio"
        command = "project-cmd"
        "#,
    );

    let config = layers.load();

    let (server, source) = &config.mcp_servers["github"];
    assert_eq!(*source, ConfigSource::Project);
    match &server.transport {
        McpTransportConfig::Stdio { command, .. } => assert_eq!(command, "project-cmd"),
        other => panic!("expected stdio transport, got {other:?}"),
    }
    // Wholesale replace: the project entry didn't repeat `allow_tools`, so it
    // must NOT inherit the global entry's value.
    assert!(server.allow_tools.is_empty());
}

#[test]
fn hooks_concatenate_global_then_project() {
    let layers = Layers::new();
    layers.write_global(hook_toml("global-hook", "PostBash", "echo global"));
    layers.write_project(hook_toml("project-hook", "PreBash", "echo project"));

    let config = layers.load();

    let names: Vec<&str> = config.hooks.iter().map(|(h, _)| h.name.as_str()).collect();
    assert_eq!(names, vec!["global-hook", "project-hook"]);
    assert_eq!(config.hooks[0].1, ConfigSource::Global);
    assert_eq!(config.hooks[1].1, ConfigSource::Project);
}

#[test]
fn hooks_same_name_project_shadows_global() {
    let layers = Layers::new();
    layers.write_global(hook_toml("cargo-fmt", "PostFileWrite", "echo global"));
    layers.write_project(
        r#"
        schema_version = 1
        [[hooks]]
        name = "cargo-fmt"
        event = "PostFileWrite"
        enabled = false
        action = { type = "shell", command = "echo project" }
        "#,
    );

    let config = layers.load();

    // A same-named project hook shadows the global one in place, rather than
    // both surviving into the merged list.
    let cargo_fmt_hooks: Vec<_> = config
        .hooks
        .iter()
        .filter(|(h, _)| h.name == "cargo-fmt")
        .collect();
    assert_eq!(cargo_fmt_hooks.len(), 1, "{:?}", config.hooks);
    assert_eq!(cargo_fmt_hooks[0].1, ConfigSource::Project);
    assert!(!cargo_fmt_hooks[0].0.enabled);
}

fn hook_toml(name: &str, event: &str, command: &str) -> String {
    format!(
        r#"
        schema_version = 1
        [[hooks]]
        name = "{name}"
        event = "{event}"
        action = {{ type = "shell", command = "{command}" }}
        "#
    )
}

#[test]
fn env_config_path_replaces_project_layer() {
    let layers = Layers::new();
    layers.write_project(hook_toml("project-hook", "PreBash", "echo project"));

    let override_dir = tempfile::tempdir().unwrap();
    let override_path = override_dir.path().join("override.toml");
    std::fs::write(
        &override_path,
        hook_toml("override-hook", "PreBash", "echo override"),
    )
    .unwrap();

    let config = layers.load_with_env_override(Some(override_path.clone()));

    let names: Vec<&str> = config.hooks.iter().map(|(h, _)| h.name.as_str()).collect();
    assert_eq!(names, vec!["override-hook"]);
    assert_eq!(config.hooks[0].1, ConfigSource::Env);
    assert_eq!(config.layers.project_path_source, ConfigSource::Env);
    assert_eq!(config.layers.project_path, override_path);
}

#[test]
fn invalid_server_entry_yields_diagnostic_naming_extension_and_field() {
    let layers = Layers::new();
    layers.write_project(
        r#"
        schema_version = 1
        [mcp.servers.context7]
        transport = "http"
        "#,
    );

    let config = layers.load();

    assert!(config.mcp_servers.is_empty());
    let diagnostic = config
        .diagnostics
        .iter()
        .find(|d| d.scope == "mcp.servers.context7")
        .unwrap_or_else(|| {
            panic!(
                "no diagnostic for mcp.servers.context7: {:?}",
                config.diagnostics
            )
        });
    assert!(
        diagnostic.message.contains("url"),
        "diagnostic should name the missing field: {}",
        diagnostic.message
    );
}

#[test]
fn invalid_hook_name_yields_diagnostic_and_is_excluded() {
    let layers = Layers::new();
    layers.write_project(
        r#"
        schema_version = 1
        [[hooks]]
        name = "bad.name"
        event = "PostFileWrite"
        action = { type = "shell", command = "echo hi" }
        "#,
    );

    let config = layers.load();

    assert!(config.hooks.is_empty(), "{:?}", config.hooks);
    let diagnostic = config
        .diagnostics
        .iter()
        .find(|d| d.scope.contains("bad.name"))
        .unwrap_or_else(|| {
            panic!(
                "no diagnostic for the bad hook name: {:?}",
                config.diagnostics
            )
        });
    assert!(
        diagnostic.message.contains("name"),
        "{}",
        diagnostic.message
    );
}

#[test]
fn empty_hook_command_yields_diagnostic_and_is_excluded() {
    let layers = Layers::new();
    layers.write_project(
        r#"
        schema_version = 1
        [[hooks]]
        name = "protect-dotenv"
        event = "PreFileWrite"
        action = { type = "shell", command = "" }
        "#,
    );

    let config = layers.load();

    assert!(config.hooks.is_empty(), "{:?}", config.hooks);
    let diagnostic = config
        .diagnostics
        .iter()
        .find(|d| d.scope.contains("protect-dotenv"))
        .unwrap_or_else(|| {
            panic!(
                "no diagnostic for the empty command: {:?}",
                config.diagnostics
            )
        });
    assert!(
        diagnostic.message.contains("command"),
        "{}",
        diagnostic.message
    );
}

#[test]
fn newer_schema_version_warns_but_loads() {
    let layers = Layers::new();
    layers.write_project(
        r#"
        schema_version = 999
        [sandbox]
        deny_network = true
        "#,
    );

    let config = layers.load();

    assert_eq!(config.sandbox.deny_network, Some(true));
    assert!(
        config
            .diagnostics
            .iter()
            .any(|d| d.scope == "schema_version" && d.message.contains("999")),
        "{:?}",
        config.diagnostics
    );
}

#[test]
fn legacy_global_config_is_skipped_with_migration_hint() {
    let layers = Layers::new();
    layers.write_global(
        r#"
        api_key = "sk-legacy"
        model = "some-model"
        "#,
    );
    layers.write_project(
        r#"
        schema_version = 1
        [mcp.servers.filesystem]
        transport = "stdio"
        command = "npx"
        "#,
    );

    let config = layers.load();

    // The legacy global layer contributed nothing to the merged view...
    assert!(config.hooks.is_empty());
    // ...while the project layer, parsed normally, still applies.
    assert!(config.mcp_servers.contains_key("filesystem"));
    assert!(
        config
            .diagnostics
            .iter()
            .any(|d| d.source == ConfigSource::Global && d.message.contains("legacy")),
        "{:?}",
        config.diagnostics
    );
}

#[test]
fn missing_files_load_silently_with_no_diagnostics() {
    let layers = Layers::new();

    let config = layers.load();

    assert!(config.diagnostics.is_empty());
    assert!(config.mcp_servers.is_empty());
    assert!(config.hooks.is_empty());
}

#[test]
fn unknown_top_level_key_warns_without_failing_the_file() {
    let layers = Layers::new();
    layers.write_project(
        r#"
        schema_version = 1
        totally_unknown = true
        [skills]
        enabled = true
        [sandbox]
        deny_network = true
        "#,
    );

    let config = layers.load();

    assert_eq!(config.sandbox.deny_network, Some(true));
    assert!(
        config
            .diagnostics
            .iter()
            .any(|d| d.scope == "[totally_unknown]")
    );
    assert!(
        config
            .diagnostics
            .iter()
            .any(|d| d.scope == "[skills]" && d.message.contains("not yet supported"))
    );
}

#[test]
fn validate_mcp_server_rejects_blank_required_fields() {
    let stdio = McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "   ".to_string(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
        },
        enabled: true,
        allow_tools: Vec::new(),
        declared: Default::default(),
        timeout_secs: 30,
    };
    assert!(validate::validate_mcp_server(&stdio).is_err());

    let blank_client = McpServerConfig {
        transport: McpTransportConfig::Http {
            url: "https://mcp.example/mcp".to_string(),
            headers: Default::default(),
            oauth_client_id: Some("  ".to_string()),
            oauth_scopes: Vec::new(),
        },
        enabled: true,
        allow_tools: Vec::new(),
        declared: Default::default(),
        timeout_secs: 30,
    };
    assert!(validate::validate_mcp_server(&blank_client).is_err());

    let blank_scope = McpServerConfig {
        transport: McpTransportConfig::Http {
            url: "https://mcp.example/mcp".to_string(),
            headers: Default::default(),
            oauth_client_id: None,
            oauth_scopes: vec!["".to_string()],
        },
        ..blank_client
    };
    assert!(validate::validate_mcp_server(&blank_scope).is_err());
}
