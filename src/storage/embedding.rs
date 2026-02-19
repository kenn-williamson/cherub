//! Embedding provider abstraction and OpenAI implementation (M6c).
//!
//! `EmbeddingProvider` is a true `dyn Trait` boundary — the backend is selected at
//! runtime based on environment configuration (`OPENAI_API_KEY`).

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tracing::Instrument;

use crate::error::CherubError;

/// Produces dense vector embeddings from text.
///
/// This is a legitimate `dyn Trait` boundary — the embedding backend is selected at
/// runtime (`OpenAiEmbeddingProvider`, or a mock in tests) and stored as
/// `Option<Arc<dyn EmbeddingProvider>>` in `PgMemoryStore`.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Dimension of the vectors produced (e.g., 1536 for text-embedding-3-small).
    fn dimension(&self) -> usize;

    /// Model name, for logging / metadata.
    fn model_name(&self) -> &str;

    /// Embed a single text string.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, CherubError>;

    /// Embed multiple strings. Default: sequential calls to `embed()`.
    /// Implementations should override with a batched HTTP call when possible.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CherubError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

// ─── OpenAI implementation ────────────────────────────────────────────────────

const OPENAI_EMBED_URL: &str = "https://api.openai.com/v1/embeddings";
const OPENAI_MODEL: &str = "text-embedding-3-small";
const OPENAI_DIMENSION: usize = 1536;

/// Calls the OpenAI Embeddings API (`text-embedding-3-small`, 1536 dimensions).
///
/// The API key is kept in a `SecretString`. `expose_secret()` is called only
/// inside this module at the point where the HTTP request is built.
pub struct OpenAiEmbeddingProvider {
    client: reqwest::Client,
    // CREDENTIAL: SecretString wraps the API key — Debug impl auto-redacts.
    api_key: SecretString,
}

impl OpenAiEmbeddingProvider {
    /// Create a new provider. Builds a `reqwest::Client` with standard timeouts.
    pub fn new(api_key: SecretString) -> Result<Self, CherubError> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| CherubError::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(Self { client, api_key })
    }

    /// POST a batch of texts to the Embeddings API and return all vectors.
    async fn call_api(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, CherubError> {
        let span = tracing::info_span!("openai_embed", count = input.len(), model = OPENAI_MODEL);

        async {
            // CREDENTIAL: expose_secret() is the only access point — used only here.
            let resp = self
                .client
                .post(OPENAI_EMBED_URL)
                .bearer_auth(self.api_key.expose_secret())
                .json(&serde_json::json!({
                    "input": input,
                    "model": OPENAI_MODEL,
                }))
                .send()
                .await
                .map_err(|e| CherubError::Provider(format!("embedding request failed: {e}")))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(CherubError::Provider(format!(
                    "embedding API error {status}: {body}"
                )));
            }

            let body: serde_json::Value = resp.json().await.map_err(|e| {
                CherubError::Provider(format!("failed to parse embedding response: {e}"))
            })?;

            let data = body["data"].as_array().ok_or_else(|| {
                CherubError::Provider("embedding response missing 'data' array".into())
            })?;

            // The API returns objects in the same order as the input.
            data.iter()
                .map(|item| {
                    item["embedding"]
                        .as_array()
                        .ok_or_else(|| {
                            CherubError::Provider("embedding item missing 'embedding' array".into())
                        })
                        .and_then(|arr| {
                            arr.iter()
                                .map(|v| {
                                    v.as_f64().map(|f| f as f32).ok_or_else(|| {
                                        CherubError::Provider(
                                            "embedding value is not a number".into(),
                                        )
                                    })
                                })
                                .collect::<Result<Vec<f32>, _>>()
                        })
                })
                .collect()
        }
        .instrument(span)
        .await
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    fn dimension(&self) -> usize {
        OPENAI_DIMENSION
    }

    fn model_name(&self) -> &str {
        OPENAI_MODEL
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, CherubError> {
        let mut results = self.call_api(vec![text.to_owned()]).await?;
        results
            .pop()
            .ok_or_else(|| CherubError::Provider("embedding API returned empty data".into()))
    }

    /// Batched: single HTTP call for all texts.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CherubError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.call_api(texts.to_vec()).await
    }
}
