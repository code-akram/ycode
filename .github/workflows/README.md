# Workflow Strategy

The workflows in this directory keep pull-request feedback focused while
retaining a broader Cargo verification pass on `main`.

## Pull Requests

`rust-ci.yml` runs the targeted Cargo checks used for routine changes, including
formatting, dependency hygiene, and the argument-comment lint package tests.

## Post-Merge On `main`

`rust-ci-full.yml` retains the heavier Cargo checks and platform test matrix.
Run or expand that workflow only when the broader coverage is warranted.
