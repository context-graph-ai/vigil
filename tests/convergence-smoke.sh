#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mode="${1:-help}"

case "${mode}" in
  change)
    shift
    cd "${repo_root}"
    exec "${repo_root}/scripts/verify" change "$@"
    ;;
  socket)
    if [[ -z "${VIGIL_OWNER_SMOKE:-}" ]]; then
      printf 'VIGIL_OWNER_SMOKE must name the owner-authorized physical smoke script.\n' >&2
      exit 2
    fi
    exec "${VIGIL_OWNER_SMOKE}"
    ;;
  fast|full|container-full|artifacts|all)
    printf '%s\n' \
      "The '${mode}' convergence mode was retired because it duplicated builds and bypassed receipts." \
      "Use 'scripts/verify change ...' for an edit cycle, the manual dev-closeout workflow" \
      "before fast-forwarding dev, or the owner-authorized release-qualification workflow for artifacts." >&2
    exit 2
    ;;
  help|-h|--help)
    printf 'usage: %s change <verifier options> | socket\n' "$0"
    printf 'Run scripts/verify --help for the checked-in verification contract.\n'
    ;;
  *)
    printf 'unknown mode: %s\n' "${mode}" >&2
    exit 2
    ;;
esac
