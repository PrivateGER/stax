use axum::{
    body::Body,
    http::{
        HeaderValue, StatusCode, Uri,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::Response,
};

include!(concat!(env!("OUT_DIR"), "/frontend_assets.rs"));

const INDEX_PATH: &str = "index.html";

pub(crate) async fn serve(uri: Uri) -> Response {
    let path = uri.path();
    if is_api_path(path) {
        return not_found();
    }

    let asset_path = request_asset_path(path);
    if let Some(asset) = find_asset(asset_path) {
        return asset_response(asset);
    }

    if should_fallback_to_index(asset_path) {
        if let Some(index) = find_asset(INDEX_PATH) {
            return asset_response(index);
        }
    }

    not_found()
}

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

fn request_asset_path(path: &str) -> &str {
    let path = path.trim_start_matches('/');
    if path.is_empty() { INDEX_PATH } else { path }
}

fn find_asset(path: &str) -> Option<&'static EmbeddedAsset> {
    EMBEDDED_FRONTEND_ASSETS
        .iter()
        .find(|asset| asset.path == path)
}

fn should_fallback_to_index(path: &str) -> bool {
    if path.starts_with("assets/") {
        return false;
    }

    !path.rsplit('/').next().unwrap_or(path).contains('.')
}

fn asset_response(asset: &'static EmbeddedAsset) -> Response {
    let mut response = Response::new(Body::from(asset.contents.to_vec()));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(content_type(asset.path)),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(cache_control(asset.path)),
    );
    response
}

fn not_found() -> Response {
    let mut response = Response::new(Body::from("Not Found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit('.').next().unwrap_or_default();

    match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn cache_control(path: &str) -> &'static str {
    if path == INDEX_PATH {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}
