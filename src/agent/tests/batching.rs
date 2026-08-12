use super::*;

#[test]
fn batches_read_only_calls_together() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-3", "grep", r#"{"pattern":"fn","path":"src"}"#),
    ];

    let batches = super::tool_call_batches(&calls, &empty_registry(), batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2", "call-3"]]
    );
}

#[test]
fn region_read_tools_are_path_scoped_reads() {
    let calls = vec![
        test_tool_call(
            "call-1",
            "read_region",
            r#"{"path":"src/main.rs","start_line":1,"end_line":20}"#,
        ),
        test_tool_call(
            "call-2",
            "read_symbol",
            r#"{"path":"src/main.rs","query":"main"}"#,
        ),
        test_tool_call("call-3", "write", r#"{"path":"src/main.rs","content":"x"}"#),
        test_tool_call(
            "call-4",
            "read_region",
            r#"{"path":"src/lib.rs","start_line":1,"end_line":20}"#,
        ),
    ];
    let registry = registry_with_policies(&[
        ("read_region", crate::tool::ParallelPolicy::PathScoped),
        ("read_symbol", crate::tool::ParallelPolicy::PathScoped),
        ("write", crate::tool::ParallelPolicy::PathScoped),
    ]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2", "call-4"], vec!["call-3"]]
    );
}

#[test]
fn project_info_batches_as_read_only_orientation() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "project_info", r#"{}"#),
        test_tool_call("call-3", "grep", r#"{"pattern":"fn","path":"src"}"#),
    ];
    let registry = registry_with_policies(&[
        ("project_info", crate::tool::ParallelPolicy::AlwaysSafe),
        ("read", crate::tool::ParallelPolicy::PathScoped),
        ("grep", crate::tool::ParallelPolicy::PathScoped),
    ]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2", "call-3"]]
    );
}

