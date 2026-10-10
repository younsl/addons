#!/bin/zsh

set -euo pipefail

TETHER_IMAGE="ghcr.io/younsl/tether:0.1.0"
TETHER_NAME="tether"
TETHER_PORT=8080
ADDONS_DIR="$HOME/github/younsl/addons"
TETHER_CONFIG_FILE="$ADDONS_DIR/box/addons/tether/config.toml"

REPLY=""

container_runtime() {
    if command -v docker &>/dev/null; then
        echo docker
    elif command -v podman &>/dev/null; then
        echo podman
    else
        return 1
    fi
}

print_plan() {
    echo "tether will reconcile dotfiles symlinks into $HOME:"
    echo "  image: $TETHER_IMAGE"
    echo "  config: $TETHER_CONFIG_FILE"
    echo "  api:   http://127.0.0.1:$TETHER_PORT/status"
}

start_tether() {
    local runtime="$1"

    if [[ ! -f "$TETHER_CONFIG_FILE" ]]; then
        echo "Config not found: $TETHER_CONFIG_FILE"
        echo "Clone younsl/addons to $ADDONS_DIR first."
        return 1
    fi

    if "$runtime" container inspect "$TETHER_NAME" &>/dev/null; then
        echo "Replacing existing $TETHER_NAME container ..."
        "$runtime" rm -f "$TETHER_NAME" >/dev/null
    fi

    "$runtime" run -d \
        --name "$TETHER_NAME" \
        --restart unless-stopped \
        --user "$(id -u):$(id -g)" \
        -e HOME="$HOME" \
        -e LOG_FORMAT=text \
        -v "$HOME:$HOME" \
        -v "$TETHER_CONFIG_FILE:/etc/tether/config.toml:ro" \
        -p "127.0.0.1:$TETHER_PORT:8080" \
        "$TETHER_IMAGE" >/dev/null

    echo "$TETHER_NAME started. Check the first reconcile with:"
    echo "  curl -s 127.0.0.1:$TETHER_PORT/status"
}

prompt_user() {
    read "REPLY?Do you want to proceed with this action? (yY/n): " || REPLY=""
    echo
}

install_precommit() {
  if ! command -v pre-commit &>/dev/null; then
    echo "pre-commit not found. Installing via Homebrew ..."
    brew install pre-commit
  else
    pre_commit_version=$(pre-commit --version)
    echo "pre-commit $pre_commit_version is already installed."
  fi

  printf "\n"
}

configure_brew_autoupdate() {
  if ! command -v brew &>/dev/null; then
    echo "Homebrew is not installed. Skipping brew autoupdate configuration."
    printf "\n"
    return
  fi

  local autoupdate_interval=86400

  echo "Configuring brew autoupdate ..."

  if ! brew tap | grep -q "homebrew/autoupdate"; then
    echo "Installing homebrew/autoupdate tap ..."
    brew tap homebrew/autoupdate
  else
    echo "homebrew/autoupdate tap is already installed."
  fi

  if brew autoupdate status 2>/dev/null | grep -q "running"; then
    echo "Brew autoupdate is already configured and running."
  else
    echo "Starting brew autoupdate with $autoupdate_interval seconds interval ..."
    brew autoupdate start $autoupdate_interval --upgrade --cleanup --immediate
    echo "Brew autoupdate configured successfully!"
  fi

  printf "\n"
}

main() {
    local runtime
    if ! runtime=$(container_runtime); then
        echo "Neither docker nor podman found. Install one to run tether."
        exit 1
    fi

    print_plan
    prompt_user

    if [[ $REPLY =~ ^[Yy]$ ]]; then
        echo ""
        echo "=========================================="
        echo "Starting tether ..."
        echo "=========================================="
        start_tether "$runtime"

        echo ""
        echo "=========================================="
        echo "Setting up pre-commit hooks ..."
        echo "=========================================="
        install_precommit
        (cd "$ADDONS_DIR" && pre-commit install)

        echo ""
        echo "=========================================="
        echo "Configuring brew autoupdate ..."
        echo "=========================================="
        configure_brew_autoupdate
    else
        echo "Operation cancelled."
    fi
}

main
