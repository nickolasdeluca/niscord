# AGENTS.md

Rules for everyone who changes this repository, people and AI agents alike.
Where a rule says "the maintainer", it means the repository owner, Nickolas.
Agents follow these rules over their own defaults.

## The project

Niscord is private, self-hosted screen sharing for a group of friends: a native
Rust desktop app (Slint GUI, Windows) and a small signaling server (Linux or
Windows). See `README.md` for the layout, how it works and how to run it.

```
crates/protocol   messages shared by app and server
crates/server     signaling server
crates/media      capture, H.264 (GPU via Media Foundation, or OpenH264), audio
crates/transport  WebRTC (webrtc-rs)
crates/app        the desktop app (niscord.exe)
deploy/           Ubuntu install script, systemd unit
scripts/          maintenance scripts
```

## Commits

### Authorship

- **Commits are authored by the human who asked for the change.** Agents commit
  only as that person, with their configured git identity.
- **No co-author trailers.** Never add `Co-Authored-By:` (any capitalisation),
  for an AI or anyone else, unless the maintainer explicitly asks for one in a
  specific commit.
- **No AI attribution anywhere** in commit messages, pull request descriptions,
  release notes or code comments: no "Generated with Claude Code", no robot
  emoji footers, no session links.
- This is enforced: `scripts/check-commit-msg.sh` rejects such messages, as a
  `commit-msg` hook locally and in CI. Enable the hook once per clone:

  ```sh
  git config core.hooksPath .githooks
  ```

  Never bypass it (`--no-verify`) or rewrite it to let a trailer through.

### Messages

Follow the style of the existing history ([Conventional Commits](https://www.conventionalcommits.org)):

```
<type>[(scope)]: <what changed, imperative, lower case, no trailing period>

<why it changed and anything a reviewer couldn't tell from the diff:
the problem, measurements, trade-offs. Wrap at ~72 columns.>
```

- Types: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `chore`, `ci`.
  Scopes are optional, e.g. `fix(deploy):`.
- The subject says what the change does for Niscord, not which files moved.
- The body explains *why*. For fixes, say what was broken and how it showed
  up; when there were measurements (latency, bitrate, sizes), give the numbers.
- One logical change per commit. Don't mix a feature with an unrelated fix.

### What goes in a commit

- Never commit secrets: passwords, TURN secrets, tokens, private keys.
- **Never commit the maintainer's server address**, domain or IP, or anything
  else that identifies the private deployment, in code, docs, tests, commit
  messages or CI variables. Use `wss://niscord.example.com` in examples. The
  address is only baked into private builds made locally with
  `NISCORD_DEFAULT_SERVER`.
- No build output (`target/`), logs, local settings, or scratch files.
- Scripts that run on Linux (`*.sh`, `deploy/`) keep LF line endings
  (`.gitattributes` enforces this).

## Branches, pushes and releases

- Work on a branch named `feat/…`, `fix/…` or `chore/…`, then fast-forward
  merge it into `main` (`git merge --ff-only`) and delete it. No merge commits.
- **Agents never push, tag, create releases, or change GitHub settings without
  the maintainer's go-ahead for that specific action.** Approval for one push
  doesn't cover the next.
- Never force-push `main` or rewrite pushed history. Amending is fine only for
  commits that haven't been pushed.
- **Releases:** bump `version` in the root `Cargo.toml`, commit it as
  `chore: release X.Y.Z`, then push the matching tag `vX.Y.Z`. The release
  workflow builds and publishes it, and installed apps update themselves from
  it, so a release reaches every user within hours. Public releases never
  carry a server address.

## Before committing

Run what CI runs, and fix every warning (CI fails on them):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Building needs NASM on `PATH` (see README). Additionally:

- New behaviour comes with tests. Bug fixes come with a test that fails
  without the fix.
- Tests must be deterministic. Hardware-dependent tests (GPU encoder, audio
  devices) skip themselves when the hardware is missing; tests that aren't
  about the encoder use `EncoderPreference::Software`. Tests bind to
  `127.0.0.1` only.
- Check that changed install or deploy scripts at least parse
  (`bash -n deploy/install.sh`, the PowerShell parser for `.ps1` files).

## Code

- Match the surrounding code: naming, error handling, comment density.
  `rustfmt.toml` sets the format (120 columns).
- Comments explain *why* (constraints, measurements, platform quirks), not
  what the next line does. Every `unsafe` block gets a `// SAFETY:` comment.
- Keep dependencies few. New ones must have licenses compatible with the
  project's MIT license (MIT, Apache-2.0, BSD, ISC, Zlib and similar). No GPL
  dependencies. Slint is used under its royalty-free license, which requires
  the About dialog's attribution to stay.
- User-facing text is plain, short English, written for friends who aren't
  technical.

## The machine you work on

- Don't change things outside the repository (system settings, global
  toolchains, other repositories, the maintainer's Niscord settings or
  installation) without asking.
- Ask before driving the desktop (synthetic input, moving or focusing
  windows): the maintainer, or another agent, may be using it.
- Build output grows quickly; `scripts\clean.ps1` frees it (`-WhatIf` first to
  preview).
