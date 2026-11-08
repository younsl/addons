#!/usr/bin/env bash
# Build, test, sign, and publish the ward signature database as an OCI artifact.
# Usage: ci-ward-db.sh <fetch|plan|corpus|build|check|publish>
#   fetch    download the MalwareBazaar SHA-256 dump and the latest YARA Forge core
#            release with its provenance, and copy the repository allowlist
#   plan     compare the inputs with latest, skip the rest when they match
#   corpus   export known-good images for both architectures
#   build    run `ward feed build`, pruning what matches the allowlist and corpus
#   check    run `ward feed check` against the corpus (EICAR self-test included)
#   publish  sign with cosign and push with oras, then tag latest
# Env: WORK_DIR, WARD_BIN, REPOSITORY (ghcr.io/<owner>/ward-db), GITHUB_OUTPUT,
#   GITHUB_RUN_NUMBER, GITHUB_STEP_SUMMARY, FORCE (true rebuilds identical inputs),
#   MAX_PRUNED, ABUSE_CH_AUTH_KEY (optional), COSIGN_PRIVATE_KEY, COSIGN_PASSWORD, REGISTRY_USER,
#   REGISTRY_PASSWORD (publish).
set -euo pipefail

WORK_DIR="${WORK_DIR:?}"
SOURCES="${WORK_DIR}/sources"
CORPUS="${WORK_DIR}/corpus"
DB="${WORK_DIR}/ward-db.json.gz"
SIG="${DB}.sig"
STATE="${WORK_DIR}/state.env"
ALLOWLIST="box/addons/ward/feed/allowlist.txt"
MAX_PRUNED="${MAX_PRUNED:-25}"
BUILD_LOG="${WORK_DIR}/build.log"
CORPUS_PLATFORMS=(linux/amd64 linux/arm64)
CORPUS_IMAGES=(
  debian:stable-slim
  ubuntu:latest
  alpine:latest
  busybox:latest
  gcr.io/distroless/static-debian12:latest
  gcr.io/distroless/base-debian12:latest
  python:3-slim
  node:lts-slim
  golang:alpine
  eclipse-temurin:21-jre
  nginx:stable
)
INPUTS_ANNOTATION="sh.ward.db.inputs-digest"
YARA_FORGE_REPO="https://github.com/YARAHQ/yara-forge"

fetch() {
  mkdir -p "${SOURCES}/yara"
  local auth=()
  if [[ -n "${ABUSE_CH_AUTH_KEY:-}" ]]; then
    auth=(-H "Auth-Key: ${ABUSE_CH_AUTH_KEY}")
  fi
  curl -fsSL --retry 5 --retry-all-errors "${auth[@]}" -o "${SOURCES}/malwarebazaar.zip" \
    https://bazaar.abuse.ch/export/txt/sha256/full/
  unzip -p "${SOURCES}/malwarebazaar.zip" > "${SOURCES}/malwarebazaar.txt"
  rm -f "${SOURCES}/malwarebazaar.zip"

  local tag zip="${SOURCES}/yara-forge-core.zip"
  tag=$(curl -fsSL --retry 5 --retry-all-errorsI -o /dev/null -w '%{url_effective}' "${YARA_FORGE_REPO}/releases/latest")
  tag="${tag##*/}"
  curl -fsSL --retry 5 --retry-all-errors -o "${zip}" "${YARA_FORGE_REPO}/releases/download/${tag}/yara-forge-rules-core.zip"
  unzip -q -o -j "${zip}" '*.yar' -d "${SOURCES}/yara"
  jq -n --arg version "${tag}" --arg sha256 "$(sha256sum "${zip}" | cut -d' ' -f1)" \
    --arg url "${YARA_FORGE_REPO}" \
    '{name: "YARA Forge core", license: "Per rule, see the license meta of each rule", url: $url, version: $version, sha256: $sha256}' \
    > "${SOURCES}/yara/source.json"
  rm -f "${zip}"

  cp "${ALLOWLIST}" "${SOURCES}/allowlist.txt"

  echo "hashes: $(grep -cvE '^(#|$)' "${SOURCES}/malwarebazaar.txt")"
  echo "yara forge ${tag}: $(find "${SOURCES}/yara" -name '*.yar' | wc -l) files"
}

corpus() {
  local image platform dir
  for image in "${CORPUS_IMAGES[@]}"; do
    for platform in "${CORPUS_PLATFORMS[@]}"; do
      dir="${CORPUS}/${image//[:\/]/_}_${platform//\//_}"
      mkdir -p "${dir}"
      crane export --platform "${platform}" "${image}" - | tar -x -C "${dir}" --no-same-owner --exclude 'dev/*'
      echo "${image} ${platform}: $(find "${dir}" -type f | wc -l) files"
    done
  done
  du -sh "${CORPUS}"
}

inputs_digest() {
  (cd "${SOURCES}" && find . -type f -print0 | sort -z | xargs -0 sha256sum) | sha256sum | cut -d' ' -f1
}

plan() {
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
}

build() {
  local REVISION dir args=()
  REVISION=$(sed -n 's/^REVISION=//p' "${STATE}")
  for dir in "${CORPUS}"/*/; do
    args+=(--corpus "${dir}")
  done
  "${WARD_BIN}" feed build \
    --out "${DB}" \
    --revision "${REVISION}" \
    --malwarebazaar "${SOURCES}/malwarebazaar.txt" \
    --yara-dir "${SOURCES}/yara" \
    --allowlist "${SOURCES}/allowlist.txt" \
    --max-pruned "${MAX_PRUNED}" \
    "${args[@]}" 2>&1 | tee "${BUILD_LOG}"
  summary
  ls -lh "${DB}"
}

summary() {
  local pruned
  [[ -n "${GITHUB_STEP_SUMMARY:-}" ]] || return 0
  pruned=$(sed -n 's/^pruned //p' "${BUILD_LOG}" | sed "s#${CORPUS}/##")
  {
    echo "## ward-db ${REVISION}"
    echo
    grep -E '^.* revision .*: ' "${BUILD_LOG}" | tail -1
    echo
    if [[ -z "${pruned}" ]]; then
      echo "Nothing pruned against the corpus."
    else
      echo "Pruned against the corpus ($(wc -l <<<"${pruned}" | tr -d ' ') of at most ${MAX_PRUNED}):"
      echo
      echo '```'
      echo "${pruned}"
      echo '```'
    fi
  } >> "${GITHUB_STEP_SUMMARY}"
}

check() {
  local dir args=()
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
  fetch | plan | corpus | build | check | publish) "$1" ;;
  *)
    echo "usage: $0 <fetch|plan|corpus|build|check|publish>" >&2
    exit 2
    ;;
esac
