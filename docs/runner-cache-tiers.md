# Runner SDK layers and the central cache

The runner owner manages one local, versioned L1 installation per tool identity on each physical
managed self-hosted host. Slots share the verified read-only SDK layer. Mutable work directories
and per-job state stay with the slot. The central cache is L2 for dependency downloads and
compatible build results. It can also serve the original archive on an L1 miss when that exact
source already has an admitted route.

## Acquiring an SDK object

The runner's trusted manifest binds the tool and exact version or build revision, target OS and
architecture, required ABI, original publisher and object path, byte length and SHA-256. A checked
immutable L1 installation for that identity satisfies the SDK lookup before any cache readiness
probe, tunnel or HTTP request for the object. Its use can finish through a central-cache outage. Ordinary
dependency or build-cache traffic from the same job follows its existing policy.

On an L1 miss, the runner owner serializes preparation of the same identity on that host and
rechecks L1 inside that lease. When a matching route and transport are already admitted, fetch
the archive from L2. Read the completed body under a deadline and verify the independent size and
digest before extraction and publication. The cache's response header describes its own lookup.
An L2 hit or a successful transfer does not attest to the local SDK identity. Keep staging
separate from the selected layer and publish only a complete verified result.

Use the existing `/playwright/{path}` mapping for files whose original path is on the admitted
Playwright CDN. Preserve the origin path, including the
`/dbazure/download/playwright/builds/` form when present. The separate
`/chrome-for-testing/{path}` mapping is for its own upstream, not a substitute origin for a
Playwright pin. Check `contracts/clients.json` for the supported mappings. There is no APT route.
A tool outside this allowlist uses its existing approved SDK or release provider.

For an unavailable, refused or unverified L2 response, the existing consumer policy may try the
same pinned object from its approved provider. Where that policy requires the managed route,
report an acquisition failure and preserve the selected layer. A different version, origin,
architecture or digest cannot satisfy the pin. A speculative HEAD can start an upstream fill,
so it is unnecessary for deciding whether a verified local layer exists.

Admit the L2 path only on selected managed self-hosted routes. External hosted jobs use their
provider's cache and tool setup. Public download paths accept no client uploads. Sccache and
Gradle use the authenticated build-result store, with their existing reader and trusted writer
roles. Keep sccache's compiler, target, source, flags and result-format keys. SDK archives and
release or rollback artifacts do not belong in the build-result namespace.

## Measuring a pilot

Record L1 hit or miss and publication, L2 attempt and outcome, fallback, bytes received, separate
fetch, verification, extraction and setup times, and the selected host and manifest identity in
the private runner receipt. Count L2 calls made by SDK acquisition separately from unrelated job
traffic. A cold L1 may see an L2 hit, miss or joined fill. Repeat with the same host and manifest
for the warm L1, then repeat with the SDK consumer's L2 route unavailable. Require zero SDK L2
requests, zero archive download or extraction and the same usable SDK for the warm cases.
Also verify a mismatched pin leaves the selected layer intact and the external runner route
injects no private cache settings.

Observe `/v1/health` route deltas over the same process lifetime, including hit, miss, joined,
failures, bytes sent and bytes filled. The existing fill and build-store events report duration.
Compare build and deploy operation latency and errors with browser fill windows and the shared
disk's I/O queue and pressure. A route counter cannot identify a runner or prove overlap between
requests. The socket fixture checks one same-object upstream fill under a cold burst, concurrent
build access and a warm dependency hit. It does not measure native disk contention. Introduce
admission changes only if measurements identify a specific bottleneck.

Retention is at least 24 hours from observed completed use and extends for active leases.
Selected local layers and rollback pins remain protected by their SDK and release owners.
L2 is a disposable copy, so cache retention cannot stand in for release or rollback storage.
