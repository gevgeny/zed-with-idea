<div align="center">

# Zed Plus

**A fork of [Zed](https://github.com/zed-industries/zed) that pulls its panels out into windows of their own.**

[![Based on Zed](https://img.shields.io/badge/based%20on-Zed-084CCF)](https://github.com/zed-industries/zed)
[![License: GPL v3](https://img.shields.io/badge/license-GPL--3.0-blue)](./LICENSE-GPL)
![Built by Claude](https://img.shields.io/badge/built%20by-Claude-D97757)

</div>

> [!WARNING]
> **Every line here was written by Claude.** I directed the work and reviewed it, but only
> superficially — I don't write Rust, and I don't know Zed's internals well enough to catch a
> subtle mistake in them.
>
> Treat this as a personal build, not a maintained project. It works for what I use it for.
> Nothing guarantees it works for anything else: there are no tests beyond what upstream already
> had, and the changes to Zed's own crates have only been checked by someone unqualified to
> check them.

## Why

Zed keeps a lot of what you need in docked panels, but a dock shows one of them at a time.
Wanting the git panel and the agent side by side means swapping between them all day, on a screen
that usually has room for both.

This fork moves those two into real windows:

- **Beside the editor**, both visible at once
- **On a second display**, out of the way of the code
- **Over the editor**, opened and dismissed like a dialog

The window is another way to reach the same panel, not a copy of it — same conversation, same
staged files, same state.

## Features

| Window | Shortcut | |
| --- | --- | --- |
| **Agent** | <kbd>cmd</kbd>+<kbd>alt</kbd>+<kbd>u</kbd> | Agent panel and threads sidebar, side by side |
| **Git** | <kbd>cmd</kbd>+<kbd>alt</kbd>+<kbd>v</kbd> | Changes tree, commit box and a side-by-side diff |

On Linux and Windows, <kbd>ctrl</kbd> replaces <kbd>cmd</kbd>.

Both windows follow the editor. Activating a thread that belongs to another worktree switches the
editor to it, and the window with it.

### Agent window

<!-- screenshot: docs/screenshots/agent-window.png -->

### Git window

<!-- screenshot: docs/screenshots/git-window.png -->

## Building

There are no prebuilt releases — build it the way you would build Zed:

```sh
git clone https://github.com/gevgeny/zed-with-idea.git
cd zed-with-idea
cargo run
```

See upstream's [development docs](./docs/src/development) for platform prerequisites.

---

<div align="center">

Everything not described above is upstream Zed, unchanged —
including [licensing](./LICENSE-GPL) and how the editor itself works.

</div>
