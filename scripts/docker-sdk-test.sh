#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${BRFP_DOCKER_IMAGE:-rust:1.96-bookworm}"
platform="${BRFP_DOCKER_PLATFORM:-linux/amd64}"

docker_env=(
  -e CARGO_TARGET_DIR=/work/target-linux
  -e TIMSDATA_LIB_DIR=/work/vendor/timsdata/linux64
)

if [[ -n "${BRFP_TEST_PRIVATE_DATA:-}" ]]; then
  docker_env+=(-e "BRFP_TEST_PRIVATE_DATA=${BRFP_TEST_PRIVATE_DATA}")
elif [[ -d "${repo_root}/fixtures/private" ]]; then
  docker_env+=(-e BRFP_TEST_PRIVATE_DATA=/work/fixtures/private)
fi

if [[ -n "${BRFP_TEST_BAF_DATA:-}" ]]; then
  docker_env+=(-e "BRFP_TEST_BAF_DATA=${BRFP_TEST_BAF_DATA}")
fi

if [[ -n "${BRFP_BAF2SQL_LIB:-}" ]]; then
  docker_env+=(-e "BRFP_BAF2SQL_LIB=${BRFP_BAF2SQL_LIB}")
fi

if [[ -n "${BRFP_TEST_MTBLS18_UV_CDF:-}" ]]; then
  docker_env+=(-e "BRFP_TEST_MTBLS18_UV_CDF=${BRFP_TEST_MTBLS18_UV_CDF}")
fi

if [[ -n "${LD_LIBRARY_PATH:-}" ]]; then
  docker_env+=(-e "LD_LIBRARY_PATH=${LD_LIBRARY_PATH}")
fi

docker run --rm --platform "${platform}" \
  -v "${repo_root}:/work" \
  -w /work \
  "${docker_env[@]}" \
  "${image}" \
  bash -lc '
    set -euo pipefail
    export PATH="/usr/local/cargo/bin:${PATH}"
    if ! command -v cmake >/dev/null 2>&1; then
      apt-get update
      apt-get install -y --no-install-recommends cmake
    fi
    cargo --version
    cargo test --all-targets
    if [ -n "${BRFP_TEST_BAF_DATA:-}" ] && [ -n "${BRFP_BAF2SQL_LIB:-}" ]; then
      BRFP_TEST_BAF_DATA="${BRFP_TEST_BAF_DATA}" \
      BRFP_BAF2SQL_LIB="${BRFP_BAF2SQL_LIB}" \
      BRFP_TEST_MTBLS18_UV_CDF="${BRFP_TEST_MTBLS18_UV_CDF:-}" \
      cargo test --test mzpeak_e2e -- --nocapture
    fi
  '
