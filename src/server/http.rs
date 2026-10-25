//! Authenticated experimental HTTP boundary. No client-controlled runtime policy.
use super::{auth, config::Config, contract, controller::Controller};
use crate::storage::{SessionId, SessionRunSelection, Storage};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};

pub(crate) const PREFIX: &str = "/api/experimental/v0";
pub(crate) const BODY_LIMIT: usize = 1024 * 1024;
#[derive(Clone)]
pub(crate) struct Api(pub(super) Arc<ApiState>);
pub(crate) struct ApiState {
    pub controller: Arc<Controller>,
    pub storage: Storage,
    pub root: String,
    pub selection: SessionRunSelection,
    pub config: Config,
    pub host: String,
    pub epoch: String,
    pub mutations: Mutex<()>,
    pub streams: Arc<Semaphore>,
    pub requests: Arc<Semaphore>,
    pub rate: std::sync::Mutex<(std::time::Instant, u32)>,
    pub artifacts: super::artifacts::Artifacts,
}
impl Api {
    pub fn new(state: ApiState) -> Self {
        Self(Arc::new(state))
    }
    pub fn router(&self) -> Router {
        Router::new()
            .fallback(any(dispatch))
            .layer(middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }
}
#[derive(Clone)]
struct Identity {
    digest: String,
    token: crate::storage::server_api::ApiToken,
    request: String,
}
#[derive(Debug)]
struct Failure(StatusCode, &'static str);
type Result<T> = std::result::Result<T, Failure>;
impl Failure {
    fn internal() -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }
}
fn error(status: StatusCode, code: &str, request: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"code":code,"message":code,"request_id":request}})),
    )
        .into_response()
}
fn response(value: Value) -> Response {
    axum::Json(value).into_response()
}
async fn guard(State(api): State<Api>, mut req: Request, next: Next) -> Response {
    let request = uuid::Uuid::now_v7().to_string();
    let result = guard_inner(&api, &mut req, &request).await;
    let mut output = match result {
        Ok(permit) => {
            let permit = Arc::new(permit);
            req.extensions_mut().insert(permit.clone());
            let stream = req.method() == Method::GET && req.uri().path().ends_with("/events");
            let output = if req.method() == Method::OPTIONS {
                let mut output = StatusCode::NO_CONTENT.into_response();
                if let Some(origin) = req.headers().get("origin") {
                    output
                        .headers_mut()
                        .insert("access-control-allow-origin", origin.clone());
                }
                output.headers_mut().insert(
                    "access-control-allow-methods",
                    HeaderValue::from_static("GET, POST, OPTIONS"),
                );
                output.headers_mut().insert(
                    "access-control-allow-headers",
                    HeaderValue::from_static("Authorization, Content-Type, Last-Event-ID"),
                );
                output.headers_mut().insert(
                    "vary",
                    HeaderValue::from_static(
                        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
                    ),
                );
                output
            } else if stream {
                next.run(req).await
            } else {
                match tokio::time::timeout(Duration::from_secs(30), next.run(req)).await {
                    Ok(r) => r,
                    Err(_) => error(StatusCode::GATEWAY_TIMEOUT, "request_timeout", &request),
                }
            };
            drop(permit);
            output
        }
        Err(f) => error(f.0, f.1, &request),
    };
    output.headers_mut().insert(
        "bonsai-api-version",
        HeaderValue::from_static("experimental-v0"),
    );
    output.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    output
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&request) {
        output.headers_mut().insert("x-request-id", value);
    }
    output
}
async fn guard_inner(
    api: &Api,
    req: &mut Request,
    request: &str,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    let permit = api
        .0
        .requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure(StatusCode::TOO_MANY_REQUESTS, "request_limit"))?;
    {
        let mut rate = api
            .0
            .rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rate.0.elapsed() >= Duration::from_secs(1) {
            *rate = (std::time::Instant::now(), 0);
        }
        if rate.1 >= 100 {
            return Err(Failure(StatusCode::TOO_MANY_REQUESTS, "rate_limit"));
        }
        rate.1 += 1;
    }
    let host = req.headers().get_all("host");
    let mut hosts = host.iter();
    let value = hosts.next().and_then(|v| v.to_str().ok()).unwrap_or("");
    if hosts.next().is_some()
        || !(value == api.0.host || api.0.config.hosts.iter().any(|h| h == value))
    {
        return Err(Failure(StatusCode::FORBIDDEN, "host_denied"));
    }
    if let Some(origin) = req.headers().get("origin") {
        let origin = origin
            .to_str()
            .map_err(|_| Failure(StatusCode::FORBIDDEN, "origin_denied"))?;
        if !api.0.config.origins.iter().any(|o| o == origin) {
            return Err(Failure(StatusCode::FORBIDDEN, "origin_denied"));
        }
    }
    // No URL credential channel, even when a valid header is also present.
    if req.uri().query().is_some_and(|q| {
        q.split('&').any(|p| {
            matches!(
                p.split('=').next(),
                Some("token" | "access_token" | "authorization")
            )
        })
    }) {
        return Err(Failure(
            StatusCode::BAD_REQUEST,
            "url_credentials_forbidden",
        ));
    }
    if req.method() == Method::OPTIONS {
        // Browser preflight carries no Bearer. It reveals only static policy;
        // every subsequent resource request still takes the full auth path.
        let method = req
            .headers()
            .get("access-control-request-method")
            .and_then(|v| v.to_str().ok());
        let headers = req
            .headers()
            .get("access-control-request-headers")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !req.headers().contains_key("origin")
            || req
                .headers()
                .get_all("access-control-request-method")
                .iter()
                .count()
                != 1
            || req
                .headers()
                .get_all("access-control-request-headers")
                .iter()
                .count()
                > 1
            || !matches!(method, Some("GET" | "POST"))
            || !headers
                .split(',')
                .filter(|h| !h.trim().is_empty())
                .all(|h| {
                    ["authorization", "content-type", "last-event-id"]
                        .iter()
                        .any(|allowed| h.trim().eq_ignore_ascii_case(allowed))
                })
        {
            return Err(Failure(StatusCode::FORBIDDEN, "preflight_denied"));
        }
        return Ok(permit);
    }
    let mut headers = req.headers().get_all("authorization").iter();
    let bearer = headers
        .next()
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|v| v.len() == 43);
    let token = if headers.next().is_none() {
        match bearer {
            Some(secret) => api
                .0
                .storage
                .api_token(&auth::digest(secret))
                .await
                .map_err(|_| Failure::internal())?,
            None => None,
        }
    } else {
        None
    };
    let Some(token) = token.filter(|t| t.workspace == api.0.root) else {
        api.0
            .storage
            .api_audit(None, None, None, request, "auth", "deny")
            .await
            .map_err(|_| Failure::internal())?;
        return Err(Failure(StatusCode::UNAUTHORIZED, "unauthorized"));
    };
    let digest = auth::digest(bearer.unwrap_or_default());
    req.extensions_mut().insert(Identity {
        digest,
        token,
        request: request.into(),
    });
    Ok(permit)
}
async fn authorize(api: &Api, id: &Identity, scope: &str, session: Option<i64>) -> Result<()> {
    let token = api
        .0
        .storage
        .api_token(&id.digest)
        .await
        .map_err(|_| Failure::internal())?;
    let allowed = token.is_some_and(|t| {
        t.workspace == api.0.root
            && t.scopes.iter().any(|s| s == scope)
            && match session {
                Some(s) => t.sessions.is_empty() || t.sessions.contains(&s),
                None => t.sessions.is_empty(),
            }
    });
    if !allowed {
        api.0
            .storage
            .api_audit(
                Some(&id.token.id),
                session,
                None,
                &id.request,
                "scope",
                "deny",
            )
            .await
            .map_err(|_| Failure::internal())?;
        return Err(Failure(StatusCode::FORBIDDEN, "scope_denied"));
    }
    Ok(())
}
async fn dispatch(State(api): State<Api>, req: Request) -> Response {
    let id = req.extensions().get::<Identity>().cloned();
    let Some(id) = id else {
        return error(StatusCode::UNAUTHORIZED, "unauthorized", "");
    };
    let origin = req.headers().get("origin").cloned();
    // Read bounded input before locking or detaching. The outer deadline drops
    // incomplete bodies; detached work retains its concurrency permit to settle.
    let result = if req.method() == Method::POST {
        let (parts, body) = req.into_parts();
        match to_bytes(body, BODY_LIMIT).await {
            Err(_) => Err(Failure(StatusCode::PAYLOAD_TOO_LARGE, "body_limit")),
            Ok(bytes) if serde_json::from_slice::<Value>(&bytes).is_err() => {
                Err(Failure(StatusCode::BAD_REQUEST, "invalid_json"))
            }
            Ok(bytes) => {
                let api = api.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    let _permit = parts
                        .extensions
                        .get::<Arc<tokio::sync::OwnedSemaphorePermit>>()
                        .cloned();
                    route(&api, &id, Request::from_parts(parts, Body::from(bytes))).await
                })
                .await
                .unwrap_or_else(|_| Err(Failure::internal()))
            }
        }
    } else {
        route(&api, &id, req).await
    };
    let mut output = match result {
        Ok(r) => r,
        Err(f) => error(f.0, f.1, &id.request),
    };
    if let Some(origin) = origin {
        output
            .headers_mut()
            .insert("access-control-allow-origin", origin);
        output
            .headers_mut()
            .insert("vary", HeaderValue::from_static("Origin"));
    }
    output
}
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Page {
    after: Option<i64>,
    offset: Option<usize>,
    limit: Option<usize>,
}
fn page(query: Option<&str>) -> Result<Page> {
    let mut page = Page::default();
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (key, value) = pair
            .split_once('=')
            .ok_or(Failure(StatusCode::BAD_REQUEST, "invalid_page"))?;
        match key {
            "after" => {
                page.after = Some(
                    value
                        .parse()
                        .map_err(|_| Failure(StatusCode::BAD_REQUEST, "invalid_page"))?,
                )
            }
            "offset" => {
                page.offset = Some(
                    value
                        .parse()
                        .map_err(|_| Failure(StatusCode::BAD_REQUEST, "invalid_page"))?,
                )
            }
            "limit" => {
                page.limit = Some(
                    value
                        .parse()
                        .map_err(|_| Failure(StatusCode::BAD_REQUEST, "invalid_page"))?,
                )
            }
            _ => return Err(Failure(StatusCode::BAD_REQUEST, "invalid_page")),
        }
    }
    if page.limit.is_some_and(|l| l == 0 || l > 100) || page.after.is_some_and(|a| a < 0) {
        return Err(Failure(StatusCode::BAD_REQUEST, "invalid_page"));
    }
    Ok(page)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutation {
    request_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    request_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    request_id: String,
    run_id: String,
    answer: contract::Answer,
}
async fn body<T: serde::de::DeserializeOwned>(req: Request) -> Result<T> {
    let bytes = to_bytes(req.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| Failure(StatusCode::PAYLOAD_TOO_LARGE, "body_limit"))?;
    serde_json::from_slice(&bytes).map_err(|_| Failure(StatusCode::BAD_REQUEST, "invalid_json"))
}
fn request_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err(Failure(StatusCode::BAD_REQUEST, "invalid_request_id"));
    }
    Ok(())
}
async fn route(api: &Api, id: &Identity, req: Request) -> Result<Response> {
    let path = req
        .uri()
        .path()
        .strip_prefix(PREFIX)
        .ok_or(Failure(StatusCode::NOT_FOUND, "not_found"))?
        .to_owned();
    let method = req.method().clone();
    let headers = req.headers().clone();
    let query = req.uri().query().map(str::to_owned);
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if method == Method::OPTIONS {
        return Ok((
            StatusCode::NO_CONTENT,
            [
                ("access-control-allow-methods", "GET, POST, OPTIONS"),
                (
                    "access-control-allow-headers",
                    "Authorization, Content-Type, Last-Event-ID",
                ),
            ],
        )
            .into_response());
    }
    if parts == ["sessions"] && method == Method::GET {
        // Session-scoped readers may list only their explicitly delegated sessions.
        if !id.token.scopes.iter().any(|s| s == "read") {
            return Err(Failure(StatusCode::FORBIDDEN, "scope_denied"));
        }
        let page = page(query.as_deref())?;
        let limit = page.limit.unwrap_or(50);
        let ids = api
            .0
            .storage
            .api_session_ids(
                &api.0.root,
                page.after.unwrap_or(0),
                limit as i64,
                &id.token.sessions,
            )
            .await
            .map_err(|_| Failure::internal())?;
        let mut items = Vec::new();
        for session in ids {
            if id.token.sessions.is_empty() || id.token.sessions.contains(&session) {
                authorize(api, id, "read", Some(session)).await?;
                items.push(json!({"id":session}));
                if items.len() == limit {
                    break;
                }
            }
        }
        return Ok(response(
            json!({"sessions":items,"next_after":items.last().and_then(|v|v.get("id"))}),
        ));
    }
    if parts == ["sessions"] && method == Method::POST {
        let input: Mutation = body(req).await?;
        request_id(&input.request_id)?;
        let _lock = api.0.mutations.lock().await;
        authorize(api, id, "control", None).await?;
        if let Some(r) = prior(api, "create", 0, &input.request_id, "").await? {
            return Ok(response(r));
        }
        admit(api, id, "create", 0, &input.request_id, "").await?;
        authorize(api, id, "control", None).await?;
        let session = api
            .0
            .controller
            .create(&api.0.selection)
            .await
            .map_err(|_| Failure(StatusCode::CONFLICT, "create_failed"))?;
        let result = json!({"session_id":session.as_i64()});
        finish(api, "create", 0, &input.request_id, &result).await?;
        return Ok((StatusCode::CREATED, axum::Json(result)).into_response());
    }
    let session = parts
        .get(1)
        .filter(|_| parts.first() == Some(&"sessions"))
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or(Failure(StatusCode::NOT_FOUND, "not_found"))?;
    let sid = SessionId::from_raw(session);
    let scope = if parts.get(2) == Some(&"artifacts") {
        "artifacts"
    } else if parts.get(2) == Some(&"interactions") {
        "respond"
    } else if method == Method::GET {
        "read"
    } else {
        "control"
    };
    authorize(api, id, scope, Some(session)).await?;
    let saved = api
        .0
        .storage
        .load_runtime_session_snapshot(sid)
        .await
        .map_err(|_| Failure::internal())?
        .filter(|s| s.summary.project_path == api.0.root)
        .ok_or(Failure(StatusCode::NOT_FOUND, "not_found"))?;
    if parts.len() == 2 && method == Method::GET {
        let page = page(query.as_deref())?;
        if let Ok((snapshot, _)) = api.0.controller.subscribe().await
            && snapshot.control.session_id == sid
        {
            return Ok(response(contract::snapshot(
                &snapshot,
                page.offset.unwrap_or(0),
                page.limit.unwrap_or(100),
            )));
        }
        return Ok(response(
            json!({"session_id":session,"state":"unloaded","pending":[],"transcript":saved.transcript.iter().skip(page.offset.unwrap_or(0)).take(page.limit.unwrap_or(100)).collect::<Vec<_>>(),"total":saved.transcript.len(),"plan":saved.plan,"todos":saved.todos,"usage":{"prompt_tokens":saved.summary.prompt_token_count,"completion_tokens":saved.summary.completion_token_count,"cost_micros":saved.summary.cost_micros}}),
        ));
    }
    if parts.len() == 3 && parts[2] == "events" && method == Method::GET {
        return stream(api, id, sid, &headers).await;
    }
    if (parts.len() == 3 || parts.len() == 4)
        && parts.get(2) == Some(&"artifacts")
        && method == Method::GET
    {
        return artifact(api, id, session, parts.get(3).copied()).await;
    }
    if method != Method::POST {
        return Err(Failure(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ));
    }
    let _lock = api.0.mutations.lock().await;
    authorize(api, id, scope, Some(session)).await?;
    match parts.as_slice() {
        [_, _, "messages"] => {
            let input: Message = body(req).await?;
            request_id(&input.request_id)?;
            if input.text.trim().is_empty() {
                return Err(Failure(StatusCode::BAD_REQUEST, "empty_message"));
            }
            api.0
                .storage
                .api_audit(
                    Some(&id.token.id),
                    Some(session),
                    None,
                    &id.request,
                    "message",
                    "admit",
                )
                .await
                .map_err(|_| Failure::internal())?;
            authorize(api, id, "control", Some(session)).await?;
            let command = api
                .0
                .controller
                .message(sid, &input.request_id, &input.text)
                .await
                .map_err(|_| Failure(StatusCode::CONFLICT, "message_conflict"))?;
            Ok((
                StatusCode::ACCEPTED,
                axum::Json(json!({"run_id":command.run_id,"state":command.state})),
            )
                .into_response())
        }
        [_, _, op @ ("resume" | "interrupt")] => {
            let input: Mutation = body(req).await?;
            request_id(&input.request_id)?;
            authorize(api, id, "control", Some(session)).await?;
            if let Some(r) = prior(api, op, session, &input.request_id, "").await? {
                return Ok(response(r));
            }
            admit(api, id, op, session, &input.request_id, "").await?;
            authorize(api, id, "control", Some(session)).await?;
            if *op == "resume" {
                // Never cancel another session as a side effect of a target's resume.
                if let Ok((current, _)) = api.0.controller.snapshot().await
                    && current.session_id != sid
                {
                    api.0
                        .controller
                        .release_idle()
                        .await
                        .map_err(|_| Failure(StatusCode::CONFLICT, "slot_busy"))?;
                }
                let selection = SessionRunSelection {
                    provider_id: saved.summary.provider_id,
                    model: saved.summary.model,
                    reasoning: saved.summary.reasoning,
                };
                api.0
                    .controller
                    .resume(sid, &selection)
                    .await
                    .map_err(|_| Failure(StatusCode::CONFLICT, "resume_conflict"))?;
            } else {
                api.0
                    .controller
                    .interrupt(sid)
                    .await
                    .map_err(|_| Failure(StatusCode::CONFLICT, "interrupt_conflict"))?;
            }
            let result = json!({"session_id":session,"state":if *op=="resume" {"resumed"} else {"cancellation_requested"}});
            finish(api, op, session, &input.request_id, &result).await?;
            Ok(response(result))
        }
        [_, _, "interactions", request, "responses"] => {
            let pending = request
                .parse::<u64>()
                .map_err(|_| Failure(StatusCode::BAD_REQUEST, "invalid_interaction"))?;
            let input: Answer = body(req).await?;
            request_id(&input.request_id)?;
            authorize(api, id, "respond", Some(session)).await?;
            let content = serde_json::to_string(&(&input.run_id, pending, &input.answer))
                .map_err(|_| Failure::internal())?;
            let operation = format!("interaction:{pending}");
            if let Some(result) =
                prior(api, &operation, session, &input.request_id, &content).await?
            {
                return Ok(response(result));
            }
            let outcome = input
                .answer
                .outcome()
                .map_err(|_| Failure(StatusCode::BAD_REQUEST, "invalid_decision"))?;
            // Durable request identity does not bypass the engine's single-use gate.
            admit(api, id, &operation, session, &input.request_id, &content).await?;
            api.0
                .storage
                .api_audit(
                    Some(&id.token.id),
                    Some(session),
                    Some(&input.run_id),
                    &id.request,
                    "interaction",
                    "respond",
                )
                .await
                .map_err(|_| Failure::internal())?;
            authorize(api, id, "respond", Some(session)).await?;
            api.0
                .controller
                .respond(sid, &input.run_id, pending, outcome)
                .await
                .map_err(|_| Failure(StatusCode::CONFLICT, "stale_interaction"))?;
            let result = json!({"state":"resolved"});
            finish(api, &operation, session, &input.request_id, &result).await?;
            Ok(response(result))
        }
        _ => Err(Failure(StatusCode::NOT_FOUND, "not_found")),
    }
}
async fn prior(
    api: &Api,
    op: &str,
    session: i64,
    request: &str,
    content: &str,
) -> Result<Option<Value>> {
    match api
        .0
        .storage
        .api_mutation(&api.0.root, op, session, request, &auth::digest(content))
        .await
        .map_err(|_| Failure(StatusCode::CONFLICT, "request_conflict"))?
    {
        None => Ok(None),
        Some(Some(value)) => Ok(Some(
            serde_json::from_str(&value).map_err(|_| Failure::internal())?,
        )),
        Some(None) => Err(Failure(StatusCode::CONFLICT, "interrupted_acceptance")),
    }
}
async fn admit(
    api: &Api,
    id: &Identity,
    op: &str,
    session: i64,
    request: &str,
    content: &str,
) -> Result<()> {
    api.0
        .storage
        .api_audit(
            Some(&id.token.id),
            (session > 0).then_some(session),
            None,
            &id.request,
            op,
            "admit",
        )
        .await
        .map_err(|_| Failure::internal())?;
    api.0
        .storage
        .api_admit(&api.0.root, op, session, request, &auth::digest(content))
        .await
        .map_err(|_| Failure::internal())
}
async fn finish(api: &Api, op: &str, session: i64, request: &str, result: &Value) -> Result<()> {
    api.0
        .storage
        .api_finish(&api.0.root, op, session, request, &result.to_string())
        .await
        .map_err(|_| Failure::internal())
}

