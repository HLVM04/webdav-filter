use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, TryStreamExt, stream};
use http::{HeaderMap, Method, StatusCode, header};
use percent_encoding::percent_decode_str;
use quick_xml::{Reader, events::Event};
use reqwest::{Client, Response};
use tokio::time::timeout;
use url::Url;

use crate::{
    config::UpstreamConfig,
    model::{Resource, SourceItem},
};

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:getetag/><d:getcontenttype/></d:prop></d:propfind>"#;

#[derive(Clone)]
pub struct Upstream {
    client: Client,
    base: Url,
    username: Option<String>,
    password: Option<String>,
    header_timeout: Duration,
    concurrency: usize,
}

impl Upstream {
    pub fn new(config: &UpstreamConfig, password: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("build upstream HTTP client")?;
        let mut base = config.url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            client,
            base,
            username: config.username.clone(),
            password,
            header_timeout: Duration::from_secs(config.header_timeout_secs),
            concurrency: config.crawl_concurrency,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn request(&self, method: Method, source_path: &str) -> reqwest::RequestBuilder {
        let mut url = self.base.clone();
        url.set_path(source_path);
        self.authorize(self.client.request(method, url))
    }

    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.username {
            Some(username) => request.basic_auth(username, self.password.as_ref()),
            None => request,
        }
    }

    pub async fn send_headers(&self, request: reqwest::RequestBuilder) -> Result<Response> {
        timeout(self.header_timeout, request.send())
            .await
            .context("upstream header timeout")?
            .context("upstream request")
    }

    pub async fn discover(
        &self,
        previous: &HashMap<String, SourceItem>,
        full: bool,
    ) -> Result<Vec<SourceItem>> {
        let root_path = self.base.path().trim_end_matches('/').to_owned() + "/";
        let roots = self.propfind(&root_path).await?;
        let root_key = normalize_collection(&root_path);
        let children: Vec<Resource> = roots
            .into_iter()
            .filter(|r| normalize_collection(&r.source_path) != root_key)
            .collect();
        stream::iter(children)
            .map(|root| async move {
                if !full
                    && let Some(cached) = previous.get(&root.source_path)
                    && cached.root == root
                {
                    return Ok(cached.clone());
                }
                self.crawl_item(root).await
            })
            .buffer_unordered(self.concurrency)
            .try_collect()
            .await
    }

    async fn crawl_item(&self, root: Resource) -> Result<SourceItem> {
        let id = root.source_path.clone();
        if !root.is_collection {
            return Ok(SourceItem {
                id,
                root,
                descendants: Vec::new(),
            });
        }
        let mut queue = VecDeque::from([format!("{}/", root.source_path.trim_end_matches('/'))]);
        let mut seen = HashSet::from([normalize_collection(&root.source_path)]);
        let mut descendants = Vec::new();
        while let Some(path) = queue.pop_front() {
            for resource in self.propfind(&path).await? {
                let key = normalize_collection(&resource.source_path);
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                if resource.is_collection {
                    queue.push_back(resource.source_path.clone());
                }
                descendants.push(resource);
            }
        }
        Ok(SourceItem {
            id,
            root,
            descendants,
        })
    }

    pub async fn propfind(&self, source_path: &str) -> Result<Vec<Resource>> {
        let method = Method::from_bytes(b"PROPFIND").unwrap();
        let response = self
            .send_headers(
                self.request(method, source_path)
                    .header("Depth", "1")
                    .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(PROPFIND_BODY),
            )
            .await?;
        if response.status() != StatusCode::MULTI_STATUS && !response.status().is_success() {
            bail!("PROPFIND {} returned {}", source_path, response.status());
        }
        let bytes = response.bytes().await.context("read PROPFIND response")?;
        parse_multistatus(&bytes)
    }

    pub async fn delete_item(&self, source_path: &str) -> Result<()> {
        let response = self
            .send_headers(self.request(Method::DELETE, source_path))
            .await?;
        if !response.status().is_success() {
            bail!("upstream DELETE returned {}", response.status());
        }
        Ok(())
    }

    pub async fn call_hook(
        &self,
        url: Url,
        method: Method,
        headers: &HeaderMap,
        reuse_auth: bool,
    ) -> Result<()> {
        let mut request = self.client.request(method, url).headers(headers.clone());
        if reuse_auth {
            request = self.authorize(request);
        }
        let response = self.send_headers(request).await?;
        if !response.status().is_success() {
            bail!("refresh hook returned {}", response.status());
        }
        Ok(())
    }
}

#[derive(Default)]
struct DavResponse {
    href: String,
    collection: bool,
    size: u64,
    modified: Option<DateTime<Utc>>,
    etag: Option<String>,
    content_type: Option<String>,
}

fn parse_multistatus(xml: &[u8]) -> Result<Vec<Resource>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut resources = Vec::new();
    let mut response: Option<DavResponse> = None;
    let mut active = String::new();
    loop {
        match reader.read_event().context("parse WebDAV XML")? {
            Event::Start(tag) => {
                let name = local_name(tag.name().as_ref());
                if name == "response" {
                    response = Some(DavResponse::default());
                }
                if name == "collection"
                    && let Some(value) = response.as_mut()
                {
                    value.collection = true;
                }
                active = name;
            }
            Event::Empty(tag) => {
                if local_name(tag.name().as_ref()) == "collection"
                    && let Some(value) = response.as_mut()
                {
                    value.collection = true;
                }
            }
            Event::Text(text) => {
                if let Some(value) = response.as_mut() {
                    let decoded = text.decode().context("decode WebDAV XML")?.into_owned();
                    let text = quick_xml::escape::unescape(&decoded)
                        .context("unescape WebDAV XML")?
                        .into_owned();
                    match active.as_str() {
                        "href" => value.href = text,
                        "getcontentlength" => value.size = text.parse().unwrap_or(0),
                        "getlastmodified" => {
                            value.modified = httpdate::parse_http_date(&text)
                                .ok()
                                .map(DateTime::<Utc>::from)
                        }
                        "getetag" => value.etag = Some(text),
                        "getcontenttype" => value.content_type = Some(text),
                        _ => {}
                    }
                }
            }
            Event::End(tag) => {
                let name = local_name(tag.name().as_ref());
                if name == "response"
                    && let Some(value) = response.take()
                    && let Some(resource) = value.into_resource()
                {
                    resources.push(resource);
                }
                active.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(resources)
}

impl DavResponse {
    fn into_resource(self) -> Option<Resource> {
        if self.href.is_empty() {
            return None;
        }
        let raw_path = Url::parse(&self.href)
            .map(|url| url.path().to_owned())
            .unwrap_or(self.href);
        let mut source_path = percent_decode_str(&raw_path)
            .decode_utf8_lossy()
            .into_owned();
        if source_path.len() > 1 {
            source_path.truncate(source_path.trim_end_matches('/').len());
        }
        let name = source_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        Some(Resource {
            source_path,
            name,
            is_collection: self.collection,
            size: self.size,
            modified: self.modified,
            etag: self.etag,
            content_type: self.content_type,
        })
    }
}

fn local_name(name: &[u8]) -> String {
    let name = std::str::from_utf8(name).unwrap_or_default();
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

fn normalize_collection(path: &str) -> String {
    path.trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaced_multistatus() {
        let xml = br#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/Film%20One/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>abc</d:getetag></d:prop></d:propstat></d:response></d:multistatus>"#;
        let values = parse_multistatus(xml).unwrap();
        assert_eq!(values[0].name, "Film One");
        assert!(values[0].is_collection);
    }
}
