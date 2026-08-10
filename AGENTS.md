# ycode repository instructions

- ycode is a terminal-only product.
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

## Protected native ChatGPT subscription boundary

Do not change or delete any of the following without direct user approval for
the specific change:

- native ChatGPT subscription login and browser authorization;
- authentication token acquisition, storage, and refresh;
- ChatGPT account and session handling;
- subscription entitlement and model access; or
- backend and client bindings required by the native ChatGPT subscription path.
