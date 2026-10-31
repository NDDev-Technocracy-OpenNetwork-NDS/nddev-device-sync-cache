# Cache service architecture

The cache service is one Rust process that serves owner devices and private runners. It proxies a
fixed allowlist of upstream routes, keeps their objects on the cache volume and reports its state
through the NDS observability pipeline. It is not an open proxy: a request outside a declared route
is rejected before any upstream connection.

## Access

The service listens on a loopback address of its host. Devices and runners reach it with
an estate-admitted remote-access forward, such as restricted SSH or `rds forward`. That transport
authenticates its peer and restricts the remote target; the private estate owns the selection.
Redundant forwards reach the same central listener and store. Write operations that later protocol
increments add, such as shared build caches, additionally require a per-client key that only CI
identities hold. Host, port and allowlist values live in the private estate.

## Responsibilities

| Module | Responsibility |
| --- | --- |
| `domain` | Route allowlist, upstream mapping, request path validation and object freshness. Pure Rust without I/O. |
| `cache` | The serve use case: store lookup, single-flight upstream fetch, revalidation, bounded fetch admission and stale fallback. It composes the store and upstream adapters and performs no I/O itself. |
| `store` | Objects and metadata on the cache volume, atomic commits through temporary files and renames, size accounting, least-recently-used eviction and readiness. |
| `upstream` | HTTPS client with rustls, connect and read deadlines, conditional requests and redirects limited to allowlisted hosts. |
| `http` | axum routes for the downloads, `/v1/health`, `/v1/ready` and `/source`; matched route templates for telemetry. |
| `telemetry` | Build identity for the shared `nddev-device-sync-telemetry` SDK, which owns the envelope, filtering and output. |
| `config` | Bounded settings from the process environment. |

Each adapter has one implementation, so the use case calls it directly instead of through a port
trait. Tests exercise the real filesystem store and a real local upstream over sockets.

## Objects and consistency

An object is immutable when its upstream path identifies one release artifact, and mutable when it
is a channel or index document that upstream rewrites.

A missing immutable object is downloaded by a background task that the first request starts. The
task writes a temporary file and publishes how many bytes it has flushed; every client of that
object, the first and any that join later, streams the growing file at once. A client that
disconnects does not stop the download, and `HEAD` answers from the upstream headers without waiting
for the body. A `HEAD` on a missing object therefore also starts its complete download, which later
requests join or find stored. The download runs in its own operation span inside the trace of the
request that started it, so its completion or failure is reported in that trace with its duration. The download is committed only after it completes within its size bound, so a partial
transfer never becomes a cached object; a failed download ends every following response early and
leaves nothing behind. Concurrent requests for one missing object share one upstream fetch.

A pending GET does not advertise upstream `Content-Length`: receiving that many flushed bytes
must not complete HTTP before the writer verifies the digest and commits. Its streaming framing
ends only on `Committed`; `Failed` aborts it, including a zero-byte body. A stored GET and a HEAD
retain their known lengths. This follows the completion rules in
[RFC9112 sections6.3 and8](https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3).

Mutable objects are small. They are downloaded completely before they are served, carry their
upstream validators and are revalidated after a bounded age; when revalidation fails, the stored copy
is served and the stale response is reported.

Package registries name their downloads by absolute upstream URL inside index documents: PyPI
project pages link release files and pub package documents link archives. uv, pip and pub fetch
those URLs as written, so the cache serves such a document with its download links pointed at the
client's base address, the loopback address of its forward. Only a link prefix that opens a JSON
string is rewritten, while the document streams; hashes, integrity values and the rest of the
document stay as upstream sent them, and the store keeps the upstream bytes. A request whose host
is not loopback is refused, so a rewritten link never leads elsewhere. The entity tag of a served
document names both the stored object and the base, so a client at another address revalidates.

An index document has several representations: npm serves an abbreviated install document and
the full document, and the simple repository API a JSON and an HTML form. The route lists the
representations it serves in preference order. The cache picks the one the client's `Accept` ranks
highest, requests that one upstream and stores each representation as its own object; a client
that accepts none of them gets `406`. The PyPI route serves only the JSON form, which uv and pip
prefer.

npm package documents keep the public tarball links upstream sent. npm replaces the public
registry host in a link with its configured registry, so it downloads through the cache while
`package-lock.json` keeps the public links. Bun follows a link as written, but records every
tarball of the public registry in `bun.lock` with an empty link, and builds an empty link from its
configured registry, the package name and the version. A client whose `User-Agent` opens with
`Bun/` therefore gets the install document with exactly those links empty: the `tarball` of each
version's `dist` that starts with `https://registry.npmjs.org/`. Bun then downloads through the
cache on an online install as on a frozen one, and creates the `bun.lock` a public install
creates. Bun loads an empty link as its configured registry, though, so a change to an existing
`bun.lock` through the route records the cache address for the entries it already held; such
changes are authored against the canonical registry.
The links are emptied while the document streams, as links are rewritten, so one stored object
serves both forms; the response varies by `Accept` and `User-Agent`, and the entity tag names the
emptied form. A link to another host, an escaped link and every other value stay as upstream sent
them. The device client leaves Bun unbound, because Bun does not read the npm global
configuration file that binds npm; a project or job selects the route for Bun.

