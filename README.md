# webdav-filter

`webdav-filter` is a fast, provider-neutral WebDAV sorting proxy. It reads an
upstream WebDAV library, applies Zurg-compatible filters, and exposes the
matching items as virtual folders. Media is streamed directly from upstream,
including HTTP range requests; complete files are never buffered or stored.

## Quick start

Copy the example configuration, edit the upstream URL and credentials, then
validate it:

```sh
cp config.example.yml config.yml
cargo run -- --config config.yml --check-config
```

Run it directly:

```sh
cargo run -- --config config.yml
```

Or run the Docker Compose deployment:

```sh
docker compose up -d --build
```

The default listener is `127.0.0.1:9999`. Mount it with rclone:

```ini
[filtered]
type = webdav
url = http://127.0.0.1:9999/
vendor = other
```

```sh
rclone mount filtered: /mnt/filtered \
  --allow-other \
  --dir-cache-time 1m \
  --vfs-cache-mode minimal
```

## Configuration

Credentials must come from an environment variable or a mounted secret file;
literal passwords in YAML are rejected. HTTP upstreams require the explicit
`allow_http: true` opt-in.

Directory filters support regex, contains, descendant-file filters, nested
`and`/`or`, size checks, episode detection, group priority, and largest-file
selection. Directory names and grouping are entirely configuration-defined.

Refreshes run every five seconds by default. Unchanged top-level items reuse
their metadata, while `full_interval_secs` forces periodic validation. The
optional refresh hook can ask an upstream provider to update its own index
before scanning. `POST /-/refresh` requests an immediate coalesced refresh.

## Behavior and safety

- Output Basic authentication is optional; anonymous access should be limited
  to a trusted network or protected reverse proxy.
- If deletion is enabled, deleting an item or any descendant deletes the entire
  corresponding top-level upstream item and all of its virtual aliases.
- Upload, rename, move, copy, and lock methods are rejected.
- A failed refresh keeps the last complete snapshot available. SQLite persists
  that snapshot across restarts.
- `/-/healthz` is public for container probes. Readiness, metrics, and manual
  refresh use Basic authentication when configured.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
docker build .
```
