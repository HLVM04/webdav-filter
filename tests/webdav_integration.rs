use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{Router, body::Body, extract::State, response::Response, routing::any};
use http::{HeaderValue, Method, StatusCode, header};
use metrics_exporter_prometheus::PrometheusBuilder;
use tempfile::tempdir;
use tokio::net::TcpListener;
use url::Url;
use webdav_filter::{
    config::{Config, DirectoryConfig},
    filter::Classifier,
    index::Index,
    server::{AppState, router},
    store::SnapshotStore,
    upstream::Upstream,
};

#[derive(Clone)]
struct MockState(Arc<AtomicBool>);

#[tokio::test]
async fn lists_ranges_and_deletes_complete_items() {
    let deleted = Arc::new(AtomicBool::new(false));
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream_listener.local_addr().unwrap();
    let upstream_app = Router::new()
        .route("/{*path}", any(mock_dav))
        .route("/", any(mock_dav))
        .with_state(MockState(deleted.clone()));
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app).await.unwrap();
    });

    let directory = tempdir().unwrap();
    let yaml = format!(
        r#"
version: 1
upstream:
  url: http://{upstream_address}/dav/
  allow_http: true
refresh:
  interval_secs: 60
delete:
  enabled: true
state:
  database: "{}"
directories:
  movies:
    group: media
    group_order: 30
    filters:
      - regex: /.*/
"#,
        directory.path().join("index.db").display()
    );
    let config: Config = serde_yaml::from_str(&yaml).unwrap();
    config.validate().unwrap();
    let configs: &BTreeMap<String, DirectoryConfig> = &config.directories;
    let classifier = Classifier::compile(configs).unwrap();
    let upstream = Upstream::new(&config.upstream, None).unwrap();
    let store = SnapshotStore::open(&config.state.database).unwrap();
    let index = Index::new(&config, upstream.clone(), classifier, store).unwrap();
    assert!(index.refresh().await.unwrap());

    let metrics = PrometheusBuilder::new().install_recorder().unwrap();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = proxy_listener.local_addr().unwrap();
    let proxy_app = router(AppState {
        index,
        upstream,
        downstream_auth: None,
        delete_enabled: true,
        metrics,
    });
    let proxy_task = tokio::spawn(async move {
        axum::serve(proxy_listener, proxy_app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let base = Url::parse(&format!("http://{proxy_address}/")).unwrap();

    let listing = client
        .request(
            Method::from_bytes(b"PROPFIND").unwrap(),
            base.join("movies/Film/").unwrap(),
        )
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(listing.status(), StatusCode::MULTI_STATUS);
    assert!(listing.text().await.unwrap().contains("video.mkv"));

    let ranged = client
        .get(base.join("movies/Film/video.mkv").unwrap())
        .header(header::RANGE, "bytes=2-5")
        .send()
        .await
        .unwrap();
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(ranged.bytes().await.unwrap().as_ref(), b"2345");

    let removed = client
        .delete(base.join("movies/Film/video.mkv").unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert!(deleted.load(Ordering::SeqCst));

    let missing = client
        .get(base.join("movies/Film/video.mkv").unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    proxy_task.abort();
    upstream_task.abort();
}

async fn mock_dav(State(state): State<MockState>, request: axum::extract::Request) -> Response {
    let path = request.uri().path();
    match request.method().as_str() {
        "PROPFIND" if path == "/dav/" => xml(
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
<d:response><d:href>/dav/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/dav/Film/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>film-1</d:getetag></d:prop></d:propstat></d:response>
</d:multistatus>"#.into()
        ),
        "PROPFIND" if path == "/dav/Film" || path == "/dav/Film/" => xml(
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
<d:response><d:href>/dav/Film/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/dav/Film/video.mkv</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>10</d:getcontentlength><d:getcontenttype>video/x-matroska</d:getcontenttype></d:prop></d:propstat></d:response>
</d:multistatus>"#.into()
        ),
        "GET" if path == "/dav/Film/video.mkv" => {
            let ranged = request.headers().get(header::RANGE).is_some();
            let body: &'static [u8] = if ranged { b"2345" } else { b"0123456789" };
            let mut response = Response::new(Body::from(body));
            if ranged {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_static("bytes 2-5/10"),
                );
            }
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&body.len().to_string()).unwrap(),
            );
            response
                .headers_mut()
                .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            response
        }
        "DELETE" if path == "/dav/Film" || path == "/dav/Film/" => {
            state.0.store(true, Ordering::SeqCst);
            status(StatusCode::NO_CONTENT)
        }
        _ => status(StatusCode::NOT_FOUND),
    }
}

fn xml(body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::MULTI_STATUS;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    response
}

fn status(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}