async fn stream(api: &Api, id: &Identity, sid: SessionId, headers: &HeaderMap) -> Result<Response> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let permit = api
        .0
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure(StatusCode::TOO_MANY_REQUESTS, "stream_limit"))?;
    let (snapshot, receiver, sink) = api
        .0
        .controller
        .subscribe_stream()
        .await
        .map_err(|_| Failure(StatusCode::CONFLICT, "resume_required"))?;
    if snapshot.control.session_id != sid {
        return Err(Failure(StatusCode::CONFLICT, "resume_required"));
    }
    let cursor_epoch = format!("{}:{}", api.0.epoch, sink.generation());
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit_once(':'))
        .and_then(|(epoch, seq)| seq.parse::<u64>().ok().map(|seq| (epoch, seq)));
    let replay = cursor
        .filter(|(epoch, _)| *epoch == cursor_epoch)
        .and_then(|(_, seq)| sink.replay(seq, snapshot.watermark));
    let mut initial = std::collections::VecDeque::new();
    if let Some(replay) = replay {
        for e in replay {
            initial.push_back(("event", e.sequence, contract::event(&e.event)));
        }
    } else {
        if headers.contains_key("last-event-id") {
            initial.push_back((
                "reset",
                snapshot.watermark,
                json!({"reason":"cursor_unavailable"}),
            ));
        }
        initial.push_back((
            "snapshot",
            snapshot.watermark,
            contract::snapshot(&snapshot, 0, snapshot.transcript.len()),
        ));
    }
    struct StreamState {
        api: Api,
        id: Identity,
        sid: SessionId,
        cursor_epoch: String,
        receiver: tokio::sync::broadcast::Receiver<super::sink::OrderedEvent>,
        initial: std::collections::VecDeque<(&'static str, u64, Value)>,
        done: bool,
    }
    let state = StreamState {
        api: api.clone(),
        id: id.clone(),
        sid,
        cursor_epoch,
        receiver,
        initial,
        done: false,
    };
    let output = futures::stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        if authorize(&s.api, &s.id, "read", Some(s.sid.as_i64()))
            .await
            .is_err()
        {
            return None;
        }
        let next = if let Some(item) = s.initial.pop_front() {
            item
        } else {
            match tokio::time::timeout(Duration::from_secs(1), s.receiver.recv()).await {
                Ok(Ok(e)) => {
                    if matches!(e.event, super::sink::SessionEvent::ResyncRequired) {
                        s.done = true;
                        (
                            "reset",
                            e.sequence,
                            json!({"reason":"event_too_large","resync_required":true}),
                        )
                    } else {
                        ("event", e.sequence, contract::event(&e.event))
                    }
                }
                Ok(Err(_)) => {
                    s.done = true;
                    (
                        "reset",
                        0,
                        json!({"reason":"slow_consumer","resync_required":true}),
                    )
                }
                Err(_) => ("heartbeat", 0, json!({})),
            }
        };
        // Authorization can expire while waiting for the next event.
        if authorize(&s.api, &s.id, "read", Some(s.sid.as_i64()))
            .await
            .is_err()
        {
            return None;
        }
        let bytes = next.2.to_string();
        if bytes.len() > 8 * 1024 * 1024 {
            s.done = true;
            return Some((
                Ok::<_, std::convert::Infallible>(
                    Event::default()
                        .event("reset")
                        .data("{\"reason\":\"event_too_large\"}"),
                ),
                s,
            ));
        }
        let mut event = Event::default().event(next.0).data(bytes);
        if next.0 != "heartbeat" && next.0 != "reset" {
            event = event.id(format!("{}:{}", s.cursor_epoch, next.1));
        }
        Some((Ok::<_, std::convert::Infallible>(event), s))
    });
    // A one-item handoff bounds queued bytes and advances independently of
    // socket backpressure. No reader can retain its SSE slot indefinitely.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        use futures::StreamExt;
        let _permit = permit;
        futures::pin_mut!(output);
        loop {
            let next = tokio::select! {
                () = tx.closed() => break,
                next = output.next() => next,
            };
            let Some(event) = next else {
                break;
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), tx.send(event)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });
    let output = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    });
    Ok(Sse::new(output)
        .keep_alive(KeepAlive::default())
        .into_response())
}
async fn artifact(
    api: &Api,
    id: &Identity,
    session: i64,
    artifact: Option<&str>,
) -> Result<Response> {
    api.0
        .storage
        .api_audit(
            Some(&id.token.id),
            Some(session),
            None,
            &id.request,
            "artifact",
            "read",
        )
        .await
        .map_err(|_| Failure::internal())?;
    if let Some(artifact) = artifact {
        let (record, file) = api
            .0
            .artifacts
            .download(&api.0.storage, &api.0.root, session, artifact)
            .await
            .map_err(|_| Failure(StatusCode::NOT_FOUND, "artifact_unavailable"))?;
        let mut output = Body::from_stream(tokio_util::io::ReaderStream::new(
            tokio::fs::File::from_std(file),
        ))
        .into_response();
        output.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("application/octet-stream"),
        );
        output.headers_mut().insert(
            "content-disposition",
            HeaderValue::from_str(&format!("attachment; filename=\"{}\"", record.id))
                .map_err(|_| Failure::internal())?,
        );
        output.headers_mut().insert(
            "content-length",
            HeaderValue::from_str(&record.size.to_string()).map_err(|_| Failure::internal())?,
        );
        Ok(output)
    } else {
        Ok(response(
            json!({"artifacts":api.0.artifacts.list(&api.0.storage,&api.0.root,session).await.map_err(|_|Failure::internal())?}),
        ))
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn documented_request_fixtures_decode_with_actual_http_dtos() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../docs/server-contract.json")).unwrap();
        assert_eq!(fixture["prefix"], PREFIX);
        let mutation: Mutation =
            serde_json::from_value(fixture["requests"]["mutation"].clone()).unwrap();
        assert_eq!(mutation.request_id, "request-1");
        let message: Message =
            serde_json::from_value(fixture["requests"]["message"].clone()).unwrap();
        assert_eq!(
            (message.request_id.as_str(), message.text.as_str()),
            ("request-2", "Hello")
        );
        let answer: Answer = serde_json::from_value(fixture["requests"]["answer"].clone()).unwrap();
        assert_eq!(
            (answer.request_id.as_str(), answer.run_id.as_str()),
            ("request-3", "run-1")
        );
        assert_eq!(
            serde_json::to_value(answer.answer).unwrap(),
            fixture["requests"]["answer"]["answer"]
        );
        let mut invalid = fixture["requests"]["message"].clone();
        invalid["system_prompt"] = json!("not accepted");
        assert!(serde_json::from_value::<Message>(invalid).is_err());
    }
}
