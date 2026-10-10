#!/usr/bin/env bash
#
# Validate the frontmatter and sections of every box/kb note.
#
#   ./scripts/kb-lint.sh

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
kb="$root/box/kb"
sections="Rule Why Exceptions Example References"
required_sections="Rule Why References"
allowed_keys=" description tags resources status superseded_by reviewed "

if ! command -v yq >/dev/null 2>&1; then
  echo "yq is required: brew install yq" >&2
  exit 2
fi

errors=0

fail() {
  echo "$1: $2" >&2
  errors=$((errors + 1))
}

fm() {
  yq --front-matter=extract "$1" "$2"
}

headings() {
  awk -v level="$1" '
    /^```/ { fence = !fence; next }
    !fence && index($0, level " ") == 1 { print substr($0, length(level) + 2) }
  ' "$2"
}

validate() {
  local file=$1 rel=$2 key status target expected got
  local -a h2

  if [[ "$(head -n 1 "$file")" != "---" ]]; then
    fail "$rel" "missing frontmatter"
    return
  fi

  while IFS= read -r key; do
    case "$allowed_keys" in
      *" $key "*) ;;
      *) fail "$rel" "unknown frontmatter key: $key" ;;
    esac
  done < <(fm 'keys | .[]' "$file")

  [[ -n "$(fm '.description // ""' "$file")" ]] || fail "$rel" "description is empty"
  [[ "$(fm '.tags | length' "$file")" -gt 0 ]] || fail "$rel" "tags is empty"
  [[ "$(fm '.reviewed // ""' "$file")" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || fail "$rel" "reviewed must be YYYY-MM-DD"

  status=$(fm '.status // ""' "$file")
  case "$status" in
    adopted) ;;
    superseded)
      target=$(fm '.superseded_by // ""' "$file")
      if [[ -z "$target" ]]; then
        fail "$rel" "superseded note needs superseded_by"
      elif [[ ! -f "$(dirname "$file")/$target" ]]; then
        fail "$rel" "superseded_by not found: $target"
      fi
      ;;
    *) fail "$rel" "status must be adopted or superseded" ;;
  esac

  [[ "$(headings '#' "$file" | wc -l | tr -d ' ')" == "1" ]] || fail "$rel" "needs exactly one H1"

  h2=()
  while IFS= read -r got; do h2+=("$got"); done < <(headings '##' "$file")

  local i=0 ok=true
  for expected in $sections; do
    if [[ $i -lt ${#h2[@]} && "${h2[$i]}" == "$expected" ]]; then
      i=$((i + 1))
    elif [[ " $required_sections " == *" $expected "* ]]; then
      ok=false
    fi
  done
  if [[ $ok == false || $i -lt ${#h2[@]} ]]; then
    fail "$rel" "sections must be Rule, Why, [Exceptions], [Example], References in order, found: ${h2[*]:-none}"
  fi
}

while IFS= read -r file; do
  validate "$file" "box/kb/${file#"$kb"/}"
done < <(find "$kb" -mindepth 2 -name '*.md' | LC_ALL=C sort)

if [[ $errors -gt 0 ]]; then
  exit 1
fi
