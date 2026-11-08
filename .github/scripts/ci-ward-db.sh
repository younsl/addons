#!/usr/bin/env bash
# Build, test, sign, and publish the ward signature database as an OCI artifact.
# Usage: ci-ward-db.sh <fetch|corpus|build|check|publish>
#   fetch    download the MalwareBazaar SHA-256 dump and YARA Forge core rules
#   corpus   export base images into a false-positive corpus
#   build    run `ward feed build`, skip the rest when inputs match latest
#   check    run `ward feed check` against the corpus (EICAR self-test included)
#   publish  sign with cosign and push with oras, then tag latest
# Env: WORK_DIR, WARD_BIN, REPOSITORY (ghcr.io/<owner>/ward-db), GITHUB_OUTPUT,
#   GITHUB_RUN_NUMBER, FORCE (true rebuilds identical inputs), ABUSE_CH_AUTH_KEY
#   (optional), COSIGN_PRIVATE_KEY, COSIGN_PASSWORD, REGISTRY_USER,
#   REGISTRY_PASSWORD (publish).
set -euo pipefail

WORK_DIR="${WORK_DIR:?}"
SOURCES="${WORK_DIR}/sources"
CORPUS="${WORK_DIR}/corpus"
DB="${WORK_DIR}/ward-db.json.gz"
SIG="${DB}.sig"
STATE="${WORK_DIR}/state.env"
CORPUS_IMAGES=(debian:stable-slim alpine:latest ubuntu:latest)
INPUTS_ANNOTATION="sh.ward.db.inputs-digest"

fetch() {
  mkdir -p "${SOURCES}/yara"
  local auth=()
  if [[ -n "${ABUSE_CH_AUTH_KEY:-}" ]]; then
    auth=(-H "Auth-Key: ${ABUSE_CH_AUTH_KEY}")
  fi
  curl -fsSL "${auth[@]}" -o "${SOURCES}/malwarebazaar.zip" \
    https://bazaar.abuse.ch/export/txt/sha256/full/
  unzip -p "${SOURCES}/malwarebazaar.zip" > "${SOURCES}/malwarebazaar.txt"
  rm -f "${SOURCES}/malwarebazaar.zip"

  curl -fsSL -o "${SOURCES}/yara-forge-core.zip" \
    https://github.com/YARAHQ/yara-forge/releases/latest/download/yara-forge-rules-core.zip
  unzip -q -o -j "${SOURCES}/yara-forge-core.zip" '*.yar' -d "${SOURCES}/yara"
  rm -f "${SOURCES}/yara-forge-core.zip"

  echo "hashes: $(grep -cvE '^(#|$)' "${SOURCES}/malwarebazaar.txt")"
  echo "yara files: $(find "${SOURCES}/yara" -name '*.yar' | wc -l)"
}

corpus() {
  for image in "${CORPUS_IMAGES[@]}"; do
    local dir="${CORPUS}/${image//[:\/]/_}"
    mkdir -p "${dir}"
    crane export --platform linux/amd64 "${image}" - | tar -x -C "${dir}" --no-same-owner --exclude 'dev/*'
    echo "${image}: $(find "${dir}" -type f | wc -l) files"
  done
}

inputs_digest() {
  (cd "${SOURCES}" && find . -type f -print0 | sort -z | xargs -0 sha256sum) | sha256sum | cut -d' ' -f1
}

build() {
  local digest previous revision changed
  digest=$(inputs_digest)
  previous=$(crane manifest "${REPOSITORY}:latest" 2>/dev/null \
    | jq -r --arg k "${INPUTS_ANNOTATION}" '.annotations[$k] // empty' || true)

  changed="true"
  if [[ "${digest}" == "${previous}" && "${FORCE:-false}" != "true" ]]; then
    echo "inputs unchanged since latest (${digest}), skip"
    changed="false"
  fi

  revision="$(date -u +%Y%m%d).${GITHUB_RUN_NUMBER:-0}"
  {
    echo "REVISION=${revision}"
    echo "INPUTS_DIGEST=${digest}"
  } > "${STATE}"
  echo "changed=${changed}" >> "${GITHUB_OUTPUT}"
  echo "revision=${revision}" >> "${GITHUB_OUTPUT}"
  [[ "${changed}" == "true" ]] || return 0

  "${WARD_BIN}" feed build \
    --out "${DB}" \
    --revision "${revision}" \
    --malwarebazaar "${SOURCES}/malwarebazaar.txt" \
    --yara-dir "${SOURCES}/yara"
  ls -lh "${DB}"
}

check() {
  local args=()
  for dir in "${CORPUS}"/*/; do
    args+=(--corpus "${dir}")
  done
  "${WARD_BIN}" feed check --db "${DB}" "${args[@]}"
}

publish() {
  local REVISION INPUTS_DIGEST
  REVISION=$(sed -n 's/^REVISION=//p' "${STATE}")
  INPUTS_DIGEST=$(sed -n 's/^INPUTS_DIGEST=//p' "${STATE}")
  cosign sign-blob --yes --key env://COSIGN_PRIVATE_KEY --tlog-upload=false \
    --output-signature "${SIG}" "${DB}"

  echo "${REGISTRY_PASSWORD}" | oras login "${REPOSITORY%%/*}" -u "${REGISTRY_USER}" --password-stdin
  (cd "$(dirname "${DB}")" && oras push "${REPOSITORY}:${REVISION}" \
    --artifact-type application/vnd.ward.db.v1 \
    --annotation "${INPUTS_ANNOTATION}=${INPUTS_DIGEST}" \
    --annotation "org.opencontainers.image.version=${REVISION}" \
    --annotation "org.opencontainers.image.source=https://github.com/younsl/addons" \
    "$(basename "${DB}"):application/vnd.ward.db.v1+gzip" \
    "$(basename "${SIG}"):application/vnd.ward.db.signature.v1")
  oras tag "${REPOSITORY}:${REVISION}" latest
  echo "published ${REPOSITORY}:${REVISION} and latest"
}

case "${1:-}" in
  fetch | corpus | build | check | publish) "$1" ;;
  *)
    echo "usage: $0 <fetch|corpus|build|check|publish>" >&2
    exit 2
    ;;
esac
