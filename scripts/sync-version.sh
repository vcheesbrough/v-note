#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
VERSION=$(grep '^version = ' "${ROOT_DIR}/Cargo.toml" | head -1 | sed 's/version = "\(.*\)"/\1/')
printf '%s\n' "${VERSION}" > "${ROOT_DIR}/version.txt"
echo "${VERSION}"
