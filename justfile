set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

validator_version := "0.60.1"
rust_toolchain := "1.99.1"
tools := justfile_directory() / ".tools"
validator := tools / "bin" / "jsonschema-cli"

# List the available operations without preparing tools.
default:
    @just --list

# Install the pinned Rust JSON Schema validator into .tools with the pinned Rust toolchain.
setup:
    cargo +{{ rust_toolchain }} install --locked --version {{ validator_version }} jsonschema-cli --root "{{ tools }}"

# Run every local check: registry, formatting, tests, lints and dependency policy.
check: registry-check registry-test fmt-check test clippy dependencies-check

# Validate the schemas, the registry files, the client contract and decisions-to-state consistency.
registry-check:
    #!/usr/bin/env bash
    set -euo pipefail
    validate() { "{{ validator }}" validate -d 2020 --assert-format --offline --errors-only "$@"; }
    work=$(mktemp -d)
    trap 'rm -rf -- "$work"' EXIT
    "{{ validator }}" validate --offline --errors-only -i contracts/*.schema.json
    validate contracts/stack.schema.json -i registry/stack.json
    validate contracts/decisions.schema.json -i registry/decisions.json
    validate contracts/clients.schema.json -i contracts/clients.json
    jq -s . registry/journal.jsonl > "$work/journal.json"
    validate contracts/journal.schema.json -i "$work/journal.json"
    jq -e -n --slurpfile d registry/decisions.json --slurpfile s registry/stack.json '
      [$d[0].toolchains | to_entries[] | . as $e
        | ($s[0].toolchains | map(select(.id == $e.key))) as $m
        | select(($m | length) != 1 or $m[0].stack != $e.value.stack or $m[0].status != $e.value.status)
        | $e.key] as $toolchains
      | [$d[0].package_holds | to_entries[] | .key as $eco | .value | to_entries[] | . as $h
        | ($s[0].packages[$eco][$h.key]) as $p
        | select($p != null and ([$p.exceptions[]?.version] | index($h.value.stack)) == null)
        | "\($eco):\($h.key)"] as $holds
      | if ($toolchains + $holds | length) == 0 then true
        else error("stack.json disagrees with decisions.json: \($toolchains + $holds)") end' > /dev/null
    echo "registry valid"

# Confirm that the schemas reject representative invalid registry content.
registry-test:
    #!/usr/bin/env bash
    set -euo pipefail
    work=$(mktemp -d)
    trap 'rm -rf -- "$work"' EXIT
    jq -s . registry/journal.jsonl > "$work/journal-source.json"
    reject() {
        local schema=$1 name=$2 source=$3 filter=$4
        jq "$filter" "$source" > "$work/$name.json"
        if "{{ validator }}" validate -d 2020 --assert-format --offline --errors-only "$schema" -i "$work/$name.json" > /dev/null 2>&1; then
            echo "accepted invalid fixture: $name" >&2
            return 1
        fi
        echo "rejected: $name"
    }
    reject contracts/stack.schema.json held-without-removal registry/stack.json '(.toolchains[] | select(.status == "held")) |= del(.removal_condition)'
    reject contracts/stack.schema.json compat-line-without-lines registry/stack.json '(.toolchains[] | select(.status == "latest+compat")) |= (.stack = .stack[0])'
    reject contracts/stack.schema.json exception-without-upstream registry/stack.json '(.packages[][] | select(.status == "latest+exception") | .exceptions[]) |= del(.upstream)'
    reject contracts/stack.schema.json unknown-status registry/stack.json '(.packages.npm[] | select(.status == "latest")) |= (.status = "pinned")'
    reject contracts/stack.schema.json latest-with-reason registry/stack.json '(.packages.npm[] | select(.status == "latest")) |= (.reason = "kept")'
    reject contracts/stack.schema.json action-without-commit registry/stack.json '.github_actions[] |= (.sha = .latest)'
    reject contracts/decisions.schema.json hold-without-removal registry/decisions.json '.package_holds.npm[] |= del(.removal_condition)'
    reject contracts/decisions.schema.json held-toolchain-without-reason registry/decisions.json '(.toolchains[] | select(.status == "held")) |= del(.reason)'
    reject contracts/journal.schema.json event-without-timestamp "$work/journal-source.json" '.[-1] |= del(.ts)'
    reject contracts/journal.schema.json journal-without-creation "$work/journal-source.json" '.[1:]'
    reject contracts/clients.schema.json route-without-clients contracts/clients.json '.routes[0].clients = []'
    reject contracts/clients.schema.json setting-outside-base contracts/clients.json '.routes[0].clients[0].setting |= map_values("https://example.com")'
    reject contracts/clients.schema.json plain-http-upstream contracts/clients.json '.routes[0].upstream = "http://static.rust-lang.org"'

# Verify that registry/journal.jsonl only appends to its content at the given Git revision.
journal-check revision="origin/main":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git cat-file -e "$1:registry/journal.jsonl" 2> /dev/null; then
        echo "no journal at $1; nothing to compare"
        exit 0
    fi
    base=$(mktemp)
    trap 'rm -f -- "$base"' EXIT
    git show "$1:registry/journal.jsonl" > "$base"
    if ! cmp -s -n "$(wc -c < "$base")" "$base" registry/journal.jsonl; then
        echo "registry/journal.jsonl rewrites history recorded at $1" >&2
        exit 1
    fi
    echo "journal appends to $1"

# Require rustfmt formatting of the service.
fmt-check:
    cargo fmt --all -- --check

# Run the unit tests and the real-socket service tests against isolated local upstreams.
test:
    cargo test --locked --workspace --all-targets --jobs 4
    cargo test --locked --workspace --doc --jobs 4

# Deny every Clippy warning across all targets.
clippy:
    cargo clippy --locked --workspace --all-targets --jobs 4 -- -D warnings

# Check RustSec advisories, licenses and dependency sources of the locked graph.
dependencies-check:
    cargo audit --deny warnings
    cargo deny --locked check licenses sources

# Build the service image and exercise the hardened container: readiness, identity and events.
container-check commit=`git rev-parse HEAD`:
    #!/usr/bin/env bash
    set -euo pipefail
    name="nds-cache-check-$$"
    work=$(mktemp -d)
    trap 'docker rm --force "$name" > /dev/null 2>&1 || true; docker image rm "$name" > /dev/null 2>&1 || true; rm -rf -- "$work"' EXIT
    docker build --build-arg "NDS_SOURCE_COMMIT=$1" --tag "$name" .
    docker run --detach --name "$name" --read-only --cap-drop ALL \
        --security-opt no-new-privileges:true --memory 128m --pids-limit 64 \
        --tmpfs /var/lib/nds-cache:uid=10001,gid=10001,mode=0700,size=64m \
        --env NDS_CACHE_CAPACITY_BYTES=1073741824 --env NDS_CACHE_MAX_OBJECT_BYTES=1048576 \
        --publish 127.0.0.1::8740 "$name" > /dev/null
    for _ in {1..50}; do
        docker exec "$name" nddev-device-sync-cache healthcheck && break
        sleep 0.2
    done
    docker exec "$name" nddev-device-sync-cache healthcheck
    base="http://$(docker port "$name" 8740/tcp | head -n 1)"
    curl --fail --silent --max-time 2 "$base/v1/health" > "$work/health.json"
    jq -e --arg commit "$1" '.status == "ok" and .source_commit == $commit and .store_objects == 0' \
        "$work/health.json" > /dev/null
    test "$(curl --silent --output /dev/null --write-out '%{http_code}' "$base/unknown/object")" = 404
    docker stop --time 10 "$name" > /dev/null
    docker logs "$name" > "$work/events.jsonl" 2>&1
    jq -e -s --arg commit "$1" '
      (map(select(."event.name" == "transport.listening")) | length) == 1
      and (map(select(."event.name" == "cache.rejected" and .route == "unmatched")) | length) == 1
      and (map(select(."event.name" == "process.stopped")) | length) == 1
      and all(."source.commit" == $commit and ."service.name" == "nddev-device-sync-cache")' \
        "$work/events.jsonl" > /dev/null
    echo "container ready, identified and observable"

# Run the std-0001 structural check with a prepared nddev validator.
structure:
    "${NDDEV_STRUCTURE_VALIDATOR:-nddev}" check --root .