OCI pulls name the registry after the cache's address, for example
`127.0.0.1:8740/docker.io/library/alpine:3`. The cache answers the `/v2/` check itself, asks the
registry for every image manifest type, takes an anonymous token from the registry's allowlisted
token service when it is challenged, and follows blob redirects only to allowlisted storage hosts.
Manifests by tag are revalidated; manifests by digest and blobs are content-addressed, and the
cache stores them only when their bytes have the digest of their path. Responses carry
`Docker-Content-Digest`.

Public GitHub release assets use `/github-releases/sha256/{digest}/owner/repository/releases/download/tag/asset`.
The digest prefix is retained in the store key and stripped only from the upstream URL. These
objects use the same streamed download and digest verification as content-addressed OCI objects,
without OCI authentication or response headers. A replaced release asset cannot poison the old
digest's cache entry. The downloader still verifies the completed file before using it.

The store has a configured capacity. Commits that would exceed it evict the least recently used
objects first. Storage pressure, eviction and failed commits are reported, never hidden. The cache
holds acceleration state only: losing it costs downloads, not data.

## Device client

The module's device side is the `nddev-device-sync-cache-client` adapter in `crates/client`, built
on the ADR 0014 template: a `NetworkCache` port with `status`, `bind` and `unbind`, the adapter
behind it on adapter-io's local authority, and the `nds-cache` command. A binding projects one
base onto one scope: the home of a user, or a CI lane whose jobs run with `HOME` at the lane prefix
and their workspaces below it. Configuration files that tools read for a user get managed blocks,
because they rank below project configuration; only tools without such a file get variables, from
one environment file. Tools whose lockfiles record the resolving index, uv and pub, are not bound.
A lane's Cargo configuration sits at the prefix, found by directory discovery, so the jobs'
`CARGO_HOME` store is never written. The projection refuses a scope root that is not the user's
own real directory, a lane whose jobs' environment would not read its files, a link in place of a
file, foreign edits of the files it owns, a user file that already names a default registry, a
file above its 1 MiB read bound or one the binding would grow past it, and a lower-ranked Cargo
home configuration that names a source. It checks every file before it writes any, writes each
atomically and restores the written ones on a failure; a failure it cannot undo, or a read-back
that fails after the writes, answers `uncertain`. A bound scope fails closed;
there is no automatic switch back to direct downloads. The remote module owns the persistent forward and the
device identity, so the client holds no credential.

## Current limits

- Registry admission is a later increment: the crates, npm, PyPI and pub routes proxy every
  published package, and the clients verify each download against the checksum or integrity value of its
  index document.

- Clients that refuse plain HTTP distribution URLs, such as some Gradle wrapper settings and Gradle
  Maven repositories, need the forwarded endpoint behind a local TLS terminator or a later TLS
  listener.
- A response without an upstream `Content-Length` is sent chunked; its end marks a committed
  download.

## Observability

Every request, lifecycle transition and dependency state change emits the canonical NDS event
envelope through the shared SDK with `module = cache`. Events use matched route templates, status,
duration, outcome and fixed error classes; they never carry URLs, client keys or project
identities. The cache-specific signals in `registry/cache-services.json` that the shared event
contract cannot yet express, such as served bytes and hit ratio per protocol, are added through a
coordinated change of that contract in `nddev-device-sync-observability`.

## Deployment

`compose.yaml` runs the image with a read-only root filesystem, no capabilities, a non-root user
and the store directory as the only writable bind mount. The host port binds to loopback. The
`compose.collector.yaml` overlay switches the container log driver to the Fluent collector of the
observability deployment, which is the canonical event transport. Host values, capacity and the
store directory live in the private estate.

## Increments

1. Toolchain downloads: rustup distribution, Flutter storage, python-build-standalone releases,
   Playwright browsers, Gradle and Go distributions, Chrome for Testing archives.
2. crates.io sparse index and crate files. The cache generates the index `config.json` for the
   loopback base address of each client, so downloads return through the cache.
3. npm registry for Bun and npm, and the PyPI simple index for uv and pip. Their index documents
   are served per negotiated representation; PyPI file links point at the client base.
4. pub hosted repository for Dart and Flutter, through the same index documents.
5. Maven Central, Google Maven and the Gradle Plugin Portal; Gradle reaches a plain-HTTP repository
   only through its explicit insecure-protocol opt-in.
6. Go module proxy and toolchain modules, and the Go checksum database through the proxy: the go
   command reaches the database only here and verifies its signed notes with the database's key.
7. OCI pull-through for Docker Hub, GitHub, Google and Chainguard registries; BuildKit cache
   references belong to the shared build caches.
8. Shared build caches with CI-only writes.
9. Registry admission and the Rust registry generator.

Each increment is accepted with its real protocol client against the deployed service.