#[test]
fn websearch_is_independent_read_only_network_work() {
    let calls = vec![
        test_tool_call("call-1", "websearch", r#"{"query":"Rust docs"}"#),
        test_tool_call("call-2", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-3", "websearch", r#"{"query":"Tokio docs"}"#),
    ];
    let registry = registry_with_policies(&[
        ("websearch", crate::tool::ParallelPolicy::AlwaysSafe),
        ("read", crate::tool::ParallelPolicy::PathScoped),
    ]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2", "call-3"]]
    );
}

#[test]
fn read_only_agent_fan_out_runs_in_parallel() {
    // Three read-only research subagents spawned in one turn should batch
    // together (a whole-tree read each) instead of serializing.
    let calls = vec![
        test_tool_call("call-1", "agent", r#"{"agent":"explore","prompt":"a"}"#),
        test_tool_call("call-2", "agent", r#"{"agent":"research","prompt":"b"}"#),
        test_tool_call("call-3", "agent", r#"{"agent":"plan","prompt":"c"}"#),
    ];
    let registry = registry_with_agent_delegation(&["explore", "research", "plan"]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2", "call-3"]]
    );
}

#[test]
fn write_capable_agent_serializes_but_read_only_peers_still_batch() {
    // A write-capable subagent (`fixer`) serializes against everything; the two
    // read-only delegations around it still batch with each other.
    let calls = vec![
        test_tool_call("call-1", "agent", r#"{"agent":"explore","prompt":"a"}"#),
        test_tool_call("call-2", "agent", r#"{"agent":"research","prompt":"b"}"#),
        test_tool_call("call-3", "agent", r#"{"agent":"fixer","prompt":"c"}"#),
    ];
    let registry = registry_with_agent_delegation(&["explore", "research"]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2"], vec!["call-3"]]
    );
}

#[test]
fn read_only_agent_serializes_against_a_concurrent_write() {
    // A read-only delegation is a whole-tree read, so it must not run alongside
    // a real write to the tree in the same turn.
    let calls = vec![
        test_tool_call("call-1", "agent", r#"{"agent":"explore","prompt":"a"}"#),
        test_tool_call("call-2", "write", r#"{"path":"src/main.rs","content":"x"}"#),
    ];
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PolicyStub {
        name: "agent",
        policy: crate::tool::ParallelPolicy::Serialized,
        read_only_agents: Some(&["explore"]),
    }));
    registry.register(Arc::new(PolicyStub {
        name: "write",
        policy: crate::tool::ParallelPolicy::PathScoped,
        read_only_agents: None,
    }));

    let batches = super::tool_call_batches(&calls, &Arc::new(registry), batch_root());

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn agent_target_with_empty_read_only_list_serializes() {
    // An empty read-only list means every resolved target classifies as
    // write-capable (`delegation_is_read_only` → `Some(false)`), so the calls
    // serialize. This is the same path as test 5, but with an empty list —
    // the "resolved but not in the list" case collapses to write-capable.
    let calls = vec![
        test_tool_call("call-1", "agent", r#"{"agent":"explore","prompt":"a"}"#),
        test_tool_call("call-2", "agent", r#"{"agent":"explore","prompt":"b"}"#),
    ];
    let registry = registry_with_agent_delegation(&[]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn unresolvable_agent_target_serializes() {
    // When the agent name cannot be resolved, `delegation_is_read_only`
    // returns `None` (modeled by `read_only_agents: None` on the stub) and
    // the batcher's `unwrap_or(false)` routes the call to `GlobalWrite` —
    // preserving the conservative default for unknown targets.
    let calls = vec![
        test_tool_call("call-1", "agent", r#"{"agent":"explore","prompt":"a"}"#),
        test_tool_call("call-2", "agent", r#"{"agent":"explore","prompt":"b"}"#),
    ];
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PolicyStub {
        name: "agent",
        policy: crate::tool::ParallelPolicy::Serialized,
        read_only_agents: None,
    }));

    let batches = super::tool_call_batches(&calls, &Arc::new(registry), batch_root());

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn lsp_navigation_tools_are_path_scoped_reads() {
    let calls = vec![
        test_tool_call(
            "call-1",
            "definition",
            r#"{"path":"src/main.rs","line":1,"character":1}"#,
        ),
        test_tool_call(
            "call-2",
            "hover",
            r#"{"path":"src/main.rs","line":1,"character":1}"#,
        ),
        test_tool_call(
            "call-3",
            "references",
            r#"{"path":"src/lib.rs","line":1,"character":1}"#,
        ),
        test_tool_call("call-4", "write", r#"{"path":"src/main.rs","content":"x"}"#),
    ];
    let registry = registry_with_policies(&[
        ("definition", crate::tool::ParallelPolicy::PathScoped),
        ("hover", crate::tool::ParallelPolicy::PathScoped),
        ("references", crate::tool::ParallelPolicy::PathScoped),
        ("write", crate::tool::ParallelPolicy::PathScoped),
    ]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2", "call-3"], vec!["call-4"]]
    );
}

#[test]
fn rename_symbol_serializes_as_global_write() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call(
            "call-2",
            "rename_symbol",
            r#"{"path":"src/main.rs","line":1,"character":1,"new_name":"renamed"}"#,
        ),
        test_tool_call("call-3", "read", r#"{"path":"src/lib.rs"}"#),
    ];
    let registry = registry_with_policies(&[
        ("read", crate::tool::ParallelPolicy::PathScoped),
        ("rename_symbol", crate::tool::ParallelPolicy::Serialized),
    ]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]]
    );
}

#[test]
fn batches_overlapping_writes_after_prior_conflicts() {
    let calls = vec![
        test_tool_call("call-1", "write", r#"{"path":"src"}"#),
        test_tool_call("call-2", "read", r#"{"path":"tests/main.rs"}"#),
        test_tool_call("call-3", "edit", r#"{"path":"src/main.rs"}"#),
    ];

    let batches = super::tool_call_batches(&calls, &empty_registry(), batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2"], vec!["call-3"]]
    );
}

#[test]
fn batches_absolute_project_path_with_relative_alias() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let project_root = temp_dir.path().to_path_buf();
    let absolute_path = project_root.join("src/main.rs").display().to_string();
    let write_args = serde_json::json!({ "path": absolute_path }).to_string();
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "write", &write_args),
    ];

    let batches = super::tool_call_batches(&calls, &empty_registry(), &project_root);

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn yolo_serializes_write_edit_aliases_outside_project() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let project_root = temp_dir.path().join("project");
    let absolute_path = temp_dir.path().join("outside.txt").display().to_string();
    let edit_args = serde_json::json!({
        "path": absolute_path,
        "old_string": "one",
        "new_string": "two",
    })
    .to_string();
    let calls = vec![
        test_tool_call(
            "call-1",
            "write",
            r#"{"path":"../outside.txt","content":"one"}"#,
        ),
        test_tool_call("call-2", "edit", &edit_args),
    ];

    let batches = super::tool_call_batches_with_yolo(
        &calls,
        &empty_registry(),
        &project_root,
        true,
        &std::collections::HashSet::new(),
    );

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn batches_global_side_effects_without_reordering_later_calls_before_them() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "bash", r#"{"command":"cargo fmt"}"#),
        test_tool_call("call-3", "read", r#"{"path":"tests/main.rs"}"#),
    ];

    let registry = registry_with_policies(&[("bash", crate::tool::ParallelPolicy::Serialized)]);
    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]]
    );
}

#[test]
fn plan_tools_serialize_around_other_calls() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call(
            "call-2",
            "plan_replace_draft",
            r#"{"title":"Plan","sections":[],"tasks":["Do it"],"phases":[],"questions":[]}"#,
        ),
        test_tool_call(
            "call-3",
            "plan_insert_task",
            r#"{"text":"Write tests","position":"end"}"#,
        ),
        test_tool_call("call-4", "read", r#"{"path":"src/agent.rs"}"#),
    ];
    let registry = registry_with_policies(&[
        (
            "plan_replace_draft",
            crate::tool::ParallelPolicy::Serialized,
        ),
        ("plan_insert_task", crate::tool::ParallelPolicy::Serialized),
    ]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![
            vec!["call-1"],
            vec!["call-2"],
            vec!["call-3"],
            vec!["call-4"]
        ]
    );
}

#[test]
fn set_session_title_serializes_around_other_calls() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "set_session_title", r#"{"title":"Fix resume"}"#),
        test_tool_call("call-3", "read", r#"{"path":"src/agent.rs"}"#),
    ];
    let registry =
        registry_with_policies(&[("set_session_title", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]]
    );
}

#[test]
fn recall_serializes_around_other_calls() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "recall", r#"{"episode":1}"#),
        test_tool_call("call-3", "read", r#"{"path":"src/agent.rs"}"#),
    ];
    let registry = registry_with_policies(&[("recall", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]]
    );
}

fn batch_ids(batches: &[Vec<crate::provider::ToolCall>]) -> Vec<Vec<&str>> {
    batches
        .iter()
        .map(|batch| batch.iter().map(|call| call.id.as_str()).collect())
        .collect()
}

#[test]
fn bash_defaults_to_serialized() {
    // Two bash calls with no `parallel` flag go to separate batches.
    // `bash` is registered as `Serialized` (its real policy).
    let calls = vec![
        test_tool_call("call-1", "bash", r#"{"command":"echo a"}"#),
        test_tool_call("call-2", "bash", r#"{"command":"echo b"}"#),
    ];
    let registry = registry_with_policies(&[("bash", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"]],
        "bash without parallel:true should serialize"
    );
}

#[test]
fn parallel_bash_groups_into_one_batch() {
    // Two `bash{parallel: true}` calls run in the same batch.
    // The per-call override is checked before the policy lookup.
    let calls = vec![
        test_tool_call("call-1", "bash", r#"{"command":"echo a","parallel":true}"#),
        test_tool_call("call-2", "bash", r#"{"command":"echo b","parallel":true}"#),
    ];
    let registry = registry_with_policies(&[("bash", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2"]],
        "two parallel:true bash calls should share a batch"
    );
}

#[test]
fn background_bash_groups_like_parallel_bash() {
    let calls = vec![
        test_tool_call(
            "call-1",
            "bash",
            r#"{"command":"sleep 5","run_in_background":true}"#,
        ),
        test_tool_call(
            "call-2",
            "bash",
            r#"{"command":"sleep 5","run_in_background":true}"#,
        ),
    ];
    let registry = registry_with_policies(&[("bash", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(batch_ids(&batches), vec![vec!["call-1", "call-2"]]);
}

#[test]
fn tasks_tool_serializes_around_other_calls() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "tasks", r#"{"action":"wait","wait_seconds":1}"#),
        test_tool_call("call-3", "read", r#"{"path":"src/agent.rs"}"#),
    ];
    let registry = registry_with_policies(&[("tasks", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]]
    );
}

#[test]
fn terminal_tool_serializes_around_other_calls() {
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call(
            "call-2",
            "terminal",
            r#"{"action":"read","terminal_id":"pty-1"}"#,
        ),
        test_tool_call("call-3", "read", r#"{"path":"src/agent.rs"}"#),
    ];
    let registry = registry_with_policies(&[("terminal", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]]
    );
}

#[test]
fn unflagged_bash_does_not_batch_with_parallel_bash() {
    // A `parallel: true` call classifies as `ParallelBash`; an un-flagged bash
    // classifies as `GlobalWrite`. `ParallelBash` conflicts with `GlobalWrite`,
    // so the serialized (un-flagged) call splits the parallel calls into
    // separate batches instead of silently riding along with them.
    let calls = vec![
        test_tool_call("call-1", "bash", r#"{"command":"echo a","parallel":true}"#),
        test_tool_call("call-2", "bash", r#"{"command":"echo b"}"#),
        test_tool_call("call-3", "bash", r#"{"command":"echo c","parallel":true}"#),
    ];
    let registry = registry_with_policies(&[("bash", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]],
        "an un-flagged (serialized) bash must not share a batch with parallel:true calls"
    );

    // Sanity: two un-flagged calls still serialize (the GlobalWrite default).
    let two_serial = vec![
        test_tool_call("a", "bash", r#"{"command":"x"}"#),
        test_tool_call("b", "bash", r#"{"command":"y"}"#),
    ];
    let batches = super::tool_call_batches(&two_serial, &registry, batch_root());
    assert_eq!(batch_ids(&batches), vec![vec!["a"], vec!["b"]]);
}

#[test]
fn unknown_tool_serializes_against_other_calls() {
    // A tool name we don't recognize classifies as GlobalWrite and must not run
    // concurrently with anything — not even read-only calls.
    let calls = vec![
        test_tool_call("call-1", "read", r#"{"path":"src/main.rs"}"#),
        test_tool_call("call-2", "frobnicate", r#"{"whatever":1}"#),
        test_tool_call("call-3", "read", r#"{"path":"src/agent.rs"}"#),
    ];

    let batches = super::tool_call_batches(&calls, &empty_registry(), batch_root());

    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"], vec!["call-3"]],
        "an unknown tool must serialize, not batch alongside reads"
    );
}

#[test]
fn two_unknown_tools_do_not_batch() {
    let calls = vec![
        test_tool_call("call-1", "frobnicate", r#"{}"#),
        test_tool_call("call-2", "frobnicate", r#"{}"#),
    ];

    let batches = super::tool_call_batches(&calls, &empty_registry(), batch_root());

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn parallel_bash_false_keeps_serialized_default() {
    // `parallel: false` is the explicit opt-out — does NOT unlock
    // parallelism. Same as omitting the field.
    let calls = vec![
        test_tool_call("call-1", "bash", r#"{"command":"echo a","parallel":false}"#),
        test_tool_call("call-2", "bash", r#"{"command":"echo b","parallel":false}"#),
    ];
    let registry = registry_with_policies(&[("bash", crate::tool::ParallelPolicy::Serialized)]);

    let batches = super::tool_call_batches(&calls, &registry, batch_root());

    assert_eq!(batch_ids(&batches), vec![vec!["call-1"], vec!["call-2"]]);
}

#[test]
fn parallel_bash_batches_only_with_parallel_bash() {
    // A `parallel: true` bash carries no trustworthy path scope, so the only
    // safe concurrent class is another explicitly parallel/background bash.
    let registry = registry_with_policies(&[
        ("bash", crate::tool::ParallelPolicy::Serialized),
        ("tasks", crate::tool::ParallelPolicy::Serialized),
        ("todowrite", crate::tool::ParallelPolicy::Serialized),
        ("noop", crate::tool::ParallelPolicy::AlwaysSafe),
    ]);

    let calls = vec![
        test_tool_call(
            "call-1",
            "bash",
            r#"{"command":"echo hi > out.txt","parallel":true}"#,
        ),
        test_tool_call("call-2", "write", r#"{"path":"out.txt","content":"x"}"#),
    ];
    let batches = super::tool_call_batches(&calls, &registry, batch_root());
    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2"]],
        "a parallel:true bash must not share a batch with a write"
    );

    // A backgrounded bash splits from a write the same way, but still batches
    // with another parallel/background bash.
    let background = vec![
        test_tool_call(
            "call-1",
            "bash",
            r#"{"command":"touch built","run_in_background":true}"#,
        ),
        test_tool_call("call-2", "bash", r#"{"command":"sleep 5","parallel":true}"#),
        test_tool_call("call-3", "edit", r#"{"path":"built","old":"a","new":"b"}"#),
    ];
    let batches = super::tool_call_batches(&background, &registry, batch_root());
    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1", "call-2"], vec!["call-3"]],
        "a run_in_background bash must not share a batch with an edit"
    );

    let with_read_and_safe_tool = vec![
        test_tool_call("call-1", "bash", r#"{"command":"echo hi","parallel":true}"#),
        test_tool_call("call-2", "read", r#"{"path":"out.txt"}"#),
        test_tool_call("call-3", "noop", r#"{}"#),
    ];
    let batches = super::tool_call_batches(&with_read_and_safe_tool, &registry, batch_root());
    assert_eq!(
        batch_ids(&batches),
        vec![vec!["call-1"], vec!["call-2", "call-3"]],
        "a parallel:true bash must not share a batch with reads or always-safe tools"
    );

    let with_serialized_tools = vec![
        test_tool_call("call-1", "bash", r#"{"command":"echo hi","parallel":true}"#),
        test_tool_call("call-2", "tasks", r#"{"action":"list"}"#),
        test_tool_call(
            "call-3",
            "todowrite",
            r#"{"todos":[{"content":"x","status":"pending"}]}"#,
        ),
        test_tool_call(
            "call-4",
            "bash",
            r#"{"command":"echo bye","run_in_background":true}"#,
        ),
    ];
    let batches = super::tool_call_batches(&with_serialized_tools, &registry, batch_root());
    assert_eq!(
        batch_ids(&batches),
        vec![
            vec!["call-1"],
            vec!["call-2"],
            vec!["call-3"],
            vec!["call-4"]
        ],
        "serialized tools must split parallel bash batches"
    );
}

#[tokio::test]
async fn parallel_bash_runs_concurrently() {
    // End-to-end: two parallel-flagged bash calls go through the
    // full agent loop and run concurrently. The barrier pattern from
    // `independent_tool_calls_run_concurrently_and_are_awaited` is
    // reused; the two tools block on a 2-party barrier, so the loop
    // must run them in parallel for the timeout to pass.
    let fixture = TestFixture::new();
    let barrier = Arc::new(Barrier::new(2));
    let started = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BarrierTool::new(
        "bash",
        barrier.clone(),
        started.clone(),
    )));

    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![
                test_tool_call("call-1", "bash", r#"{"command":"sleep 1","parallel":true}"#),
                test_tool_call("call-2", "bash", r#"{"command":"sleep 1","parallel":true}"#),
            ],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        agent.run("hello", CancellationToken::new(), Arc::new(StdoutSink)),
    )
    .await
    .expect("parallel bash calls should run concurrently")
    .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(started.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelling_mid_batch_interrupts_and_balances_tool_result() {
    // A tool that has started but not finished must not pin the run past a
    // cancel, and the assistant's tool call must still receive a (synthetic)
    // tool result so the next request stays well-formed. The barrier has two
    // parties but only the tool waits, so `execute` blocks until the future is
    // dropped by cancellation.
    let fixture = TestFixture::new();
    let barrier = Arc::new(Barrier::new(2));
    let started = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BarrierTool::new(
        "bash",
        barrier.clone(),
        started.clone(),
    )));

    let provider = Box::new(MockProvider::new(vec![Ok(StreamedResponse {
        content: String::new(),
        tool_calls: vec![test_tool_call(
            "call-1",
            "bash",
            r#"{"command":"sleep 100"}"#,
        )],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let token = CancellationToken::new();
    // Cancel once the tool has started but is still blocked in `execute`.
    let watcher = {
        let token = token.clone();
        let started = started.clone();
        tokio::spawn(async move {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            token.cancel();
        })
    };

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run("hello", token, Arc::new(StdoutSink)),
    )
    .await
    .expect("a cancelled mid-batch tool must not hang the run")
    .unwrap();
    watcher.await.unwrap();

    assert_eq!(result, AgentRunResult::Interrupted(String::new()));
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the tool should have started"
    );

    // Every assistant tool call needs a matching tool result or the next
    // request is malformed; the interrupted call gets a synthetic one.
    let tool_results = agent
        .context_messages()
        .iter()
        .filter(|message| matches!(message, ChatCompletionRequestMessage::Tool(_)))
        .count();
    assert_eq!(
        tool_results, 1,
        "the interrupted tool call must still receive a tool result"
    );
}
