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

When Docker Hub refuses the digest-pinned base images with `429`, pull them through the cache by
the same digest and tag them locally, for example `docker pull
127.0.0.1:8740/docker.io/library/rust@sha256:<digest>` followed by `docker tag` to the
Dockerfile's name. The digest pin verifies the content, and the daemon keeps its configuration.

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

Every cache-route event carries the canonical NDS envelope with `module = cache`, the matched route template,
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
| `cache.hit`, `cache.miss`, `cache.joined`, `cache.revalidated`, `cache.stale` | A response was served from the store, from a new download, from a download another request started, after revalidation, or from a stale copy; `status` is `304` when the client already held the object's entity tag. |
| `cache.fill.committed`, `cache.fill.failed` | A background download completed or failed, with its duration, in the trace of the request that started it; a `HEAD` on a missing object starts one too. An OCI blob or manifest whose bytes lack the digest its path names fails with `digest_mismatch` and is not stored. |
| `cache.upstream.available`, `cache.upstream.degraded` | The upstream dependency state changed. |
| `cache.upstream.failed` | Revalidation failed and a stale copy was served. |
| `cache.index.config` | The cache answered the crates.io sparse index `config.json` for a loopback client base. |
| `cache.rejected` | A request was refused: unknown route, invalid or disallowed path, unsupported method, an object missing upstream, an index document in no representation the client accepts, or an index configuration or index document request from a host that is not loopback. |
| `cache.failed` | A request failed: upstream error class, fetch capacity or store error. |

The egress relay (`nds-cache-egress`) uses the same SDK and canonical envelope with
`module = cache`. `egress.listening`, `egress.listen.failed` and `egress.failed`
describe listener/service state; bind errors distinguish address-in-use, unavailable address
and permission denial. `egress.tunnel.opened`, `egress.tunnel.refused` and
`egress.tunnel.finished` describe tunnel admission and its terminal outcome; refusals carry
only a fixed `error.type`. Shutdown reports cancelled admitted tunnels after closing admission
and joins their tasks before the SDK reports process stop. Destinations, addresses, request
heads and tunnel bytes never enter these events.

The relay owns at most its configured number of tunnel tasks plus one serial capacity
response. Protocol response writes share the request-head timeout bound. An established
tunnel ends on peer close or relay shutdown; it has no total-duration promise.
Native DNS jobs share a separate bound equal to the tunnel limit and return at most 64
addresses. The resolve/connect decision has its configured deadline; a timed-out native
lookup retains capacity until its actual result is reclaimed. Shutdown also waits for these
jobs rather than declaring completion from a cancelled async awaiter. There is no hard
wall-clock promise for the OS resolver.

Alerts belong to the observability deployment. The cache supports these rules: a degraded store, repeated
storage pressure, a degraded upstream that does not recover, a rising rate of failed fills and an
expired heartbeat.

## Troubleshooting

| Symptom | Cause and action |
| --- | --- |
| `404` with `route_unknown`, `path_invalid` or `path_not_allowed` | The client asks for a path outside the allowlist. Correct the client setting from `contracts/clients.json`, or propose an allowlist change with a real-socket test. |
| `406` with `not_acceptable` | The client accepts no representation the route serves, such as a PyPI client that takes only HTML pages. Use a client from `contracts/clients.json`. |
| `400` with `host_not_loopback` | An index configuration or index document was requested through a host name that is not loopback. Point the client at the forwarded loopback address. |
| `502` with `upstream_redirect` | Upstream redirected to a host outside the allowlist. |
| `502` with `upstream_auth` | A registry's anonymous token service failed or answered with something other than a token. |
| A pull fails with `digest_mismatch` | The registry served bytes that do not have the digest of their path; nothing was stored. Retry; a repeated mismatch is an upstream fault. |
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
