use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::header,
    middleware::Next,
    response::Response,
};
use common_errors::AppErrorMarker;

/// Error bodies are tiny; this bound only guards against a runaway body.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Adds `error.request_id` to responses produced by `AppError` (design §13.1).
///
/// `AppError::into_response` has no access to request state, so it tags its
/// response with `AppErrorMarker`. This layer sits inside `SetRequestIdLayer`,
/// so the `x-request-id` header is already present on the request.
///
/// Known gap: `TimeoutLayer` produces a bare `408` without the envelope.
pub async fn inject_request_id(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let response = next.run(request).await;

    let Some(request_id) = request_id else {
        return response;
    };
    if response.extensions().get::<AppErrorMarker>().is_none() {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_ERROR_BODY_BYTES).await else {
        return Response::from_parts(parts, Body::empty());
    };

    let rewritten = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|mut json| {
            json.get_mut("error")?
                .as_object_mut()?
                .insert("request_id".into(), request_id.into());
            serde_json::to_vec(&json).ok()
        });

    let bytes = match rewritten {
        Some(new_bytes) => new_bytes,
        None => bytes.to_vec(),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use axum::{
        http::{Request, StatusCode},
        middleware::from_fn,
        routing::get,
        Router,
    };
    use common_errors::AppError;
    use tower::ServiceExt;

    use super::*;

    async fn fail() -> Result<&'static str, AppError> {
        Err(AppError::NotFound)
    }

    async fn plain_json() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({ "error": { "code": "NOT_AN_APP_ERROR" } }))
    }

    fn app() -> Router {
        Router::new()
            .route("/fail", get(fail))
            .route("/plain", get(plain_json))
            .layer(from_fn(inject_request_id))
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn app_error_bodies_gain_request_id() {
        let request = Request::builder()
            .uri("/fail")
            .header("x-request-id", "req-123")
            .body(Body::empty())
            .unwrap();
        let response = app().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = body_json(response).await;
        assert_eq!(json["error"]["request_id"], "req-123");
        assert_eq!(json["error"]["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn non_error_responses_are_untouched() {
        let request = Request::builder()
            .uri("/plain")
            .header("x-request-id", "req-123")
            .body(Body::empty())
            .unwrap();
        let json = body_json(app().oneshot(request).await.unwrap()).await;

        assert!(json["error"].get("request_id").is_none());
    }
}
