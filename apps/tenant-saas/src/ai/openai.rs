//! Client for OpenAI-compatible `/embeddings` endpoints.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;

use super::embedding::{validate_vectors, EmbeddingError, EmbeddingProvider};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BODY_SLACK_BYTES: usize = 1024 * 1024;
/// Generous upper bound for one JSON-encoded f32 plus separator.
const BYTES_PER_VALUE: usize = 24;

/// Deliberately not `Debug`: it holds the API key.
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    dim: usize,
}

#[derive(Deserialize)]
struct ResponseBody {
    data: Vec<Item>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Item {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u64>,
}

impl OpenAiEmbedder {
    pub fn new(
        base_url: Option<&str>,
        api_key: String,
        model: String,
        dim: usize,
        timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        let base = base_url
            .filter(|b| !b.trim().is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        Ok(Self {
            client,
            url: format!("{base}/embeddings"),
            api_key,
            model,
            dim,
        })
    }

    fn status_error(status: StatusCode, retry_after: Option<Duration>) -> EmbeddingError {
        match status.as_u16() {
            429 => EmbeddingError::Transient {
                reason: "provider rate limited the request".into(),
                retry_after,
            },
            401 | 403 => {
                tracing::error!(
                    status = status.as_u16(),
                    "embedding provider rejected credentials; check EMBEDDING_API_KEY"
                );
                EmbeddingError::transient("provider rejected credentials (check EMBEDDING_API_KEY)")
            }
            500..=599 => {
                EmbeddingError::transient(format!("provider returned HTTP {}", status.as_u16()))
            }
            400..=499 => EmbeddingError::Permanent {
                reason: format!("provider returned HTTP {}", status.as_u16()),
            },
            _ => EmbeddingError::invalid(format!("unexpected HTTP status {}", status.as_u16())),
        }
    }

    async fn read_capped(
        mut resp: reqwest::Response,
        cap: usize,
    ) -> Result<Vec<u8>, EmbeddingError> {
        if resp.content_length().is_some_and(|len| len > cap as u64) {
            return Err(EmbeddingError::invalid("response body too large"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|_| EmbeddingError::transient("failed to read response body"))?
        {
            if body.len() + chunk.len() > cap {
                return Err(EmbeddingError::invalid("response body too large"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let resp = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "model": self.model,
                "input": inputs,
                "encoding_format": "float",
            }))
            .send()
            .await
            .map_err(|e| {
                EmbeddingError::transient(if e.is_timeout() {
                    "request timed out"
                } else if e.is_connect() {
                    "connection failed"
                } else {
                    "request failed"
                })
            })?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(Self::status_error(status, retry_after));
        }

        let cap = inputs
            .len()
            .saturating_mul(self.dim)
            .saturating_mul(BYTES_PER_VALUE)
            .saturating_add(BODY_SLACK_BYTES);
        let bytes = Self::read_capped(resp, cap).await?;

        let mut parsed: ResponseBody = serde_json::from_slice(&bytes)
            .map_err(|_| EmbeddingError::invalid("malformed response body"))?;

        if let Some(tokens) = parsed.usage.and_then(|u| u.prompt_tokens) {
            tracing::debug!(
                usage_tokens = tokens,
                inputs = inputs.len(),
                "embeddings usage"
            );
        }

        // Providers do not promise order; indices must be exactly 0..n.
        parsed.data.sort_by_key(|item| item.index);
        if parsed.data.len() != inputs.len()
            || parsed
                .data
                .iter()
                .enumerate()
                .any(|(i, item)| item.index != i)
        {
            return Err(EmbeddingError::invalid(
                "response indices do not match inputs",
            ));
        }
        let vectors: Vec<Vec<f32>> = parsed.data.into_iter().map(|i| i.embedding).collect();
        validate_vectors(inputs.len(), self.dim, &vectors)?;
        Ok(vectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, Bytes};
    use axum::http::{HeaderMap, HeaderValue};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use tracing_subscriber::fmt::MakeWriter;

    const KEY: &str = "sk-test-secret-key-123";
    const DIM: usize = 4;

    type Handler = Arc<dyn Fn(HeaderMap, Bytes) -> Response + Send + Sync>;

    async fn serve(handler: Handler) -> String {
        let app = Router::new().route(
            "/embeddings",
            post(move |headers: HeaderMap, body: Bytes| {
                let h = handler.clone();
                async move { h(headers, body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn embedder(base: &str) -> OpenAiEmbedder {
        OpenAiEmbedder::new(
            Some(base),
            KEY.into(),
            "text-embedding-3-small".into(),
            DIM,
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn inputs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("text {i}")).collect()
    }

    fn json_ok(items: &[(usize, [f32; 4])]) -> Response {
        let data: Vec<_> = items
            .iter()
            .map(|(i, v)| serde_json::json!({"object": "embedding", "index": i, "embedding": v}))
            .collect();
        axum::Json(serde_json::json!({
            "data": data,
            "usage": {"prompt_tokens": 7, "total_tokens": 7}
        }))
        .into_response()
    }

    fn status(code: u16) -> Handler {
        Arc::new(move |_, _| {
            (
                StatusCode::from_u16(code).unwrap(),
                "secret provider body that must not leak",
            )
                .into_response()
        })
    }

    async fn run_status(code: u16) -> EmbeddingError {
        let base = serve(status(code)).await;
        embedder(&base).embed(&inputs(1)).await.unwrap_err()
    }

    #[tokio::test]
    async fn valid_response_is_ok_and_request_is_well_formed() {
        let seen: Arc<Mutex<Option<(HeaderMap, serde_json::Value)>>> = Arc::new(Mutex::new(None));
        let s = seen.clone();
        let base = serve(Arc::new(move |h, b| {
            *s.lock().unwrap() = Some((h, serde_json::from_slice(&b).unwrap()));
            json_ok(&[(0, [1.0, 0.0, 0.0, 0.0]), (1, [0.0, 1.0, 0.0, 0.0])])
        }))
        .await;

        let out = embedder(&base).embed(&inputs(2)).await.unwrap();
        assert_eq!(
            out,
            vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]]
        );

        let (headers, body) = seen.lock().unwrap().take().unwrap();
        assert_eq!(
            headers.get("authorization"),
            Some(&HeaderValue::from_str(&format!("Bearer {KEY}")).unwrap())
        );
        assert_eq!(body["model"], "text-embedding-3-small");
        assert_eq!(body["encoding_format"], "float");
        assert_eq!(body["input"], serde_json::json!(["text 0", "text 1"]));
    }

    #[tokio::test]
    async fn shuffled_indices_are_reordered() {
        let base = serve(Arc::new(|_, _| {
            json_ok(&[
                (2, [0.0, 0.0, 1.0, 0.0]),
                (0, [1.0, 0.0, 0.0, 0.0]),
                (1, [0.0, 1.0, 0.0, 0.0]),
            ])
        }))
        .await;
        let out = embedder(&base).embed(&inputs(3)).await.unwrap();
        assert_eq!(out[0], vec![1.0, 0.0, 0.0, 0.0]);
        assert_eq!(out[1], vec![0.0, 1.0, 0.0, 0.0]);
        assert_eq!(out[2], vec![0.0, 0.0, 1.0, 0.0]);
    }

    #[tokio::test]
    async fn bad_bodies_are_invalid_responses() {
        let cases: Vec<(&str, Handler)> = vec![
            ("not json", Arc::new(|_, _| "nope".into_response())),
            ("wrong count", Arc::new(|_, _| json_ok(&[(0, [1.0; 4])]))),
            (
                "duplicate index",
                Arc::new(|_, _| json_ok(&[(0, [1.0; 4]), (0, [1.0; 4])])),
            ),
            (
                "gap in indices",
                Arc::new(|_, _| json_ok(&[(0, [1.0; 4]), (2, [1.0; 4])])),
            ),
            (
                "wrong dim",
                Arc::new(|_, _| {
                    axum::Json(serde_json::json!({"data": [
                        {"index": 0, "embedding": [1.0, 2.0]},
                        {"index": 1, "embedding": [1.0, 2.0]},
                    ]}))
                    .into_response()
                }),
            ),
            (
                "all zero",
                Arc::new(|_, _| json_ok(&[(0, [0.0; 4]), (1, [1.0; 4])])),
            ),
            (
                "out of range number",
                Arc::new(|_, _| {
                    (
                        [("content-type", "application/json")],
                        r#"{"data":[{"index":0,"embedding":[1e999,0,0,0]},{"index":1,"embedding":[1,0,0,0]}]}"#,
                    )
                        .into_response()
                }),
            ),
        ];
        for (name, handler) in cases {
            let base = serve(handler).await;
            let err = embedder(&base).embed(&inputs(2)).await.unwrap_err();
            assert!(
                matches!(err, EmbeddingError::InvalidResponse { .. }),
                "{name}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn status_429_is_transient_with_retry_after() {
        let base = serve(Arc::new(|_, _| {
            let mut r = (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
            r.headers_mut()
                .insert("retry-after", HeaderValue::from_static("17"));
            r
        }))
        .await;
        match embedder(&base).embed(&inputs(1)).await.unwrap_err() {
            EmbeddingError::Transient { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(17)));
            }
            e => panic!("{e:?}"),
        }
        // Missing or malformed header means no hint.
        match run_status(429).await {
            EmbeddingError::Transient { retry_after, .. } => assert_eq!(retry_after, None),
            e => panic!("{e:?}"),
        }
    }

    #[tokio::test]
    async fn server_errors_are_transient() {
        for code in [500, 502, 503, 504] {
            let err = run_status(code).await;
            assert!(
                matches!(err, EmbeddingError::Transient { .. }),
                "{code}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn other_client_errors_are_permanent() {
        for code in [400, 404, 413, 422] {
            let err = run_status(code).await;
            assert!(
                matches!(err, EmbeddingError::Permanent { .. }),
                "{code}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn error_reasons_never_contain_the_response_body() {
        for code in [400, 429, 500, 401] {
            let msg = run_status(code).await.to_string();
            assert!(!msg.contains("secret provider body"), "{code}: {msg}");
        }
    }

    #[tokio::test]
    async fn timeout_is_transient() {
        let app = Router::new().route(
            "/embeddings",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let e = OpenAiEmbedder::new(
            Some(&base),
            KEY.into(),
            "m".into(),
            DIM,
            Duration::from_millis(200),
        )
        .unwrap();
        let err = e.embed(&inputs(1)).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::Transient { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn connect_error_is_transient() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = embedder(&base).embed(&inputs(1)).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::Transient { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn oversized_body_with_content_length_is_rejected() {
        // cap for 1 input = 1 * 4 * 24 + 1 MiB
        let big = vec![b'x'; 2 * 1024 * 1024];
        let base = serve(Arc::new(move |_, _| {
            (StatusCode::OK, big.clone()).into_response()
        }))
        .await;
        let err = embedder(&base).embed(&inputs(1)).await.unwrap_err();
        assert!(
            matches!(err, EmbeddingError::InvalidResponse { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn oversized_streamed_body_is_cut_off() {
        let base = serve(Arc::new(|_, _| {
            let chunks =
                (0..64).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 64 * 1024])));
            Response::new(Body::from_stream(futures_util::stream::iter(chunks)))
        }))
        .await;
        let err = embedder(&base).embed(&inputs(1)).await.unwrap_err();
        assert!(
            matches!(err, EmbeddingError::InvalidResponse { .. }),
            "{err:?}"
        );
    }

    #[derive(Clone, Default)]
    struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    async fn auth_failure_logs_the_env_var_name_but_never_the_key() {
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        for code in [401, 403] {
            let err = run_status(code).await;
            assert!(
                matches!(err, EmbeddingError::Transient { .. }),
                "{code}: {err:?}"
            );
        }
        let base = serve(Arc::new(|_, _| json_ok(&[(0, [1.0; 4])]))).await;
        embedder(&base).embed(&inputs(1)).await.unwrap();

        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("EMBEDDING_API_KEY"), "{logs}");
        assert!(!logs.contains(KEY), "{logs}");
    }

    #[tokio::test]
    async fn empty_input_makes_no_request() {
        let e = embedder("http://127.0.0.1:1");
        assert!(e.embed(&[]).await.unwrap().is_empty());
    }
}
