set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

validator_version := "0.60.1"
tools := justfile_directory() / ".tools"
validator := tools / "bin" / "jsonschema-cli"

# List the available operations without preparing tools.
default:
    @just --list

# Install the pinned Rust JSON Schema validator into .tools.
setup:
    cargo install --locked --version {{ validator_version }} jsonschema-cli --root "{{ tools }}"

# Validate the schemas, the registry files and decisions-to-state consistency.
check:
    #!/usr/bin/env bash
    set -euo pipefail
    validate() { "{{ validator }}" validate -d 2020 --assert-format --offline --errors-only "$@"; }
    work=$(mktemp -d)
    trap 'rm -rf -- "$work"' EXIT
    "{{ validator }}" validate --offline --errors-only -i contracts/*.schema.json
    validate contracts/stack.schema.json -i registry/stack.json
    validate contracts/decisions.schema.json -i registry/decisions.json
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
test:
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

# Run the std-0001 structural check with a prepared nddev validator.
structure:
    "${NDDEV_STRUCTURE_VALIDATOR:-nddev}" check --root .
