use super::*;
use crate::server::{
    artifacts::Artifacts,
    auth,
    config::Config,
    http::{Api, ApiState},
};
use reqwest::StatusCode;
use serde_json::{Value, json};

struct HttpFixture {
    f: Fixture,
    api: Api,
    server: tokio::task::JoinHandle<()>,
    url: String,
    token: String,
    client: reqwest::Client,
}
#[tokio::test]
async fn scoped_listing_filters_before_pagination_and_idle_switch_is_safe() {
    let h = HttpFixture::new().await;
    h.resume().await;
    let second = h.api.0.controller.create(&h.f.selection).await.unwrap();
    let narrow = h.token(&["read"], vec![second.as_i64()], 3600).await;
    let response = h
        .client
        .get(format!("{}/sessions?limit=1", h.url))
        .bearer_auth(narrow)
        .send()
        .await
        .unwrap();
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["sessions"], json!([{"id":second.as_i64()}]));
    let result = h
        .post(
            &format!("/sessions/{second}/resume"),
            json!({"request_id":"switch-idle"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::OK);
    assert_eq!(
        h.api.0.controller.snapshot().await.unwrap().0.session_id,
        second
    );
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn slow_sse_reader_releases_slot_and_requires_reconnect() {
    use tower::ServiceExt;
    let h = HttpFixture::new().await;
    h.resume().await;
    let request = axum::http::Request::builder()
        .uri(format!("/api/experimental/v0/sessions/{}/events", h.f.id))
        .header("host", &h.api.0.host)
        .header("authorization", format!("Bearer {}", h.token))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = h.api.router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Keep body alive but never poll it: no consumer can block the producer.
    tokio::time::timeout(Duration::from_secs(9), async {
        while h.api.0.streams.available_permits() != 8 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    drop(response);
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn allowed_browser_preflight_is_public_but_actual_resources_require_bearer() {
    let origin = "https://client.example";
    let h = HttpFixture::with_config(Config {
        origins: vec![origin.into()],
        ..Config::default()
    })
    .await;
    let preflight = || {
        h.client
            .request(reqwest::Method::OPTIONS, format!("{}/sessions", h.url))
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization, content-type",
            )
    };
    let response = preflight().send().await.unwrap();
    version(&response);
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["access-control-allow-origin"], origin);
    assert!(response.bytes().await.unwrap().is_empty());
    let response = h
        .client
        .get(format!("{}/sessions", h.url))
        .header("origin", origin)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.get("/sessions")
            .header("origin", origin)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        preflight()
            .header("host", "rebind.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        preflight()
            .header("access-control-request-method", "DELETE")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        preflight()
            .header("access-control-request-headers", "x-unsafe")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn reconnect_after_same_process_session_reload_always_resets_cursor() {
    let h = HttpFixture::new().await;
    h.resume().await;
    let path = format!("/sessions/{}/events", h.f.id);
    let mut first = h.get(&path).send().await.unwrap();
    let initial = String::from_utf8(first.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    let cursor = initial
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    drop(first);
    let old_sequence = cursor.rsplit_once(':').unwrap().1.parse::<u64>().unwrap();
    h.api
        .0
        .controller
        .release(Duration::from_secs(5))
        .await
        .unwrap();
    let resumed = h
        .post(
            &format!("/sessions/{}/resume", h.f.id),
            json!({"request_id":"reload-generation"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    let (_, sink) = h.api.0.controller.snapshot().await.unwrap();
    // Make the old sequence valid numerically in the replacement sink. Only
    // its generation can distinguish this from a legitimate reconnect.
    while sink.subscribe().0 <= old_sequence {
        sink.publish(crate::server::sink::SessionEvent::Status(
            "new generation".into(),
        ));
    }
    let mut response = h
        .get(&path)
        .header("last-event-id", cursor)
        .send()
        .await
        .unwrap();
    let chunk = String::from_utf8(response.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(chunk.contains("event: reset"), "{chunk}");
    let chunk = String::from_utf8(response.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(chunk.contains("event: snapshot"), "{chunk}");
    drop(response);
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl HttpFixture {
    async fn new() -> Self {
        Self::with_config(Config::default()).await
    }
    async fn with_config(config: Config) -> Self {
        let f = Fixture::new().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let root =
            f.db.project_path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned();
        let api = Api::new(ApiState {
            controller: Arc::new(
                Controller::new(f.db.project_path(), f.db.storage.clone(), f.factory.clone())
                    .unwrap(),
            ),
            storage: f.db.storage.clone(),
            root,
            selection: f.selection.clone(),
            config,
            host: address.to_string(),
            epoch: "test-epoch".into(),
            mutations: Mutex::new(()),
            streams: Arc::new(tokio::sync::Semaphore::new(8)),
            requests: Arc::new(tokio::sync::Semaphore::new(32)),
            rate: std::sync::Mutex::new((std::time::Instant::now(), 0)),
            artifacts: Artifacts::new(f.db.storage.home_dir()).unwrap(),
        });
        let router = api.router();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut this = Self {
            f,
            api,
            server,
            url: format!("http://{address}/api/experimental/v0"),
            token: String::new(),
            client: reqwest::Client::new(),
        };
        this.token = this
            .token(&["read", "control", "respond", "artifacts"], vec![], 3600)
            .await;
        this
    }
    async fn token(&self, scopes: &[&str], sessions: Vec<i64>, ttl: u64) -> String {
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("token");
        auth::mint(
            &self.f.db.storage,
            self.f.db.project_path(),
            &path,
            ttl,
            sessions,
            scopes.iter().map(|s| s.to_string()).collect(),
        )
        .await
        .unwrap();
        std::fs::read_to_string(path).unwrap()
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
    }
    fn post(&self, path: &str, body: Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .json(&body)
    }
    async fn resume(&self) {
        assert_eq!(
            self.post(
                &format!("/sessions/{}/resume", self.f.id),
                json!({"request_id":"resume"})
            )
            .send()
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
    }
}
fn version(response: &reqwest::Response) {
    assert_eq!(response.headers()["bonsai-api-version"], "experimental-v0");
}

#[tokio::test]
async fn incomplete_post_never_holds_mutation_lock() {
    use tokio::io::AsyncWriteExt;
    let h = HttpFixture::new().await;
    h.resume().await;
    let mut socket = tokio::net::TcpStream::connect(&h.api.0.host).await.unwrap();
    socket.write_all(format!("POST /api/experimental/v0/sessions/{}/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{{",h.f.id,h.api.0.host,h.token).as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        h.post("/sessions", json!({"request_id":"unblocked"}))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    drop(socket);
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn http_expiry_workspace_scopes_and_retry_revocation() {
    let h = HttpFixture::new().await;
    let secret = auth::random_id().unwrap();
    let token = crate::storage::server_api::ApiToken {
        id: auth::random_id().unwrap(),
        workspace: h.api.0.root.clone(),
        sessions: vec![],
        scopes: vec!["read".into()],
        expires_at_ms: 0,
    };
    h.f.db
        .storage
        .api_insert_token(&token, &auth::digest(&secret))
        .await
        .unwrap();
    assert_eq!(
        h.client
            .get(format!("{}/sessions", h.url))
            .bearer_auth(secret)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let secret = auth::random_id().unwrap();
    let token = crate::storage::server_api::ApiToken {
        id: auth::random_id().unwrap(),
        workspace: "/another-workspace".into(),
        expires_at_ms: i64::MAX,
        ..token
    };
    h.f.db
        .storage
        .api_insert_token(&token, &auth::digest(&secret))
        .await
        .unwrap();
    assert_eq!(
        h.client
            .get(format!("{}/sessions", h.url))
            .bearer_auth(secret)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let scoped = h.token(&["control"], vec![h.f.id.as_i64()], 3600).await;
    assert_eq!(
        h.client
            .post(format!("{}/sessions", h.url))
            .bearer_auth(scoped)
            .json(&json!({"request_id":"create"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let created = h
        .post("/sessions", json!({"request_id":"durable-create"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let token =
        h.f.db
            .storage
            .api_token(&auth::digest(&h.token))
            .await
            .unwrap()
            .unwrap();
    h.f.db
        .storage
        .api_revoke(&h.api.0.root, &token.id)
        .await
        .unwrap();
    assert_eq!(
        h.post("/sessions", json!({"request_id":"durable-create"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn http_stream_limits_expiry_and_replay_gap() {
    let h = HttpFixture::new().await;
    h.resume().await;
    let path = format!("/sessions/{}/events", h.f.id);
    let mut streams = Vec::new();
    for _ in 0..8 {
        let response = h.get(&path).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        streams.push(response);
    }
    assert_eq!(
        h.get(&path).send().await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(streams);
    let (_, sink) = h.api.0.controller.snapshot().await.unwrap();
    for _ in 0..1001 {
        sink.publish(crate::server::sink::SessionEvent::Status("event".into()));
    }
    let mut response = h
        .get(&path)
        .header("last-event-id", "test-epoch:0")
        .send()
        .await
        .unwrap();
    assert!(
        String::from_utf8(response.chunk().await.unwrap().unwrap().to_vec())
            .unwrap()
            .contains("reset")
    );
    drop(response);
    let short = h.token(&["read"], vec![h.f.id.as_i64()], 1).await;
    let mut response = h
        .client
        .get(format!("{}{path}", h.url))
        .bearer_auth(short)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.chunk().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(3), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn http_all_interaction_types_are_correlated_and_single_use() {
    use crate::interaction::InteractionRequest as R;
    let h = HttpFixture::new().await;
    h.resume().await;
    let response = h
        .post(
            &format!("/sessions/{}/messages", h.f.id),
            json!({"request_id":"run","text":"Hello"}),
        )
        .send()
        .await
        .unwrap();
    let run = response.json::<Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let interaction =
        h.f.factory
            .handles
            .lock()
            .await
            .interaction
            .clone()
            .unwrap();
    let templates = [
        R::Permission {
            request_id: 0,
            command: "test".into(),
            origin: None,
        },
        R::WebDomain {
            request_id: 0,
            url: "https://example.com".into(),
            host: "example.com".into(),
            redirected_from: None,
            origin: None,
        },
        R::ExtensionTool {
            request_id: 0,
            id: "test".into(),
            server: "test".into(),
            capabilities: vec![],
            args_preview: "{}".into(),
        },
        R::HookTrust {
            request_id: 0,
            name: "test".into(),
            event: "test".into(),
            action_kind: "shell".into(),
            action_preview: "test".into(),
        },
        R::SandboxEscalation {
            request_id: 0,
            command: "test".into(),
            origin: None,
            kind: crate::interaction::SandboxEscalationKind::SandboxOnly,
        },
        R::Question {
            request_id: 0,
            prompt: "Choose".into(),
            header: None,
            options: vec![crate::interaction::QuestionOption::new("one", "")],
            multiple: false,
            origin: None,
        },
    ];
    for (n, template) in templates.into_iter().enumerate() {
        let interaction = interaction.clone();
        let waiter = tokio::spawn(async move {
            interaction
                .request(|id| {
                    let mut request = template;
                    match &mut request {
                        R::Permission { request_id, .. }
                        | R::WebDomain { request_id, .. }
                        | R::ExtensionTool { request_id, .. }
                        | R::HookTrust { request_id, .. }
                        | R::SandboxEscalation { request_id, .. }
                        | R::Question { request_id, .. } => *request_id = id,
                    }
                    request
                })
                .await
        });
        let pending = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = h.api.0.controller.subscribe().await.unwrap().0;
                if let Some(request) = snapshot.control.pending.first() {
                    break crate::server::contract::interaction(request);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let id = pending["request_id"].as_u64().unwrap();
        let path = format!("/sessions/{}/interactions/{id}/responses", h.f.id);
        let answer = match n {
            4 => json!({"kind":"sandbox","decision":"deny"}),
            5 => json!({"kind":"question","choices":[0]}),
            _ => json!({"kind":"permission","decision":"deny"}),
        };
        assert_eq!(
            h.post(
                &path,
                json!({"request_id":format!("stale-{n}"),"run_id":"old","answer":answer})
            )
            .send()
            .await
            .unwrap()
            .status(),
            StatusCode::CONFLICT
        );
        assert!(!waiter.is_finished());
        let body = json!({"request_id":format!("answer-{n}"),"run_id":run,"answer":answer});
        assert_eq!(
            h.post(&path, body.clone()).send().await.unwrap().status(),
            StatusCode::OK
        );
        assert!(waiter.await.unwrap().is_ok());
        assert_eq!(
            h.post(&path, body).send().await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            h.post(
                &path,
                json!({"request_id":format!("double-{n}"),"run_id":run,"answer":answer})
            )
            .send()
            .await
            .unwrap()
            .status(),
            StatusCode::CONFLICT
        );
    }
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn http_auth_matrix_host_origin_version_and_limits() {
    let h = HttpFixture::new().await;
    for token in [None, Some("invalid")] {
        let mut r = h.client.get(format!("{}/sessions", h.url));
        if let Some(token) = token {
            r = r.bearer_auth(token);
        }
        let r = r.send().await.unwrap();
        version(&r);
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    let no_read = h.token(&["control"], vec![], 3600).await;
    assert_eq!(
        h.client
            .get(format!("{}/sessions", h.url))
            .bearer_auth(no_read)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for (header, value) in [("host", "evil.test"), ("origin", "https://evil.test")] {
        let r = h
            .get("/sessions")
            .header(header, value)
            .send()
            .await
            .unwrap();
        version(&r);
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
    for path in ["/unknown", "/sessions?limit=101", "/sessions?token=secret"] {
        let r = h.get(path).send().await.unwrap();
        version(&r);
        assert!(r.status().is_client_error());
    }
    let r = h
        .client
        .request(reqwest::Method::OPTIONS, format!("{}/sessions", h.url))
        .header("origin", "https://evil.test")
        .send()
        .await
        .unwrap();
    version(&r);
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = h
        .post(
            "/sessions",
            json!({"request_id":"create","system_prompt":"evil"}),
        )
        .send()
        .await
        .unwrap();
    version(&r);
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = h
        .post(
            "/sessions",
            json!({"request_id":"create","padding":"x".repeat(1024*1024)}),
        )
        .send()
        .await
        .unwrap();
    version(&r);
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let digest = auth::digest(&h.token);
    let record = h.f.db.storage.api_token(&digest).await.unwrap().unwrap();
    h.f.db
        .storage
        .api_revoke(&h.api.0.root, &record.id)
        .await
        .unwrap();
    assert_eq!(
        h.get("/sessions").send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn http_sessions_dedup_conflicts_and_sensitive_scope() {
    let h = HttpFixture::new().await;
    h.resume().await;
    let path = format!("/sessions/{}/messages", h.f.id);
    let body = json!({"request_id":"one","text":"Hello"});
    let first = h.post(&path, body.clone()).send().await.unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first: Value = first.json().await.unwrap();
    let retry: Value = h
        .post(&path, body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, retry);
    assert_eq!(
        h.post(&path, json!({"request_id":"one","text":"different"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.post(&path, json!({"request_id":"two","text":"busy"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let artifact_only = h.token(&["artifacts"], vec![h.f.id.as_i64()], 3600).await;
    assert_eq!(
        h.client
            .get(format!("{}/sessions/{}", h.url, h.f.id))
            .bearer_auth(artifact_only)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.post(
            &format!("/sessions/{}/interrupt", h.f.id),
            json!({"request_id":"stop"})
        )
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}
#[tokio::test]
async fn http_sse_snapshot_replay_reset_and_disconnect_does_not_interrupt() {
    let h = HttpFixture::new().await;
    h.resume().await;
    let path = format!("/sessions/{}/events", h.f.id);
    let mut response = h.get(&path).send().await.unwrap();
    version(&response);
    assert_eq!(response.status(), StatusCode::OK);
    let first = String::from_utf8(response.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(first.contains("event: snapshot"), "{first}");
    let cursor = first
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    drop(response);
    h.post(
        &format!("/sessions/{}/messages", h.f.id),
        json!({"request_id":"run","text":"Hello"}),
    )
    .send()
    .await
    .unwrap();
    let mut response = h
        .get(&path)
        .header("last-event-id", cursor)
        .send()
        .await
        .unwrap();
    let event = String::from_utf8(response.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(event.contains("user_message"), "{event}");
    drop(response);
    assert_eq!(
        h.api.0.controller.snapshot().await.unwrap().0.state,
        "running"
    );
    let mut response = h
        .get(&path)
        .header("last-event-id", "old-epoch:1")
        .send()
        .await
        .unwrap();
    let event = String::from_utf8(response.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(event.contains("reset"), "{event}");
    drop(response);
    h.api
        .0
        .controller
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}
#[tokio::test]
async fn completion_artifact_scope_never_exposes_verification_or_review_secrets() {
    use crate::completion_report::{
        CompletionEvidenceSnapshot, CompletionReport, CompletionSessionEvidence, CompletionStatus,
    };
    let h = HttpFixture::new().await;
    let secret = "synthetic-artifact-secret-NEVER-PUBLISH";
    let mut verification = crate::verification::VerificationRunRecord::running(
        crate::verification::VerificationKind::Build,
        &[crate::verification::VerificationCheck {
            name: "test".into(),
            command: format!("SERVICE_PASSWORD={secret} pytest"),
        }],
    );
    verification.terminal_reason = Some(format!("SERVICE_PASSWORD={secret} pytest"));
    let review = crate::self_review::SelfReviewRunRecord {
        tool_call_id: None,
        started_at_ms: 1,
        mode: crate::self_review::SelfReviewMode::On,
        scope: crate::self_review::SelfReviewScope::Scoped,
        diff_line_count: 0,
        reviewer_duration_ms: 1,
        reviewer_prompt_tokens: 0,
        reviewer_completion_tokens: 0,
        reviewer_cost_micros: None,
        status: crate::self_review::SelfReviewRunStatus::TimedOut,
        result: Some(secret.into()),
        findings: Default::default(),
        disposition: None,
    };
    let report = CompletionReport::from_evidence(
        CompletionStatus::Completed,
        CompletionEvidenceSnapshot::default(),
        CompletionSessionEvidence {
            completion_guard: None,
            verification: Some(verification),
            review: Some(review),
            authorization_decisions: &[],
            usage: crate::agent::UsageTotals::default(),
            session_budget: Default::default(),
            budget_exhaustion: None,
        },
    );
    assert!(serde_json::to_string(&report).unwrap().contains(secret));
    let record = h
        .api
        .0
        .artifacts
        .publish_completion(&h.f.db.storage, h.f.id.as_i64(), "safe-run", &report)
        .await
        .unwrap();
    let token = h.token(&["artifacts"], vec![h.f.id.as_i64()], 3600).await;
    let response = h
        .client
        .get(format!(
            "{}/sessions/{}/artifacts/{}",
            h.url, h.f.id, record.id
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.text().await.unwrap();
    assert!(!bytes.contains(secret));
    assert!(!bytes.contains("SERVICE_PASSWORD"));
    let summary: Value = serde_json::from_str(&bytes).unwrap();
    assert_eq!(summary.as_object().unwrap().len(), 7);
    assert!(summary["verification_status"].is_string());
    assert_eq!(
        h.client
            .get(format!("{}/sessions/{}", h.url, h.f.id))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn http_artifact_is_private_copy_attachment_and_scope_bound() {
    let h = HttpFixture::new().await;
    let source = h.f.db.project_path().join("report.svg");
    std::fs::write(&source, "<svg><script>alert(1)</script></svg>").unwrap();
    let record = h
        .api
        .0
        .artifacts
        .register(
            &h.f.db.storage,
            h.f.db.project_path(),
            h.f.id.as_i64(),
            std::path::Path::new("report.svg"),
        )
        .await
        .unwrap();
    std::fs::write(&source, "changed workspace").unwrap();
    let response = h
        .get(&format!("/sessions/{}/artifacts/{}", h.f.id, record.id))
        .send()
        .await
        .unwrap();
    version(&response);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert!(response.text().await.unwrap().contains("<svg>"));
    let other = h.api.0.controller.create(&h.f.selection).await.unwrap();
    let wrong = h.token(&["artifacts"], vec![other.as_i64()], 3600).await;
    assert_eq!(
        h.client
            .get(format!(
                "{}/sessions/{}/artifacts/{}",
                h.url, h.f.id, record.id
            ))
            .bearer_auth(wrong)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    use std::os::unix::fs::symlink;
    symlink(&source, h.f.db.project_path().join("linked.svg")).unwrap();
    assert!(
        h.api
            .0
            .artifacts
            .register(
                &h.f.db.storage,
                h.f.db.project_path(),
                h.f.id.as_i64(),
                std::path::Path::new("linked.svg")
            )
            .await
            .is_err()
    );
    std::fs::hard_link(&source, h.f.db.project_path().join("hard.svg")).unwrap();
    assert!(
        h.api
            .0
            .artifacts
            .register(
                &h.f.db.storage,
                h.f.db.project_path(),
                h.f.id.as_i64(),
                std::path::Path::new("hard.svg")
            )
            .await
            .is_err()
    );
    assert!(
        h.api
            .0
            .artifacts
            .register(
                &h.f.db.storage,
                h.f.db.project_path(),
                h.f.id.as_i64(),
                std::path::Path::new("../report.svg")
            )
            .await
            .is_err()
    );
}
