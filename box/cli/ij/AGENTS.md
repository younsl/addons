# AGENTS.md

Guidance for coding agents working in this directory. Only non-derivable constraints and traps live
here. Features and usage are in `README.md`, targets in the `Makefile`, structure in `src/`.

## Overview

`ij` (Infra Janitor) is an EC2 operations CLI: SSM connect, port forwarding, AMI cleanup, ASG scaling.
Inspired by [gossm](https://github.com/gjbae1212/gossm).

Module-per-file under `src/`, 2018+ style (`foo.rs` beside `foo/`). `main.rs` stays a thin entry point —
new behavior goes in its own module, never back into `main.rs`.

## Configuration Precedence

Resolved in `Config::resolve` (`src/config.rs`), highest first:

1. `--profile` flag
2. positional argument (`ij prd`)
3. `AWS_PROFILE`
4. `aws_profile` in `~/.config/ij/config.yaml` (`$XDG_CONFIG_HOME/ij/config.yaml`, written by `ij init`)

Every setting must stay resolvable through this chain. Adding `#[arg(env = ...)]` to a clap field
bypasses it and silently reorders precedence.

## Traps

- **PTY, not a plain child process.** The SSM session runs inside a PTY with the terminal in raw mode
  so SSH-style escape sequences (`Enter ~ .` disconnect, `Enter ~ ?` help) can be intercepted before
  they reach the remote. Terminal settings must be restored on every exit path, including panics.
- **`SIGINT` and `SIGTSTP` are ignored in the parent** while a session is live, so they pass through to
  the remote shell. Handlers are restored to default after the session ends; leaving them ignored makes
  the CLI unkillable from its own terminal.
- **MFA resolves once per invocation.** STS credentials are fetched before the region scan so the user
  is prompted for a single OTP. Do not move credential resolution into the per-region scan path.

## AWS Permissions

Caller needs `ec2:DescribeInstances` (all resources), `ec2:StartInstances` / `ec2:StopInstances`, and
`ssm:StartSession` on instances plus the `AWS-StartPortForwardingSession` and
`AWS-StartPortForwardingSessionToRemoteHost` documents. Target instances need
`AmazonSSMManagedInstanceCore`.

## Prerequisites

AWS CLI v2, `session-manager-plugin`, configured credentials. Rust toolchain per `rust-version` in
`Cargo.toml`.
