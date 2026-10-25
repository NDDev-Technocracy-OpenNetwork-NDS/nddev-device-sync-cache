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

## Checks

- `just setup` once, then `just check` and `just test` for any registry or contract change.
- `just journal-check REVISION` before integration; the journal never rewrites recorded lines.
- `just structure` with a prepared std-0001 `nddev`; the validator is private, so CI does not run it.
- A schema change keeps `just test` rejecting each covered invalid case.
