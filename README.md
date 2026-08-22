# Zed Plus

A fork of [Zed](https://github.com/zed-industries/zed) that pulls its panels out into windows of their own.

---

## Important notice

**Every line here was written by Claude.** I directed the work and reviewed it, but only superficially — I don't write Rust, and I don't know Zed's internals well enough to catch a subtle mistake in them.

So treat this as a personal build rather than a maintained project. It works for what I use it for. Nothing guarantees it works for anything else: there are no tests beyond what upstream already had, and the changes to Zed's own crates have only been checked by someone unqualified to check them.

## Features

### Agent window — `cmd-alt-u`

The agent panel and the threads sidebar, side by side, in a window of their own. It follows the editor: activating a thread from another worktree switches both.

<!-- screenshot: agent window -->

### Git window — `cmd-alt-v`

The git changes tree, commit box and a side-by-side diff, in a window of their own. Also follows the editor from one worktree to the next.

<!-- screenshot: git window -->

---

Upstream Zed's README is at [zed-industries/zed](https://github.com/zed-industries/zed). Building, contributing and licensing are unchanged; see [docs/src/development](./docs/src/development) and [LICENSE-GPL](./LICENSE-GPL).
