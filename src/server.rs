use std::{sync::Arc, time::Instant};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::Response,
    routing::{any, get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use metrics::{counter, histogram};
use metrics_exporter_prometheus::PrometheusHandle;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use quick_xml::escape::escape;
use subtle::ConstantTimeEq;
use tracing::{error, warn};

use crate::{index::Index, model::VirtualResource, upstream::Upstream};

#[derive(Clone)]
pub struct AppState {
    pub index: Arc<Index>,
    pub upstream: Upstream,
    pub downstream_auth: Option<(String, String)>,
    pub delete_enabled: bool,
    pub metrics: PrometheusHandle,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/-/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/-/readyz", get(ready))
        .route("/-/metrics", get(metrics))
        .route("/-/refresh", post(refresh))
        .route("/", any(dav))
        .route("/{*path}", any(dav))
        .with_state(state)
}

async fn ready(State(state): State<AppState>) -> StatusCode {
    if state.index.ready() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn metrics(State(state): State<AppState>, request: Request) -> Response {
    if let Some(response) = require_auth(&state, request.headers()) {
        return response;
    }
    response(
        StatusCode::OK,
        state.metrics.render(),
        Some("text/plain; version=0.0.4"),
    )
}

async fn refresh(State(state): State<AppState>, request: Request) -> Response {
    if let Some(response) = require_auth(&state, request.headers()) {
        return response;
    }
    let index = state.index.clone();
    tokio::spawn(async move {
        if let Err(error) = index.refresh().await {
            error!(%error, "manual refresh failed");
        }
    });
    empty(StatusCode::ACCEPTED)
}

async fn dav(State(state): State<AppState>, request: Request) -> Response {
    let started = Instant::now();
    if let Some(response) = require_auth(&state, request.headers()) {
        return response;
    }
    let method = request.method().clone();
    let result = match method.as_str() {
        "OPTIONS" => options(),
        "PROPFIND" => propfind(&state, &request),
        "GET" | "HEAD" => proxy(&state, request).await,
        "DELETE" => delete(&state, request).await,
        _ => {
            let mut value = empty(StatusCode::METHOD_NOT_ALLOWED);
            value.headers_mut().insert(
                header::ALLOW,
                HeaderValue::from_static("OPTIONS, PROPFIND, HEAD, GET, DELETE"),
            );
            value
        }
    };
    counter!("webdav_filter_requests_total", "method" => method.to_string(), "status" => result.status().as_u16().to_string()).increment(1);
    histogram!("webdav_filter_request_duration_seconds", "method" => method.to_string())
        .record(started.elapsed().as_secs_f64());
    result
}

fn options() -> Response {
    let mut value = empty(StatusCode::NO_CONTENT);
    value.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_static("OPTIONS, PROPFIND, HEAD, GET, DELETE"),
    );
    value
        .headers_mut()
        .insert("DAV", HeaderValue::from_static("1"));
    value
}

fn propfind(state: &AppState, request: &Request) -> Response {
    let depth = request
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1");
    if depth != "0" && depth != "1" {
        return response(
            StatusCode::FORBIDDEN,
            "<?xml version=\"1.0\"?><d:error xmlns:d=\"DAV:\"><d:propfind-finite-depth/></d:error>",
            Some("application/xml; charset=utf-8"),
        );
    }
    let dav_prefix = has_dav_prefix(request.uri().path());
    let path = match canonical_path(request.uri().path()) {
        Ok(path) => path,
        Err(status) => return empty(status),
    };
    let snapshot = state.index.snapshot();
    let Some(resource) = snapshot.get(&path) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let mut values = vec![resource];
    if depth == "1" && resource.resource.is_collection {
        values.extend(snapshot.children_of(&path));
    }
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><d:multistatus xmlns:d=\"DAV:\">");
    for value in values {
        write_prop_response(&mut xml, value, dav_prefix);
    }
    xml.push_str("</d:multistatus>");
    response(
        StatusCode::MULTI_STATUS,
        xml,
        Some("application/xml; charset=utf-8"),
    )
}

async fn proxy(state: &AppState, request: Request) -> Response {
    let path = match canonical_path(request.uri().path()) {
        Ok(path) => path,
        Err(status) => return empty(status),
    };
    let snapshot = state.index.snapshot();
    let Some(resource) = snapshot.get(&path) else {
        return empty(StatusCode::NOT_FOUND);
    };
    if resource.resource.is_collection && request.method() == Method::HEAD {
        let mut response = empty(StatusCode::OK);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("httpd/unix-directory"),
        );
        return response;
    }
    let Some(source_path) = &resource.source_path else {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    };
    if resource.resource.is_collection {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    let mut upstream = state
        .upstream
        .request(request.method().clone(), source_path);
    for name in [
        header::RANGE,
        header::IF_RANGE,
        header::IF_MATCH,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::IF_UNMODIFIED_SINCE,
    ] {
        if let Some(value) = request.headers().get(&name) {
            upstream = upstream.header(name, value);
        }
    }
    let upstream = match state.upstream.send_headers(upstream).await {
        Ok(value) => value,
        Err(error) => {
            warn!(%error, %source_path, "stream request failed");
            return empty(StatusCode::BAD_GATEWAY);
        }
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    copy_response_headers(&headers, response.headers_mut());
    response
}

async fn delete(state: &AppState, request: Request) -> Response {
    if !state.delete_enabled {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    let path = match canonical_path(request.uri().path()) {
        Ok(path) => path,
        Err(status) => return empty(status),
    };
    let snapshot = state.index.snapshot();
    let Some(resource) = snapshot.get(&path) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let Some(item_root) = resource.item_root.clone() else {
        return empty(StatusCode::FORBIDDEN);
    };
    match state.index.delete_item(&item_root).await {
        Ok(()) => empty(StatusCode::NO_CONTENT),
        Err(error) => {
            warn!(%error, %item_root, "delete failed");
            empty(StatusCode::BAD_GATEWAY)
        }
    }
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let Some((username, password)) = &state.downstream_auth else {
        return None;
    };
    let valid = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .map(|decoded| {
            let expected = format!("{username}:{password}");
            decoded.len() == expected.len() && bool::from(decoded.ct_eq(expected.as_bytes()))
        })
        .unwrap_or(false);
    if valid {
        return None;
    }
    let mut response = empty(StatusCode::UNAUTHORIZED);
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"webdav-filter\", charset=\"UTF-8\""),
    );
    Some(response)
}

