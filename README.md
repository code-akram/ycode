# ycode

ycode is an independent Rust terminal agent for Apple Silicon macOS. Its only
supported target is `aarch64-apple-darwin`. The installed product is the `codex`
CLI plus its adjacent `codex-code-mode-host` child-process companion; these
executable names remain unchanged for now.

The CLI provides an interactive TUI and noninteractive `exec` mode, including
machine-readable output. It uses the OpenAI Responses API, supports native
ChatGPT subscription login and environment-only `CODEX_API_KEY` authentication,
and retains local sessions, resume/fork, collaboration, web and image tools,
shell and patch execution, `AGENTS.md`, and filesystem-backed skills.

Build and run from source:

```sh
cd codex-rs
just build
./target/debug/codex --help
./target/debug/codex-code-mode-host --help
```

Authentication remains at `CODEX_HOME/auth.json` with its existing wire format.
API keys are read only from `CODEX_API_KEY` and are never persisted.

See [docs/install.md](docs/install.md) for the exact development, validation,
and manual installation baseline. This repository is licensed under the
[Apache-2.0 License](LICENSE).
