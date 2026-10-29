# NDDev OpenNetwork · cache

AGPL-3.0-only NDS module. [NDDev OpenNetwork](https://nddev.ai).

`nddev-device-sync-cache` is the central network cache for NDDev devices and private self-hosted
GitHub Actions runners. It proxies package registries, toolchain downloads and container images
over the internal network. Its registry admission increment will restrict them to the technology
versions recorded in this repository's registry; until then the live routes proxy their whole
upstream allowlist. Public repositories keep free GitHub-hosted runners and do not use it.

## Current state

The repository contains the technology registry, cache service, device binding client, forward
pool and egress relay. Route and client acceptance are recorded separately in the client contract;
the private estate owns deployments and their native evidence. The
[system task](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync/issues/75)
tracks the remaining work.

## Cache service

`nddev-device-sync-cache` is one Rust process. [docs/architecture.md](docs/architecture.md)
describes its modules, consistency rules, limits and increments, and
[docs/operations.md](docs/operations.md) is the operator runbook with its no-backup policy, events
and troubleshooting. It serves these routes and rejects
every other path before any upstream connection:

| Route prefix | Upstream |
| --- | --- |
| `/rustup/` | rustup distribution server |
| `/flutter/` | Flutter release storage and the Flutter Maven repository |
| `/python-build-standalone/` | python-build-standalone release assets, the CPython source for uv |
| `/github-releases/sha256/<digest>/` | Public GitHub release assets with a caller-pinned SHA-256 |
| `/playwright/` | Playwright browser builds |
| `/gradle/` | Gradle distributions and their checksums |
| `/go/` | Go toolchain archives and their checksums |
| `/chrome-for-testing/` | Chrome for Testing archives of exact versions |
| `/crates-index/` | crates.io sparse index files; the cache generates `config.json` |
| `/crates/` | crate archives of exact published versions |
| `/npm/` | npm package documents and tarballs, for npm |
| `/pypi/` | PyPI simple index project pages in their JSON form |
| `/pypi-files/` | PyPI release files and their core metadata |
| `/pub/` | pub.dev package documents and archives, for Dart and Flutter |
| `/maven-central/` | Maven Central release files |
| `/google-maven/` | Google's Maven repository for Android |
| `/gradle-plugins/` | Gradle Plugin Portal markers and plugin artifacts |
| `/goproxy/` | Go module proxy versions, including Go toolchains |
| `/goproxy/sumdb/sum.golang.org/` | The Go checksum database, which the go command reaches through the proxy |
| `/v2/<registry>/` | OCI image pulls from `docker.io`, `ghcr.io`, `gcr.io` and `cgr.dev` |

Release artifacts stream to every client while the cache downloads them once, and stay immutable
after the complete download is committed. Channel and index documents are revalidated with their
upstream validators and served stale when upstream fails. A pending GET keeps its streaming
framing open until verification and commit, even after every upstream byte arrived; only stored
GETs and HEADs advertise a known length. npm package documents, PyPI project
pages and pub package documents are stored per representation the client accepts. PyPI and pub
documents are served with their download links pointed at the loopback base address the client
used, so the downloads come back through the cache. npm documents keep their public links, which
npm fetches through its configured registry, so `package-lock.json` stays public.
`GET /v1/health` reports the build identity and store usage. `GET /v1/ready` answers `503` when
the store is not writable or lacks room for one maximum object, and reports the outcome of the
latest upstream exchange without failing on it, because stored objects stay servable while
upstream is unreachable. `GET /source` links the corresponding source.

| Variable | Meaning |
| --- | --- |
| `NDS_CACHE_ROOT` | Required absolute store directory; it must be empty or already owned by the cache. |
| `NDS_CACHE_LISTEN` | Listener address; the binary defaults to loopback. |
| `NDS_CACHE_CAPACITY_BYTES` | Store capacity; least recently used objects are evicted beyond it. |
| `NDS_CACHE_MAX_OBJECT_BYTES` | Largest admitted object. |
| `NDS_CACHE_FETCHES` | Concurrent upstream downloads. |
| `NDS_CACHE_REVALIDATE_SECONDS` | Age after which channel and index documents are revalidated. |

The service emits the canonical NDS event envelope on standard output through the shared telemetry
SDK. `compose.yaml` runs the hardened container with the store as its only writable mount, and the
`compose.collector.yaml` overlay delivers the events to the observability Fluent collector:

```sh
docker compose -f compose.yaml -f compose.collector.yaml up --detach
```

Devices and runners reach the loopback listener through estate-admitted forwards, such as
restricted SSH or `rds forward` of
[remote-device-sync](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-rds).
Several forwards can reach the same central listener through `nds-forward-pool`; their health
checks select a transport, and never select a different store or a public download fallback.
[`contracts/clients.json`](contracts/clients.json) is the client contract: every live route, the
setting that sends each tool through it relative to the forwarded base address, its known limits and
whether a real client or only the route has been accepted. A test keeps it equal to the service
route table. Clients keep their own digest and signature checks. Gradle builds pass
[`contracts/gradle/nds-cache.init.gradle.kts`](contracts/gradle/nds-cache.init.gradle.kts) with
`--init-script`; it points only the public Maven repositories at the forwarded base.

## Device client

[`crates/client`](crates/client) is the device side of the module, the
`nddev-device-sync-cache-client` adapter that NDS composes with its `NetworkCache` port and the
`nds-cache` command. `nds-cache bind --port PORT` binds the current user's tools to the cache base
`http://127.0.0.1:PORT`, the cache listener on the cache host or an admitted local forward
elsewhere; `--lane DIR` binds a CI lane instead. It writes managed blocks into the user
configuration of Cargo, pip and the go command; one environment file for rustup, Flutter,
Playwright, uv's Python downloads and npm; npm's global configuration file that this environment
file names; and the Gradle init script. User files rank below a project's own configuration, so
project pins, lockfiles and private registries stay in force; a user file that names a default
registry of its own is refused, never overwritten. So is a Cargo directory that holds the
extensionless `config`, which Cargo reads instead of `config.toml`, and a Cargo file that uses
`include`: the client never follows an included path. uv, pub and Bun stay unbound: their lockfiles
record the index they resolved against, so a bound default index would fail `uv sync --locked`
and `dart pub get --enforce-lockfile` on committed lockfiles, and `bun add` would write the cache
address into `bun.lock`. Bun reads npm's user file and variables but not npm's global
configuration file, so npm is bound there; the user's `.npmrc` must not name a default registry.
Binds and unbinds of one scope run one at a time, under an exclusive lock on the scope's home
directory that leaves no file behind; one that waits more than 10 s answers `busy`. Each file is
re-read right before its write and written only while it still holds what the preflight read, so
an edit made meanwhile answers `conflict`. This check is advisory, among binders that take the
lock, not an atomic compare-and-swap against every editor. On a failure the files already written
are restored, each only while it still holds what the client wrote; a file edited since stays as
it is, and then, as when a restore fails or the scope cannot be read back after its writes, the
answer is `uncertain` and the status has to be read back. A scope file is read only while it is a
regular file of at most 1 MiB; a larger one, or one the binding would grow past that, answers
`invalid`. The readiness probe has 3 s in total: the connect, every write and every read get only
what is left of that deadline, and one whose socket timeout cannot be set answers not ready. Files are written through a private temporary file of their own; a new file stays private,
and an existing one keeps its permissions. Binding needs a ready cache and changes nothing on any refusal; a bound scope fails closed when
the cache does not answer, and `unbind` restores the earlier files. `status` reports a file as
bound only while binding again would change nothing and refuse nothing: the file holds exactly
the projection of the given port, and the same tool's files that rank next to it leave it in
force, so an edited block, a binding of another port or a default registry in the user's `.npmrc`
reads as unbound. The client holds no
credential: the admitted transport owns its authentication and remote target. Its runtime manifest is
[`contracts/module.json`](contracts/module.json).

### CI lanes

A lane is one runner prefix whose jobs run with `HOME` at the prefix and their workspaces below it.
`nds-cache bind --port PORT --lane PREFIX` runs as the lane's user with the environment its job
steps see, so that the runner's own selections stay as they are:

- **Cargo:** the lane gets `PREFIX/.cargo/config.toml`, which Cargo finds by directory discovery
  for every workspace below the prefix. The jobs' `CARGO_HOME` is a store that lanes may share and
  maintenance may evict, so it is never written; binding refuses when its `config.toml` names a
  source, which the lane's file would shadow, includes another file, or stands beside an
  extensionless `config`.
- **pip, go:** their user files below `HOME`, which therefore has to be the prefix, with
  `XDG_CONFIG_HOME` and `GOENV` unset or at their defaults there.
- **Untouched:** `CARGO_HOME`, `RUSTUP_HOME`, `GOMODCACHE`, `GOCACHE`, `GRADLE_USER_HOME` and the
  package caches.
- **Environment-file tools and Gradle:** a runner adapter or workflow must carry the generated
  variables and npm global configuration into its actual job environment, and pass the Gradle
  init script. Writing those files alone does not make a job use them. Cargo run outside the
  prefix and container jobs require their own qualified integration.

All of a lane's files live in its prefix, so binding and unbinding one lane changes no other lane
and no personal profile. Bind and unbind only while the lane runs no job. A binding persists
across jobs until an explicit unbind.

Bound means central-only. While the forward or the cache is down, a download that the job's local
caches cannot serve fails visibly. Nothing falls back to the public source on its own. That
differs from an optional acceleration layer: locked and offline builds keep working from the
lane's local caches, and the way back to direct downloads is `unbind`.

### Public GitHub release assets

A downloader with an independently pinned digest can replace only the origin of an exact
GitHub release asset URL:

```text
https://github.com/OWNER/REPOSITORY/releases/download/TAG/ASSET
http://127.0.0.1:PORT/github-releases/sha256/SHA256/OWNER/REPOSITORY/releases/download/TAG/ASSET
```

The lowercase SHA-256 remains in the store key, while the cache requests the original GitHub
download path. Redirects use the HTTPS host allowlist. The cache commits only a complete object
with that digest; a mismatch fails the streaming response and leaves no reusable object. The
downloader must verify its own completed temporary file before publishing or executing it.
An escaped slash within a tag is supported. API requests, latest-release discovery, private
assets and source archives are outside this route; caller headers and credentials are not
forwarded. A consumer must explicitly select this URL and keep it on retries: a failed cache
request never authorizes a public fallback. The device binding does not rewrite arbitrary URLs.

GitHub documents [release asset links](https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases)
and [asset digests](https://docs.github.com/en/rest/releases/assets); a tag or download URL alone
does not establish the caller's expected bytes.

## Forward pool

A device reaches the cache, and a controller reaches a device, through forwards whose local
listener keeps accepting connections after the tunnel behind it stops carrying data.
`nds-forward-pool` is one loopback listener in front of several such forwards to the same
service:

```sh
nds-forward-pool --listen 127.0.0.1:8740 \
  --primary 127.0.0.1:18741 --primary 127.0.0.1:18742 --fallback 127.0.0.1:18740 \
  --http-probe /v1/ready
```

- **End-to-end probes.** Every 2 s (`--interval SECONDS`) each upstream is probed through the whole path:
  `--http-probe PATH` expects `HTTP/1.x 200`, `--banner-probe PREFIX` expects the server's first
  bytes, such as `SSH-`. A frozen tunnel fails the probe within 3 s although its listener accepts.
- **Spread, then fall back.** New connections go round robin over the healthy primaries, so flows
  that a link throttles one by one add up, and to the healthy fallbacks in order only while no
  primary is healthy. With nothing healthy every upstream is still tried in order.
- **No moves.** A connection stays on its upstream once it carries bytes; a client that loses it
  retries like after any connection failure.
- **Loopback only**, for the listener and every upstream; events carry outcomes, never addresses.

## Egress relay

Some device uplinks throttle direct downloads that the cache does not carry: a tool archive from
a release page, a source checkout, an API call. `nds-cache-egress --listen 127.0.0.1:PORT` runs
on the cache host as a loopback-only HTTPS relay that devices reach through the same kind of
owned forward as the cache, and that jobs select with `HTTPS_PROXY=http://127.0.0.1:PORT` and
`NO_PROXY=127.0.0.1,localhost`.

- **CONNECT only.** It answers `CONNECT host:port` and splices bytes, so TLS stays end to end
  between the job and the origin; nothing is inspected, rewritten or stored. The one local
  `GET /v1/ready` answers an empty `200` and closes; other methods/paths answer `405`.
  Plain-HTTP requests are not relayed.
- **Readiness through the forward.** A pool can select this relay with `--http-probe /v1/ready`.
  The probe reaches the actual relay without DNS or an external connection, shares its request
  size/deadline/task-capacity bounds and answers `503` when admission is full. It proves this
  transport reaches a serving relay; external DNS, origin reachability and TLS remain separate
  checks. A local listening tunnel alone does not establish readiness.
- **Public destinations only.** The destination must resolve to a public address on an allowed
  port, `443` unless `--allow-port` selects others. Loopback, private, link-local, shared,
  documentation, benchmark, multicast and reserved ranges answer `403`. IPv6 is native global
  unicast only: mapped/compatible, standard NAT64, protocol-assignment and 6to4 destinations are
  refused. This address policy does not authenticate the origin or replace the host's routing
  and firewall policy.
- **Bounds.** A request head is at most 8 KiB (`431`) and must arrive within 10 s (`408`); resolving
  and connecting take at most 15 s (`502`); at most 512 tunnels are open (`--max-connections`,
  then `503`). One serial refusal response is allowed beyond the tunnel bound; protocol writes
  also use the head timeout. Shutdown closes admission and joins every tunnel task before
  returning; established tunnels have no total-duration deadline. Native DNS jobs have the
  same capacity bound and retain their slot through actual completion, including after an
  async timeout. At most 64 resolved addresses are admitted. Shutdown waits for those jobs;
  it cannot promise a wall-clock bound on the OS resolver.
- **Events.** The existing shared SDK owns the canonical lifecycle, readiness and tunnel events, with fixed
  outcomes/error classes and no host name, address, request head or payload.

The relay is not a cache: what a job can fetch through a cache route belongs on that route, so it
accumulates on the cache host's disk and keeps the central-only guarantee of the binding.

## Technology registry

[`registry/stack.json`](registry/stack.json) records, for every technology used by core
repositories, the newest stable upstream version and the versions the cache admits:

- toolchains, SDKs and build tools;
- direct npm, PyPI, crates.io, pub and Go module dependencies;
- GitHub Actions with the commit identity of their latest release;
- container base images.

Read one entry with `jq`:

```sh
jq '.toolchains[] | select(.id == "python")' registry/stack.json
jq '.packages.npm.eslint' registry/stack.json
```

The cache admits the latest stable version by default. A lower version enters only with a recorded
reason, an upstream issue or change, and a removal condition:

| Status | Meaning |
| --- | --- |
| `latest` | Only the newest stable upstream version is admitted. |
| `latest+compat` | The newest version plus an owner-selected compatibility line. |
| `latest+lts` | The newest feature release plus the newest long-term-support release. |
| `latest+exception` | The newest version for the stack, plus an older version admitted for a counted set of projects. |
| `held` | The admitted version is below upstream until the removal condition is met. |
| `replace` | Projects move off this package. |
| `isolated-tool` | Installed as an isolated tool, never as a project dependency. |

`workspace_rules` records combinations that are valid separately but not inside one workspace.
[`registry/journal.jsonl`](registry/journal.jsonl) is the append-only event history: new
observations append lines and existing lines are never rewritten.

| File | Role |
| --- | --- |
| `registry/decisions.json` | The only hand-edited registry input: lines, holds, exceptions, replacements and their references. |
| `registry/stack.json` | Registry state derived from the decisions and official-source evidence. |
| `registry/journal.jsonl` | Append-only events with UTC timestamps. |
| `contracts/clients.json` | Client contract of the live routes, validated by `contracts/clients.schema.json`. |
| `registry/cache-services.json` | Cache design input for later increments: owner constraints, protocols, reusable Rust components, verified constraints, security and observability requirements. |

### Contracts

[`contracts/`](contracts/) holds the JSON Schemas (draft 2020-12) of the registry state, the
decisions input and the journal; the cache service and future registry consumers read the registry
through them.

### How the registry is checked

Versions come from official machine-readable release sources: language package registries,
toolchain release indexes, GitHub Releases, Maven and Google Maven metadata, the Android
repository index and container registries. Compatibility is checked per ecosystem:

1. npm: peer dependencies and engine ranges of every latest package against the latest version of
   each peer and the stack Node.js line, using npm's own semver implementation.
2. PyPI: pairwise requirements, `requires_python` and wheel tags, then a whole-set uv resolution
   of core dependencies at their latest versions on each admitted CPython line.
3. Rust: one probe manifest with every crate pinned to its latest version and the stack
   `rust-version`, resolved with `cargo generate-lockfile`; crate MSRVs from the registry index.
4. pub: environment and dependency constraints against the stack Dart and Flutter versions.
5. Android and JVM: the Kotlin, Android Gradle Plugin, Gradle and Flutter compatibility tables and
   Flutter tooling source at the stack tag.
6. Container images: manifest existence for every admitted tag.

The registry generator is part of the planned Rust implementation; the current snapshot was
produced by one-off read-only probes.

## Cache design

The owner constraints are fixed: the service is NDS Rust code or reuses open-source components
written in Rust, clients reach it only over the internal network with per-client keys, and its
consumers are owner devices and private runners.
[`registry/cache-services.json`](registry/cache-services.json) lists the protocols the service
serves, the client settings that redirect each tool, the Rust components considered for reuse and
the verified upstream constraints. Shared build caches accept writes only from CI identities;
devices and pull-request jobs read only.

The service is observable through the NDS pipeline: it emits the canonical event envelope through
the shared telemetry SDK of
[nddev-device-sync-observability](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-observability),
exposes liveness, readiness and dependency health, and is accepted only after a real event reaches
OpenObserve and an alert completes its lifecycle. The `observability` section of
`registry/cache-services.json` lists its lifecycle events, signals, metric labels and alerts.

## Commands

The commands need `just`, `jq`, Git, the pinned Rust toolchain, `cargo-audit` and `cargo-deny`;
the container check also needs Docker and `curl`:

```sh
just setup                      # install the pinned Rust JSON Schema validator into .tools
just check                      # run every local check below except the container and structure checks
just registry-check             # validate schemas, registry files and decisions-to-state consistency
just registry-test              # confirm the schemas reject representative invalid content
just test                       # unit tests and real-socket service tests against local upstreams
just clippy                     # deny every Clippy warning
just dependencies-check         # RustSec advisories, licenses and dependency sources
just journal-check origin/main  # confirm the journal only appends to that revision
just container-check            # build and exercise the hardened service container
just structure                  # std-0001 structural check with a prepared nddev validator
```

Pull requests run the registry checks, the journal check and the STD-0004 native Rust contract on
GitHub-hosted runners. `main` pushes also run the advisory audit and the container check. The
std-0001 validator is private, so the structural check runs where it is prepared.

## Privacy boundary

This public repository contains no private project names, device inventory, endpoints or
credentials. Exception-to-project bindings, the cache host and volume values and the survey
evidence belong to the private
[estate](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-estate).

## License and releases

Legal terms are in [LICENSE](LICENSE). The release journal is
[GitHub Releases](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-cache/releases).
