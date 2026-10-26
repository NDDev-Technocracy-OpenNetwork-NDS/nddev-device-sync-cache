# NDDev OpenNetwork · cache

AGPL-3.0-only NDS module. [NDDev OpenNetwork](https://nddev.ai).

`nddev-device-sync-cache` is the central network cache for NDDev devices and private self-hosted
GitHub Actions runners. It proxies package registries, toolchain downloads and container images
over the internal network. Its registry admission increment will restrict them to the technology
versions recorded in this repository's registry; until then the live routes proxy their whole
upstream allowlist. Public repositories keep free GitHub-hosted runners and do not use it.

## Current state

The repository contains the technology registry and the cache service with its first increment,
toolchain downloads. The system catalog lists the module with the `future` status until the service
is deployed and accepted with real clients. The
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
| `/playwright/` | Playwright browser builds |
| `/gradle/` | Gradle distributions and their checksums |
| `/go/` | Go toolchain archives and their checksums |
| `/chrome-for-testing/` | Chrome for Testing archives of exact versions |
| `/crates-index/` | crates.io sparse index files; the cache generates `config.json` |
| `/crates/` | crate archives of exact published versions |
| `/npm/` | npm package documents and tarballs, for Bun and npm |
| `/pypi/` | PyPI simple index project pages in their JSON form |
| `/pypi-files/` | PyPI release files and their core metadata |
| `/pub/` | pub.dev package documents and archives, for Dart and Flutter |
| `/maven-central/` | Maven Central release files |
| `/google-maven/` | Google's Maven repository for Android |
| `/gradle-plugins/` | Gradle Plugin Portal markers and plugin artifacts |
| `/goproxy/` | Go module proxy versions, including Go toolchains |
| `/v2/<registry>/` | OCI image pulls from `docker.io`, `ghcr.io`, `gcr.io` and `cgr.dev` |

Release artifacts stream to every client while the cache downloads them once, and stay immutable
after the complete download is committed. Channel and index documents are revalidated with their
upstream validators and served stale when upstream fails. npm package documents, PyPI project
pages and pub package documents are stored per representation the client accepts and served
with their download links pointed at the loopback base address the client used, so the
downloads come back through the cache.
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

Devices and runners reach the loopback listener through `rds forward` of
[remote-device-sync](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-rds).
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
`http://127.0.0.1:PORT`, the cache listener on the cache host or the local port of the RDS forward
elsewhere; `--lane DIR` binds a CI lane instead. It writes managed blocks into the user
configuration of Cargo, npm and Bun, pip and the go command, one environment file for rustup,
Flutter, Playwright and uv's Python downloads, and the Gradle init script. User files rank below a
project's own configuration, so project pins, lockfiles and private registries stay in force; a
user file that names a default registry of its own is refused, never overwritten. uv and pub stay
unbound: their lockfiles record the index they resolved against, so a bound default index would
fail `uv sync --locked` and `dart pub get --enforce-lockfile` on committed lockfiles.
Binding needs a ready cache and changes nothing on any refusal; a bound scope fails closed when
the cache does not answer, and `unbind` restores the earlier files. The client holds no
credential: RDS owns the forward and the device identity. Its runtime manifest is
[`contracts/module.json`](contracts/module.json).

### CI lanes

A lane is one runner prefix whose jobs run with `HOME` at the prefix and their workspaces below it.
`nds-cache bind --port PORT --lane PREFIX` runs as the lane's user with the environment its job
steps see, so that the runner's own selections stay as they are:

- **Cargo:** the lane gets `PREFIX/.cargo/config.toml`, which Cargo finds by directory discovery
  for every workspace below the prefix. The jobs' `CARGO_HOME` is a store that lanes may share and
  maintenance may evict, so it is never written; binding refuses when its `config.toml` names a
  source, which the lane's file would shadow.
- **npm, Bun, pip, go:** their user files below `HOME`, which therefore has to be the prefix, with
  `XDG_CONFIG_HOME` and `GOENV` unset or at their defaults there.
- **Untouched:** `CARGO_HOME`, `RUSTUP_HOME`, `GOMODCACHE`, `GOCACHE`, `GRADLE_USER_HOME` and the
  package caches.
- **Not bound in a lane:** the environment-file tools and Gradle, unless a workflow opts in. The
  same goes for Cargo run outside the prefix, and for container jobs.

All of a lane's files live in its prefix, so binding and unbinding one lane changes no other lane
and no personal profile. Bind and unbind only while the lane runs no job. A binding persists
across jobs until an explicit unbind.

Bound means central-only. While the forward or the cache is down, a download that the job's local
caches cannot serve fails visibly. Nothing falls back to the public source on its own. That
differs from an optional acceleration layer: locked and offline builds keep working from the
lane's local caches, and the way back to direct downloads is `unbind`.

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
