# ycode repository instructions

- Support macOS and Linux only. Do not add or maintain Windows support.
- Apply the principle of subtraction: prefer removing unnecessary code and
  infrastructure over adding compatibility layers or replacement machinery.
- Keep each deletion coherent, and make one coherent deletion per commit.
- Use Cargo or `just` for targeted, incremental checks of the affected crate or
  package.
- Do not run full-workspace checks or release builds without explicit approval.
- Preserve `LICENSE`, `NOTICE`, and `UPSTREAM.md`.
- Treat `upstream` as reference-only. Never push to it or merge from it.
- Read and follow any more-specific nested `AGENTS.md` before changing files in
  its subtree.
