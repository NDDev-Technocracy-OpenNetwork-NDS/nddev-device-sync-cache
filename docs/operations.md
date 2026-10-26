# Cache operations

This runbook covers one deployed cache service. Host names, ports, capacity values and admitted
client identities live in the private estate; this document names only their roles.

## No backup and no recovery

The store holds acceleration state only. It has no backup, no replica and no recovery procedure, by
design: a lost or corrupted store costs repeated downloads, never data. Do not back up the cache
volume. A store that cannot be trusted is replaced by an empty directory, and the service fills it
again from the allowlisted upstreams.

## Deploy, upgrade and roll back

1. Check out a reviewed `main` commit in a clean source directory.
2. Build the image with `NDS_SOURCE_COMMIT` set to that full commit.
3. Start it with `docker compose --project-name nds-cache --env-file RUNTIME_ENV -f compose.yaml up
   --detach --no-build`, adding `-f compose.collector.yaml` when the observability collector runs.
4. Verify that `/v1/health` reports the commit, `/v1/ready` answers `200` and a known object returns
   `x-nds-cache: hit`.

An upgrade repeats these steps with the new commit. Committed objects survive the container
replacement. Temporary files of interrupted downloads are removed at start. A rollback deploys the
previous image by its identity; the store format has no migrations, so every image of the same
store schema opens it.

The store bind mount does not create its host path. An unmounted cache volume stops the start
instead of filling the root disk. Readiness answers `503` when the store is not writable or lacks
room for one maximum object.

## Capacity

`NDS_CACHE_CAPACITY_BYTES` bounds the committed objects; a commit beyond it evicts the least
recently used objects. Lowering the capacity takes effect at the next commit. The service reports
pressure when the store reaches 95 percent of its capacity or the volume's free space falls below
one twentieth of it. Other users of the same volume count against that free space.

## Events

Every event carries the canonical NDS envelope with `module = cache`, the matched route template,
fixed error classes and no URL, client address, key or project identity.

| Event | Meaning |
| --- | --- |
| `process.started`, `process.stopped`, `process.heartbeat`, `process.failed` | Process lifecycle from the shared SDK; `process.failed` carries the startup error class. |
| `transport.listening` | The listener accepts connections. |
| `cache.store.opened` | The store opened; `rejected_count` counts discarded metadata without its object. |
| `cache.store.ready`, `cache.store.degraded` | Readiness of the store changed. |
| `cache.store.pressure` | The store or its volume is close to full. |
| `cache.store.evicted` | A commit evicted least recently used objects. |
| `cache.store.read.failed` | A stored object could not be opened for a response. |
| `cache.hit`, `cache.miss`, `cache.joined`, `cache.revalidated`, `cache.stale` | A response was served from the store, from a new download, from a download another request started, after revalidation, or from a stale copy. |
| `cache.fill.committed`, `cache.fill.failed` | A background download completed or failed, with its duration, in the trace of the request that started it; a `HEAD` on a missing object starts one too. |
| `cache.upstream.available`, `cache.upstream.degraded` | The upstream dependency state changed. |
| `cache.upstream.failed` | Revalidation failed and a stale copy was served. |
| `cache.rejected` | A request was refused: unknown route, invalid or disallowed path, unsupported method or an object missing upstream. |
| `cache.failed` | A request failed: upstream error class, fetch capacity or store error. |

Alerts belong to the observability deployment. The cache supports these rules: a degraded store, repeated
storage pressure, a degraded upstream that does not recover, a rising rate of failed fills and an
expired heartbeat.

## Troubleshooting

| Symptom | Cause and action |
| --- | --- |
| `404` with `route_unknown`, `path_invalid` or `path_not_allowed` | The client asks for a path outside the allowlist. Correct the client setting from `contracts/clients.json`, or propose an allowlist change with a real-socket test. |
| `502` with `upstream_redirect` | Upstream redirected to a host outside the allowlist. |
| `502` with `object_too_large` | The object exceeds `NDS_CACHE_MAX_OBJECT_BYTES`. |
| `503` with `fetch_capacity` | All concurrent downloads are busy for longer than the admission wait; raise `NDS_CACHE_FETCHES` or wait. |
| `504` with `upstream_timeout` | Upstream did not answer within the connect or read deadline. |
| A response ends early | The background download failed; the next request downloads the object again. |
| Start fails with `store_unowned` | The store directory holds content the cache did not create. Use an empty directory. |

## Requirement status

The observability requirements in `registry/cache-services.json` are met for the live increment as
follows. Startup, shutdown, readiness, storage pressure, eviction, upstream degradation and recovery
are emitted. Served bytes and hit ratio per protocol need a coordinated change of the shared event
contract. Registry admission events start with the admission increment, and client
authentication events start with the write endpoints of shared build caches. Acceptance through
OpenObserve, including one alert exercised through resolution, requires the deployed observability
pipeline.
