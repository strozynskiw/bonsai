//! Static embeddings for memory retrieval. The trait keeps the concrete crate
//! swappable and lets recall degrade to BM25-only whenever a model is not
//! Ready — loading is lazy and fully off the turn path: the first recall/save
//! against a non-empty store kicks a background download+load; until it
//! resolves, retrieval silently runs BM25-only. A fresh install (empty store)
//! never downloads anything.

use anyhow::Result;

/// Environment opt-out: `BONSAI_MEMORY_EMBEDDINGS=off` selects the
/// [`NoopEmbedder`] at service construction, keeping recall BM25-only.
pub(crate) const MEMORY_EMBEDDINGS_ENV: &str = "BONSAI_MEMORY_EMBEDDINGS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbedderAvailability {
    Ready,
    Loading,
    Unavailable,
}

pub(crate) trait Embedder: Send + Sync {
    /// Stable id the embedding cache is keyed by (e.g. the HF repo).
    fn model_id(&self) -> &str;
    fn availability(&self) -> EmbedderAvailability;
    /// Encode texts to vectors. Only meaningful when Ready; static embeddings
    /// are lookup + mean-pool, so this is synchronous and microsecond-fast.
    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Idempotent kick for lazy loaders; a no-op by default.
    fn ensure_loading(&self) {}
}

/// The no-embeddings embedder: always Unavailable, so retrieval is BM25-only.
pub(crate) struct NoopEmbedder;

impl Embedder for NoopEmbedder {
    fn model_id(&self) -> &str {
        "noop"
    }

    fn availability(&self) -> EmbedderAvailability {
        EmbedderAvailability::Unavailable
    }

    fn encode(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        anyhow::bail!("no embedding model available")
    }
}

/// Pick the embedder for this session. `home_dir` anchors the model cache; an
/// empty home (unresolvable) or the env opt-out selects the noop.
pub(crate) fn default_embedder(home_dir: &std::path::Path) -> std::sync::Arc<dyn Embedder> {
    let opted_out = std::env::var(MEMORY_EMBEDDINGS_ENV)
        .is_ok_and(|value| matches!(value.trim(), "off" | "0" | "false"));
    if opted_out || home_dir.as_os_str().is_empty() {
        return std::sync::Arc::new(NoopEmbedder);
    }
    #[cfg(feature = "memory-embeddings")]
    {
        std::sync::Arc::new(model2vec::Model2VecEmbedder::new(home_dir))
    }
    #[cfg(not(feature = "memory-embeddings"))]
    {
        std::sync::Arc::new(NoopEmbedder)
    }
}

#[cfg(feature = "memory-embeddings")]
mod model2vec {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use anyhow::{Context, Result};
    use model2vec_rs::model::StaticModel;

    use super::{Embedder, EmbedderAvailability};

    /// Default model: 256-dim static embeddings, ~30 MB on disk, microsecond
    /// encode. Downloaded once into `$BONSAI_HOME/cache/memory-model/`.
    const MODEL_REPO: &str = "minishlab/potion-base-8M";
    const MODEL_FILES: [&str; 3] = ["tokenizer.json", "model.safetensors", "config.json"];

    enum State {
        Idle,
        Loading,
        Ready(Arc<StaticModel>),
        Unavailable,
    }

    pub(crate) struct Model2VecEmbedder {
        inner: Arc<Inner>,
    }

    struct Inner {
        model_dir: PathBuf,
        state: Mutex<State>,
    }

    impl Model2VecEmbedder {
        pub(crate) fn new(home_dir: &Path) -> Self {
            Self {
                inner: Arc::new(Inner {
                    model_dir: home_dir
                        .join("cache/memory-model")
                        .join(MODEL_REPO.replace('/', "--")),
                    state: Mutex::new(State::Idle),
                }),
            }
        }
    }

    impl Embedder for Model2VecEmbedder {
        fn model_id(&self) -> &str {
            MODEL_REPO
        }

        fn availability(&self) -> EmbedderAvailability {
            match *self.inner.state.lock().expect("embedder state lock") {
                State::Idle | State::Loading => EmbedderAvailability::Loading,
                State::Ready(_) => EmbedderAvailability::Ready,
                State::Unavailable => EmbedderAvailability::Unavailable,
            }
        }

        fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let model = match &*self.inner.state.lock().expect("embedder state lock") {
                State::Ready(model) => model.clone(),
                _ => anyhow::bail!("embedding model is not loaded yet"),
            };
            Ok(model.encode(texts))
        }

        fn ensure_loading(&self) {
            let mut state = self.inner.state.lock().expect("embedder state lock");
            if !matches!(*state, State::Idle) {
                return;
            }
            *state = State::Loading;
            drop(state);
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let resolved = match load(&inner.model_dir).await {
                    Ok(model) => State::Ready(Arc::new(model)),
                    Err(err) => {
                        // One warning per process; retried next session because
                        // the state is per-process.
                        tracing::warn!(
                            error = %format!("{err:#}"),
                            "memory embeddings unavailable; recall stays BM25-only"
                        );
                        State::Unavailable
                    }
                };
                *inner.state.lock().expect("embedder state lock") = resolved;
            });
        }
    }

    async fn load(model_dir: &Path) -> Result<StaticModel> {
        download_missing_files(model_dir).await?;
        let dir = model_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            StaticModel::from_pretrained(&dir, None, None, None)
                .with_context(|| format!("loading memory embedding model from {}", dir.display()))
        })
        .await
        .context("embedding model load task failed")?
    }

    /// Fetch the three model files bonsai needs into its own cache (tmp +
    /// rename, resumable by re-download). No hf-hub: the in-tree reqwest
    /// follows the CDN redirects fine and bonsai owns its footprint.
    async fn download_missing_files(model_dir: &Path) -> Result<()> {
        if MODEL_FILES
            .iter()
            .all(|name| model_dir.join(name).is_file())
        {
            return Ok(());
        }
        tokio::fs::create_dir_all(model_dir)
            .await
            .with_context(|| format!("creating {}", model_dir.display()))?;
        let client = reqwest::Client::new();
        for name in MODEL_FILES {
            let target = model_dir.join(name);
            if target.is_file() {
                continue;
            }
            let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{name}");
            let response = client
                .get(&url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| format!("downloading {url}"))?;
            let bytes = response
                .bytes()
                .await
                .with_context(|| format!("reading {url}"))?;
            let tmp = model_dir.join(format!(".{name}.{}.tmp", std::process::id()));
            tokio::fs::write(&tmp, &bytes)
                .await
                .with_context(|| format!("writing {}", tmp.display()))?;
            tokio::fs::rename(&tmp, &target)
                .await
                .with_context(|| format!("renaming into place: {}", target.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_utils {
    use super::*;

    /// Deterministic embedder for fusion tests — no model download in CI.
    /// Vectors come from a fixed table; unknown texts embed orthogonally.
    pub(crate) struct FakeEmbedder {
        pub table: Vec<(&'static str, Vec<f32>)>,
    }

    impl Embedder for FakeEmbedder {
        fn model_id(&self) -> &str {
            "fake-test-model"
        }

        fn availability(&self) -> EmbedderAvailability {
            EmbedderAvailability::Ready
        }

        fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|text| {
                    self.table
                        .iter()
                        .find(|(key, _)| text.contains(key))
                        .map(|(_, vector)| vector.clone())
                        .unwrap_or_else(|| vec![0.0, 0.0, 0.0, 1.0])
                })
                .collect())
        }
    }
}
