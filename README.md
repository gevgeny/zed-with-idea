# Zed Plus

A fork of [Zed](https://github.com/zed-industries/zed) that pulls its panels out into windows of their own.

---

## Important notice

This fork is written by a developer with **no Rust experience** and no real knowledge of Zed's architecture. Every line of it was implemented by Claude.

Treat it accordingly: it is a personal build, not a maintained project. There is no test coverage beyond what upstream already had, no review by anyone who knows this codebase, and no guarantee that the changes to Zed's own crates are correct in cases nobody happened to try.

## Features

### Agent window — `cmd-alt-u`

The agent panel and the threads sidebar, side by side, in a window of their own. It follows the editor: activating a thread from another worktree switches both.

<!-- screenshot: agent window -->

### Git window — `cmd-alt-v`

The git changes tree, commit box and a side-by-side diff, in a window of their own. Also follows the editor from one worktree to the next.

<!-- screenshot: git window -->

---

Upstream Zed's README is at [zed-industries/zed](https://github.com/zed-industries/zed). Building, contributing and licensing are unchanged; see [docs/src/development](./docs/src/development) and [LICENSE-GPL](./LICENSE-GPL).
