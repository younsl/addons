#!/usr/bin/env bash
# Build, test, sign, and publish the ward signature database as an OCI artifact.
# Usage: ci-ward-db.sh <fetch|plan|corpus|build|check|publish>
#   fetch    download the malicious-hash SHA-256 lists and the latest YARA Forge
#            core release (neither needs an account or key) with their provenance,
#            and copy the repository allowlist
#   plan     compare the inputs with latest, skip the rest when they match
#   corpus   export known-good images for both architectures
#   build    run `ward feed build`, pruning what matches the allowlist and corpus
#   check    run `ward feed check` against the corpus (EICAR self-test included)
#   publish  sign with cosign and push with oras, then tag latest
# Env: WORK_DIR, WARD_BIN, REPOSITORY (ghcr.io/<owner>/ward-db), GITHUB_OUTPUT,
#   GITHUB_RUN_NUMBER, GITHUB_STEP_SUMMARY, FORCE (true rebuilds identical inputs),
#   MAX_PRUNED, COSIGN_PRIVATE_KEY, COSIGN_PASSWORD, REGISTRY_USER, REGISTRY_PASSWORD (publish).
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
HASH_REPO="https://github.com/romainmarcoux/malicious-hash"
CURL=(curl -fsSL --retry 5 --retry-all-errors)

source_json() {
  jq -n --arg name "$1" --arg license "$2" --arg url "$3" --arg version "$4" --arg sha256 "$5" \
    '{name: $name, license: $license, url: $url, version: $version, sha256: $sha256}'
}

fetch() {
  local clone="${WORK_DIR}/malicious-hash" commit tag zip="${SOURCES}/yara-forge-core.zip"
  mkdir -p "${SOURCES}/hashes" "${SOURCES}/yara"

  git clone --quiet --depth 1 "${HASH_REPO}" "${clone}"
  commit=$(git -C "${clone}" rev-parse HEAD)
  cp "${clone}"/full-hash-sha256-*.txt "${SOURCES}/hashes/"
  rm -rf "${clone}"
  source_json malicious-hash MIT "${HASH_REPO}" "${commit}" "" > "${SOURCES}/hashes/source.json"

  tag=$("${CURL[@]}" -o /dev/null -w '%{url_effective}' "${YARA_FORGE_REPO}/releases/latest")
  tag="${tag##*/}"
  "${CURL[@]}" -o "${zip}" "${YARA_FORGE_REPO}/releases/download/${tag}/yara-forge-rules-core.zip"
  unzip -q -o -j "${zip}" '*.yar' -d "${SOURCES}/yara"
  source_json "YARA Forge core" "Per rule, see the license meta of each rule" "${YARA_FORGE_REPO}" \
    "${tag}" "$(sha256sum "${zip}" | cut -d' ' -f1)" > "${SOURCES}/yara/source.json"
  rm -f "${zip}"

  cp "${ALLOWLIST}" "${SOURCES}/allowlist.txt"

  echo "malicious-hash ${commit}: $(cat "${SOURCES}"/hashes/*.txt | grep -cE '^[0-9a-fA-F]{64}$') hashes"
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
    --hash-dir "${SOURCES}/hashes" \
    --yara-dir "${SOURCES}/yara" \
    --yara-linux-only \
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
