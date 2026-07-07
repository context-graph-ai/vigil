#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mode="${1:-all}"
smoke_dir="${VIGIL_CONVERGENCE_DIR:-${repo_root}/target/convergence-smoke}"
mkdir -p "${smoke_dir}"

log() {
  printf '\n== %s ==\n' "$*"
}

run_fast_gate() {
  log "fast gate"
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo nextest run --profile pr --workspace
  cargo build --workspace
}

setup_harness() {
  log "harness setup"
  cargo xtask setup-harness
  sha256sum \
    target/vigil-test-tools/mediamtx/v1.19.1/mediamtx \
    target/vigil-test-tools/zig/zig-cc-x86_64-linux-musl \
    target/vigil-test-tools/zig/zig-cxx-x86_64-linux-musl \
    target/vigil-test-tools/zig/rust-lld-x86_64-linux-musl \
    target/vigil-test-tools/zig/zig-cc-aarch64-linux-musl \
    target/vigil-test-tools/zig/zig-cxx-aarch64-linux-musl \
    target/vigil-test-tools/zig/rust-lld-aarch64-linux-musl \
    | tee "${smoke_dir}/tool-sha256.txt"
}

run_full_tests() {
  setup_harness
  log "full test list"
  cargo nextest list --release --profile ci-full --workspace \
    --features first-light-acceptance,acceptance \
    | tee "${smoke_dir}/ci-full-tests.txt"
  log "full slow test lane"
  cargo nextest run --release --profile ci-full --workspace \
    --features first-light-acceptance,acceptance
}

run_container_full_tests() {
  log "container full slow test lane"
  docker compose -f docker-compose.ci.yml build slow-tests
  docker compose -f docker-compose.ci.yml run --rm slow-tests
}

build_artifacts() {
  setup_harness
  log "static musl builds"
  rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  cargo build --release --target x86_64-unknown-linux-musl
  cargo build --release --target aarch64-unknown-linux-musl
  file \
    target/x86_64-unknown-linux-musl/release/vigil \
    target/aarch64-unknown-linux-musl/release/vigil \
    | tee "${smoke_dir}/artifact-file.txt"
  sha256sum \
    target/x86_64-unknown-linux-musl/release/vigil \
    target/aarch64-unknown-linux-musl/release/vigil \
    | tee "${smoke_dir}/artifact-sha256.txt"
  log "multi-arch docker build"
  docker buildx build --platform linux/amd64,linux/arm64 .
}

run_owner_smoke_if_configured() {
  if [[ -z "${VIGIL_OWNER_SMOKE:-}" ]]; then
    log "owner live-camera smoke skipped"
    printf 'Set VIGIL_OWNER_SMOKE to the operator smoke script path to run the live-camera leg.\n'
    return 0
  fi
  log "owner live-camera smoke"
  "${VIGIL_OWNER_SMOKE}"
}

case "${mode}" in
  fast)
    cd "${repo_root}"
    run_fast_gate
    ;;
  full)
    cd "${repo_root}"
    run_full_tests
    ;;
  container-full)
    cd "${repo_root}"
    run_container_full_tests
    ;;
  artifacts)
    cd "${repo_root}"
    build_artifacts
    ;;
  socket)
    cd "${repo_root}"
    run_owner_smoke_if_configured
    ;;
  all)
    cd "${repo_root}"
    run_fast_gate
    run_full_tests
    build_artifacts
    run_owner_smoke_if_configured
    ;;
  *)
    printf 'usage: %s [fast|full|container-full|artifacts|socket|all]\n' "$0" >&2
    exit 2
    ;;
esac

log "convergence smoke passed (${mode})"
