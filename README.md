# NDDev OpenNetwork · cache

AGPL-3.0-only NDS module. [NDDev OpenNetwork](https://nddev.ai).

`nddev-device-sync-cache` is the central network cache for NDDev devices and private self-hosted
GitHub Actions runners. It proxies package registries, toolchain downloads and container images
over the internal network, and it admits only the technology versions recorded in this
repository's registry. Public repositories keep free GitHub-hosted runners and do not use it.

## Current state

The repository contains the technology registry and the cache design input. The cache service is
not implemented yet, and the system catalog lists the module with the `future` status. The
[system task](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync/issues/75)
tracks the remaining work.

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
| `registry/cache-services.json` | Cache design input: owner constraints, protocols, reusable Rust components, verified constraints, security and observability requirements. |

### Contracts and commands

[`contracts/`](contracts/) holds the JSON Schemas (draft 2020-12) of the registry state, the
decisions input and the journal; the cache service and future registry consumers read the registry
through them. The commands need `just`, `jq`, Git and a Rust toolchain for the pinned validator:

```sh
just setup                      # install the pinned Rust JSON Schema validator into .tools
just check                      # validate schemas, registry files and decisions-to-state consistency
just test                       # confirm the schemas reject representative invalid content
just journal-check origin/main  # confirm the journal only appends to that revision
just structure                  # std-0001 structural check with a prepared nddev validator
```

Pull requests and `main` pushes run `setup`, `check`, `test` and the journal check on GitHub-hosted
runners. The std-0001 validator is private, so the structural check runs where it is prepared.

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

## Privacy boundary

This public repository contains no private project names, device inventory, endpoints or
credentials. Exception-to-project bindings, the cache host and volume values and the survey
evidence belong to the private
[estate](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-estate).

## License and releases

Legal terms are in [LICENSE](LICENSE). The release journal is
[GitHub Releases](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-cache/releases).
