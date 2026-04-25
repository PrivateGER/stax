use axum::{
    extract::{Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{CONTENT_TYPE, HOST, ORIGIN, RANGE},
    },
    middleware::Next,
    response::Response,
};
use tower_http::cors::CorsLayer;

pub(crate) fn build_cors_with_origin(origin: Option<HeaderValue>) -> CorsLayer {
    match origin {
        Some(origin) => CorsLayer::new()
            .allow_origin(origin)
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([CONTENT_TYPE, RANGE]),
        None => CorsLayer::new().allow_methods([Method::GET, Method::POST]),
    }
}

pub(crate) async fn reject_cross_origin_requests(
    State(frontend_origin): State<Option<HeaderValue>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if origin_allowed(request.headers(), frontend_origin.as_ref()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

pub(crate) fn origin_allowed(headers: &HeaderMap, configured_origin: Option<&HeaderValue>) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };

    if let Some(configured_origin) = configured_origin {
        return origin == configured_origin;
    }

    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };

    origin == format!("http://{host}") || origin == format!("https://{host}")
}
