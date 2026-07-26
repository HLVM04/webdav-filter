use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use metrics::{counter, gauge};
use tokio::{process::Command, sync::Mutex};
use tracing::{error, info, warn};

use crate::{
    config::{Config, RefreshHookConfig},
    filter::Classifier,
    model::{Snapshot, SourceItem},
    store::SnapshotStore,
    upstream::Upstream,
};

pub struct Index {
    snapshot: ArcSwap<Snapshot>,
    refresh_lock: Mutex<()>,
    hook_last_called: Mutex<Option<Instant>>,
    baseline_ready: Mutex<bool>,
    source_items: Mutex<HashMap<String, SourceItem>>,
    last_full_refresh: Mutex<Option<Instant>>,
    upstream: Upstream,
    classifier: Classifier,
    store: SnapshotStore,
    hook: Option<RefreshHookConfig>,
    update_command: Option<Vec<String>>,
    full_interval: Duration,
}

impl Index {
    pub fn new(
        config: &Config,
        upstream: Upstream,
        classifier: Classifier,
        store: SnapshotStore,
    ) -> Result<Arc<Self>> {
        let initial = store.load()?.unwrap_or_else(Snapshot::empty);
        let baseline_ready = initial.generation > 0;
        Ok(Arc::new(Self {
            snapshot: ArcSwap::from_pointee(initial),
            refresh_lock: Mutex::new(()),
            hook_last_called: Mutex::new(None),
            baseline_ready: Mutex::new(baseline_ready),
            source_items: Mutex::new(HashMap::new()),
            last_full_refresh: Mutex::new(None),
            upstream,
            classifier,
            store,
            hook: config.refresh.hook.clone(),
            update_command: config.on_library_update.clone(),
            full_interval: Duration::from_secs(config.refresh.full_interval_secs),
        }))
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    pub fn ready(&self) -> bool {
        self.snapshot.load().refreshed_at.is_some()
    }

    pub async fn refresh(&self) -> Result<bool> {
        let Ok(_guard) = self.refresh_lock.try_lock() else {
            return Ok(false);
        };
        self.maybe_call_hook().await;
        let full = self
            .last_full_refresh
            .lock()
            .await
            .is_none_or(|last| last.elapsed() >= self.full_interval);
        let previous_items = self.source_items.lock().await.clone();
        let previous_snapshot = self.snapshot();
        let baseline_ready = *self.baseline_ready.lock().await;
        let previous_roots = if !previous_items.is_empty() {
            previous_items.keys().cloned().collect()
        } else {
            source_roots(&previous_snapshot)
        };
        let items = self
            .upstream
            .discover(&previous_items, full)
            .await
            .context("discover upstream WebDAV")?;
        let next = self
            .classifier
            .build_snapshot(&items, previous_snapshot.generation.saturating_add(1));
        let changed = changed_roots(&previous_snapshot, &next);
        let current_roots = items
            .iter()
            .map(|item| item.root.source_path.clone())
            .collect::<HashSet<_>>();
        let store = self.store.clone();
        let saved = next.clone();
        tokio::task::spawn_blocking(move || store.save(&saved))
            .await
            .context("join snapshot save")??;
        self.snapshot.store(Arc::new(next));
        *self.source_items.lock().await = items
            .iter()
            .cloned()
            .map(|item| (item.root.source_path.clone(), item))
            .collect();
        if full {
            *self.last_full_refresh.lock().await = Some(Instant::now());
        }
        if baseline_ready {
            log_entry_changes(&previous_roots, &current_roots);
        }
        *self.baseline_ready.lock().await = true;
        gauge!("webdav_filter_index_resources").set(self.snapshot.load().resources.len() as f64);
        counter!("webdav_filter_refresh_total", "result" => "success").increment(1);
        if !changed.is_empty() {
            self.run_update_command(changed).await;
        }
        Ok(true)
    }

    pub async fn delete_item(&self, item_root: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        self.upstream.delete_item(item_root).await?;
        let current = self.snapshot();
        let mut next = (*current).clone();
        let removed: HashSet<String> = next
            .resources
            .iter()
            .filter(|(_, r)| r.item_root.as_deref() == Some(item_root))
            .map(|(path, _)| path.clone())
            .collect();
        next.resources.retain(|path, _| !removed.contains(path));
        for children in next.children.values_mut() {
            children.retain(|path| !removed.contains(path));
        }
        next.children.retain(|path, _| !removed.contains(path));
        next.generation = next.generation.saturating_add(1);
        next.refreshed_at = Some(chrono::Utc::now());
        let store = self.store.clone();
        let saved = next.clone();
        tokio::task::spawn_blocking(move || store.save(&saved))
            .await
            .context("join snapshot save")??;
        self.snapshot.store(Arc::new(next));
        self.source_items.lock().await.remove(item_root);
        info!(source_path = %item_root, "upstream entry disappeared");
        counter!("webdav_filter_delete_total", "result" => "success").increment(1);
        Ok(())
    }

    async fn maybe_call_hook(&self) {
        let Some(hook) = &self.hook else {
            return;
        };
        let mut last = self.hook_last_called.lock().await;
        if last.is_some_and(|value| value.elapsed() < Duration::from_secs(hook.cooldown_secs)) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        let method = match Method::from_bytes(hook.method.as_bytes()) {
            Ok(method) => method,
            Err(error) => {
                warn!(%error, "invalid refresh hook method");
                return;
            }
        };
        let mut headers = HeaderMap::new();
        for (name, value) in &hook.headers {
            match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                (Ok(name), Ok(value)) => {
                    headers.insert(name, value);
                }
                _ => {
                    warn!(header = name, "invalid refresh hook header");
                    return;
                }
            }
        }
        let result = tokio::time::timeout(
            Duration::from_secs(hook.timeout_secs),
            self.upstream
                .call_hook(hook.url.clone(), method, &headers, hook.reuse_upstream_auth),
        )
        .await;
        match result {
            Ok(Ok(())) => counter!("webdav_filter_hook_total", "result" => "success").increment(1),
            Ok(Err(error)) => {
                counter!("webdav_filter_hook_total", "result" => "error").increment(1);
                warn!(%error, "refresh hook failed");
            }
            Err(_) => {
                counter!("webdav_filter_hook_total", "result" => "timeout").increment(1);
                warn!("refresh hook timed out");
            }
        }
    }