fn canonical_path(raw: &str) -> Result<String, StatusCode> {
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let decoded = if let Some(rest) = decoded.strip_prefix("/dav") {
        if rest.is_empty() || rest.starts_with('/') {
            if rest.is_empty() { "/" } else { rest }
        } else {
            decoded.as_ref()
        }
    } else {
        decoded.as_ref()
    };
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(StatusCode::BAD_REQUEST),
            value if value.contains('\0') => return Err(StatusCode::BAD_REQUEST),
            value => segments.push(value),
        }
    }
    Ok(if segments.is_empty() {
        "/".into()
    } else {
        format!("/{}", segments.join("/"))
    })
}

fn has_dav_prefix(raw: &str) -> bool {
    let Ok(decoded) = percent_decode_str(raw).decode_utf8() else {
        return false;
    };
    decoded == "/dav" || decoded.starts_with("/dav/")
}

fn write_prop_response(xml: &mut String, value: &VirtualResource, dav_prefix: bool) {
    let href = encode_path(
        &value.virtual_path,
        value.resource.is_collection,
        dav_prefix,
    );
    xml.push_str("<d:response><d:href>");
    xml.push_str(&escape(&href));
    xml.push_str("</d:href><d:propstat><d:prop><d:displayname>");
    xml.push_str(&escape(&value.resource.name));
    xml.push_str("</d:displayname><d:resourcetype>");
    if value.resource.is_collection {
        xml.push_str("<d:collection/>");
    }
    xml.push_str("</d:resourcetype>");
    if !value.resource.is_collection {
        xml.push_str(&format!(
            "<d:getcontentlength>{}</d:getcontentlength>",
            value.resource.size
        ));
    }
    if let Some(modified) = value.resource.modified {
        xml.push_str(&format!(
            "<d:getlastmodified>{}</d:getlastmodified>",
            httpdate::fmt_http_date(modified.into())
        ));
    }
    if let Some(etag) = &value.resource.etag {
        xml.push_str("<d:getetag>");
        xml.push_str(&escape(etag));
        xml.push_str("</d:getetag>");
    }
    if let Some(content_type) = &value.resource.content_type {
        xml.push_str("<d:getcontenttype>");
        xml.push_str(&escape(content_type));
        xml.push_str("</d:getcontenttype>");
    } else if !value.resource.is_collection {
        xml.push_str("<d:getcontenttype>application/octet-stream</d:getcontenttype>");
    }
    xml.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>");
}

fn encode_path(path: &str, collection: bool, dav_prefix: bool) -> String {
    let mut encoded = path
        .split('/')
        .map(|segment| utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string())
        .collect::<Vec<_>>()
        .join("/");
    if dav_prefix {
        encoded = if encoded == "/" {
            "/dav/".into()
        } else {
            format!("/dav{encoded}")
        };
    }
    if collection && !encoded.ends_with('/') {
        encoded.push('/');
    }
    encoded
}

fn copy_response_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for name in [
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CONTENT_TYPE,
        header::CONTENT_DISPOSITION,
        header::ETAG,
        header::LAST_MODIFIED,
        header::CACHE_CONTROL,
        header::EXPIRES,
    ] {
        if let Some(value) = source.get(&name) {
            target.insert(name, value.clone());
        }
    }
}

fn response(
    status: StatusCode,
    body: impl Into<Body>,
    content_type: Option<&'static str>,
) -> Response {
    let mut value = Response::new(body.into());
    *value.status_mut() = status;
    if let Some(content_type) = content_type {
        value
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    value
}

fn empty(status: StatusCode) -> Response {
    response(status, Body::empty(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_and_traversal_rejected() {
        assert_eq!(
            canonical_path("/movies/Film%20One").unwrap(),
            "/movies/Film One"
        );
        assert_eq!(canonical_path("/").unwrap(), "/");
        assert_eq!(canonical_path("/dav").unwrap(), "/");
        assert_eq!(canonical_path("/dav/movies").unwrap(), "/movies");
        assert_eq!(encode_path("/", true, true), "/dav/");
        assert_eq!(encode_path("/movies", true, true), "/dav/movies/");
        assert!(canonical_path("/movies/../secret").is_err());
    }
}
