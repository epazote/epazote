test: build-bin clippy fmt
  cargo test

build-bin:
  cargo build --bins

clippy:
  cargo clippy --all-targets --all-features

fmt:
  cargo fmt --all -- --check

coverage:
  cargo llvm-cov --all-features --workspace

# Release the tested develop commit; see .github/RELEASING.md.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    git fetch --quiet origin develop
    sha="$(git rev-parse origin/develop)"
    last="$(gh run list --workflow release.yml --branch develop --event workflow_dispatch --limit 1 --json databaseId --jq '.[0].databaseId // 0')"
    gh workflow run release.yml --ref develop
    run=""
    for _ in $(seq 30); do
        run="$(gh run list --workflow release.yml --branch develop --event workflow_dispatch --limit 100 --json databaseId,headSha --jq "[.[] | select(.databaseId > ${last} and .headSha == \"${sha}\")] | sort_by(.databaseId) | .[0].databaseId // empty")"
        [[ -z "${run}" ]] || break
        sleep 2
    done
    [[ -n "${run}" ]] || { echo "The dispatched release run did not appear." >&2; exit 1; }
    gh run watch "${run}" --exit-status
    ci=""
    for _ in $(seq 30); do
        ci="$(gh run list --workflow build.yml --branch develop --event push --commit "${sha}" --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
        [[ -z "${ci}" ]] || break
        sleep 2
    done
    [[ -n "${ci}" ]] || { echo "No develop push CI run for ${sha}; wait for CI OK before moving main." >&2; exit 1; }
    # Wait without --exit-status: coverage is deliberately not a release gate.
    gh run watch "${ci}" > /dev/null
    gate="$(gh run view "${ci}" --json status,jobs --jq 'if .status == "completed" then [.jobs[] | select(.name == "CI OK")] | if length == 1 then .[0].conclusion else "missing" end else "pending" end')"
    [[ "${gate}" == success ]] || { echo "CI OK is ${gate} on ${sha}; re-run failed jobs before moving main." >&2; exit 1; }
    git push origin "${sha}:refs/heads/main"