    async fn run_update_command(&self, roots: Vec<String>) {
        let Some(parts) = &self.update_command else {
            return;
        };
        let Some((program, args)) = parts.split_first() else {
            return;
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .args(roots)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match tokio::time::timeout(Duration::from_secs(30), command.output()).await {
            Ok(Ok(output)) if output.status.success() => info!("library update command completed"),
            Ok(Ok(output)) => {
                warn!(status = %output.status, stderr = %String::from_utf8_lossy(&output.stderr), "library update command failed")
            }
            Ok(Err(error)) => error!(%error, "library update command could not start"),
            Err(_) => warn!("library update command timed out"),
        }
    }
}

fn source_roots(snapshot: &Snapshot) -> HashSet<String> {
    snapshot
        .resources
        .values()
        .filter_map(|resource| resource.item_root.clone())
        .collect()
}

fn log_entry_changes(previous: &HashSet<String>, current: &HashSet<String>) {
    let mut discovered = current.difference(previous).cloned().collect::<Vec<_>>();
    discovered.sort();
    for source_path in discovered {
        info!(source_path = %source_path, "new upstream entry discovered");
    }

    let mut disappeared = previous.difference(current).cloned().collect::<Vec<_>>();
    disappeared.sort();
    for source_path in disappeared {
        info!(source_path = %source_path, "upstream entry disappeared");
    }
}

fn changed_roots(previous: &Snapshot, next: &Snapshot) -> Vec<String> {
    let categories = |snapshot: &Snapshot| {
        snapshot
            .children
            .get("/")
            .into_iter()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>()
    };
    let all = categories(previous)
        .union(&categories(next))
        .cloned()
        .collect::<HashSet<_>>();
    all.into_iter()
        .filter(|category| {
            let prefix = format!("{category}/");
            previous
                .resources
                .iter()
                .filter(|(path, _)| *path == category || path.starts_with(&prefix))
                .any(|(path, resource)| next.resources.get(path) != Some(resource))
                || next
                    .resources
                    .iter()
                    .filter(|(path, _)| *path == category || path.starts_with(&prefix))
                    .any(|(path, resource)| previous.resources.get(path) != Some(resource))
        })
        .collect()
}
