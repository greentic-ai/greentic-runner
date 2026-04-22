# Documentation Index

Use this page as the simple map for the `greentic-runner` docs.

If you are new to this repository, do not start with the oldest design notes.
Start with the document that matches your role.

## Start Here

- [../README.md](../README.md)
  Human-first overview of what this repository is, what it is for, and how it fits with the rest of Greentic.

- [coding-agents.md](coding-agents.md)
  Instructions for coding agents and contributors who need to work across `greentic-runner`, `gtc`, `greentic-pack`, and `greentic-dev` without mixing up responsibilities.

- [../crates/greentic-runner-host/README.md](../crates/greentic-runner-host/README.md)
  Lower-level runtime details for people debugging execution internals.

## Current Reference Docs

- [runner-cache.md](runner-cache.md)
  Component cache behavior, warmup, pruning, and troubleshooting.

- [fault-injection.md](fault-injection.md)
  Fault matrix format and local conformance testing.

- [pack-resolution-testing.md](pack-resolution-testing.md)
  Pack resolution property tests and regression seeds.

## Older Design And Transition Notes

These documents can still be useful, but they are not the best starting point for most readers and may describe transitions, historical plans, or older recommended surfaces.

- [vision/README.md](vision/README.md)
- [vision/canonical-v0.6.md](vision/canonical-v0.6.md)
- [vision/legacy.md](vision/legacy.md)
- [vision/deprecations.md](vision/deprecations.md)

## Historical Snapshots

These are reference notes for archaeology and comparison. Treat them as historical material rather than primary guidance.

- [runner-host-inventory.md](runner-host-inventory.md)
- [runner_current_behaviour.md](runner_current_behaviour.md)
- [pr08_scope.md](pr08_scope.md)

## If Documents Disagree

Use this order:

1. Current code
2. Focused tests
3. Crate-level docs
4. Historical notes
