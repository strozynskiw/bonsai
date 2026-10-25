//! One canonical workspace and one non-stealable OS owner through runtime drain.
use super::{
    config::{Command, Config},
    controller::{Controller, SessionRuntime},
    http::{Api, ApiState},
};
use crate::storage::{SessionRunSelection, Storage};
use anyhow::{Context, Result, ensure};
use std::{path::Path, sync::Arc, time::Duration};

pub(crate) async fn run(command: Command) -> Result<()> {
    let root = std::env::current_dir()?.canonicalize()?;
    let storage = Storage::open().await?;
    match command {
        Command::Mint {
            output,
            ttl,
            sessions,
            scopes,
        } => {
            let id = super::auth::mint(&storage, &root, &output, ttl, sessions, scopes).await?;
            println!("Token {id} saved to {}", output.display());
            Ok(())
        }
        Command::Revoke { id } => storage.api_revoke(&root.to_string_lossy(), &id).await,
        Command::Listen(config) => listen(&root, storage, config).await,
    }
}
async fn listen(root: &Path, storage: Storage, config: Config) -> Result<()> {
    config.validate()?;
    let owner = workspace_owner(storage.home_dir(), root)?;
    let project = storage.ensure_project(root).await?;
    let trust = crate::workspace_trust::WorkspaceTrustGate::load(storage.clone(), project).await?;
    let trusted = matches!(
        trust.state(),
        crate::workspace_trust::WorkspaceTrust::Trusted
    );
    let catalog = Arc::new(
        crate::model_catalog::load_catalog_from_home_and_project_with_refresh(
            storage.home_dir(),
            trusted.then_some(root),
        )
        .await?,
    );
    let registry = Arc::new(crate::provider::ProviderRegistry::from_catalog(&catalog));
    let mut session =
        crate::session::SessionStore::load_with_storage_and_catalog(&storage, Some(&catalog))
            .await?;
    crate::headless::select_provider(
        &registry,
        &mut session,
        std::env::var("BONSAI_PROVIDER").ok().as_deref(),
    )?;
    let selected = session.current_session();
    let selection = SessionRunSelection {
        provider_id: session.current_kind_id().into(),
        model: selected.model.clone(),
        reasoning: selected.reasoning,
    };
    let factory = SessionRuntime {
        root: root.into(),
        storage: storage.clone(),
        catalog,
        registry,
        session,
        budget: storage.run_budget().await?,
        autonomy: crate::yolo::YoloMode::with_level(crate::tool::ApprovalLevel::Ask),
        trust,
    };
    let controller = Arc::new(Controller::new(root, storage.clone(), Arc::new(factory))?);
    let listener = std::net::TcpListener::bind(config.bind)?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        eprintln!(
            "Warning: remote server exposes sensitive session data; TLS and scoped authentication are required"
        );
    }
    let scheme = if config.cert.is_some() {
        "https"
    } else {
        "http"
    };
    let token_path = storage
        .home_dir()
        .join(format!("server-token-{}", super::auth::random_id()?));
    let id = super::auth::mint(
        &storage,
        root,
        &token_path,
        3600,
        Vec::new(),
        ["read", "control", "respond", "artifacts"]
            .map(String::from)
            .to_vec(),
    )
    .await?;
    eprintln!(
        "Experimental server {scheme}://{address}; token {id} in {}",
        token_path.display()
    );
    let api = Api::new(ApiState {
        controller: controller.clone(),
        storage: storage.clone(),
        root: root.to_string_lossy().into_owned(),
        selection,
        config: config.clone(),
        host: address.to_string(),
        epoch: super::auth::random_id()?,
        mutations: tokio::sync::Mutex::new(()),
        streams: Arc::new(tokio::sync::Semaphore::new(8)),
        requests: Arc::new(tokio::sync::Semaphore::new(32)),
        rate: std::sync::Mutex::new((std::time::Instant::now(), 0)),
        artifacts: super::artifacts::Artifacts::new(storage.home_dir())?,
    });
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    let shutting_down = controller.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.graceful_shutdown(Some(Duration::from_secs(30)));
        shutting_down.shutdown(Duration::from_secs(30)).await
    });
    let router = api.router();
    let result = if let (Some(cert), Some(key)) = (config.cert, config.key) {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
        axum_server::from_tcp_rustls(listener, tls)?
            .handle(handle)
            .serve(router.into_make_service())
            .await
    } else {
        axum_server::from_tcp(listener)?
            .handle(handle)
            .serve(router.into_make_service())
            .await
    };
    let drained = if signal.is_finished() {
        signal.await.context("Shutdown supervisor failed")?
    } else {
        signal.abort();
        controller.shutdown(Duration::from_secs(30)).await
    };
    // A failed drain keeps the workspace lock alongside the controller until
    // process exit; do not expose a false successful shutdown or unlock early.
    if let Err(error) = drained {
        std::mem::forget(owner);
        return Err(error);
    }
    drop(owner);
    result.context("Server listener failed")
}
fn workspace_owner(home: &Path, root: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let key = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex();
    let path = home.join(format!("serve-{key}.lock"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "Invalid workspace lock");
    file.try_lock()
        .context("Another server owns this workspace")?;
    Ok(file)
}
