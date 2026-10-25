# nddev-device-sync-cache working contract

Public AGPL-3.0-only NDDev OpenNetwork module. Read [README.md](README.md) first. The NDS
product standards selected by [standarts.lock](standarts.lock) live in the
[system root](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync) `standarts/`;
[module.yaml](module.yaml) is the NDS module descriptor and [.nds/nddev.yaml](.nds/nddev.yaml)
selects the NDDev structural contract.
The parent task is
[nddev-device-sync#75](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync/issues/75).

## Constraints

- Never commit private project names, device inventory, endpoints, credentials or survey evidence.
  They belong in the private estate.
- Implementation code is Rust; reused services are open source and written in Rust. No paid or
  proprietary product enters the cache.
- `registry/decisions.json` is the only hand-edited registry input. `registry/stack.json` derives
  from it and official-source evidence. `registry/journal.jsonl` is append-only.
- A version below the latest stable upstream needs a reason, an upstream reference and a removal
  condition. Version values stay out of Markdown.
- Shared build caches accept writes only from CI identities.

## Service rules

- The service proxies only the routes declared in `src/domain.rs`; a new upstream or redirect host
  is a reviewed allowlist change with a real-socket test.
- Events go through the shared telemetry SDK with matched route templates and fixed error classes;
  they never carry URLs, client addresses, keys or project identities.
- Tests use the real filesystem store and real local upstreams over sockets, never mocks of them.
- The container keeps a read-only root, no capabilities, a non-root user and loopback-bound ports.

## Checks

- `just setup` once, then `just check` before integration; it runs the registry checks, formatting,
  tests, Clippy and the dependency policy.
- `just journal-check REVISION` before integration; the journal never rewrites recorded lines.
- `just container-check` for any change to the image, the Compose files or process lifecycle.
- `just structure` with a prepared std-0001 `nddev`; the validator is private, so CI does not run it.
- A schema change keeps `just registry-test` rejecting each covered invalid case.
