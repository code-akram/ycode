# ycode repository instructions

## Product boundaries

- ycode's initial product is a Rust CLI for `aarch64-apple-darwin` only. Preserve
  the interactive TUI and non-interactive exec mode, including machine-readable
  output. Unsupported targets must fail with a simple unsupported-target
  message.
- Initial user workflows are interactive CLI, non-interactive exec, login,
  logout, resume, fork, help, and version, plus the core primitives they need.
- Build and installation surfaces are Cargo/source and a POSIX curl installer
  for native binaries. Do not distribute ycode through npm.
- Product code and substantial development tooling should be Rust. POSIX shell
  is allowed for bootstrap and installation. Python and Node tooling are
  subtraction targets. Keep `just` temporarily; simplify or remove it later.
- Do not add CI or release workflows until ycode begins publishing releases.
- Preserve local conversation history, persistent terminal-agent sessions, and
  resume behavior.
- Preserve local `AGENTS.md` instructions, filesystem skills, and ordinary local
  logs.
- ycode is a multi-agent CLI. Preserve its built-in collaboration, delegation,
  agent lifecycle, messaging, orchestration, and required shared protocol
  surfaces. External orchestration may complement but does not replace them.
- Preserve built-in OpenAI web search and local image inspection, including
  their required protocol/tool plumbing and tests.
- Support only official OpenAI and ChatGPT endpoints. Azure OpenAI,
  third-party/OpenAI-compatible endpoints, arbitrary API base URLs, local-model
  providers, alternate-provider authentication, and provider-selection
  machinery are subtraction targets.
- Standardize official model traffic on the OpenAI Responses API. Legacy Chat
  Completions transport and protocol support are subtraction targets, but do
  not remove shared response types merely by name.
- Preserve official OpenAI model selection, reasoning-effort controls, model
  discovery/metadata needed by first-party paths, and free-form official model
  IDs through CLI/config. Free-form IDs must apply only to official OpenAI and
  ChatGPT endpoints and must not reintroduce provider selection, custom base
  URLs, or compatibility transports.
- Make no compatibility promise for legacy Codex configuration fields,
  deprecated CLI aliases, or migration code. Use `~/.ycode/auth.json` as the
  default authentication credential location and preserve its existing JSON
  format. Preserve other configuration until its dedicated cleanup.
- Plan mode and dedicated code-review mode are later subtraction targets. Main
  and sub-agents will ultimately run with full access; native multi-agent
  collaboration remains protected.
- Existing plugins and hooks are later subtraction targets. Preserve
  `AGENTS.md` instructions and filesystem-backed skills. Do not design or
  describe a replacement extension system yet.
- Updates are manual through Cargo or the curl installer. Automatic update
  checks and notices are later subtraction targets.
- Only ordinary local logs and explicit OpenAI or web requests may remain.
  Feedback upload, crash upload, analytics, experiments, and remote feature
  flags are later subtraction targets.
- TUI and theme preference machinery is a later subtraction target. Preserve
  the current functional interactive TUI until its dedicated redesign.

## Protected first-party authentication and backend boundary

This boundary overrides every subtraction target. Do not change or delete the
following without direct user approval for the specific change:

- native ChatGPT OAuth/subscription login and browser authorization;
- direct OpenAI API-key authentication through its existing environment/config
  path;
- authentication token acquisition, storage, and refresh;
- ChatGPT account, session, entitlement, and model-access handling;
- the default ChatGPT credential location (`~/.ycode/auth.json`) and existing
  JSON format; or
- official OpenAI backend/client bindings required by either protected
  first-party authentication path.

Authentication should ultimately be file-only. Use `~/.ycode/auth.json` by
default while preserving `CODEX_HOME` as the explicit compatibility and test
override. Do not import or fall back to `~/.codex`. Preserve the existing JSON
format and working ChatGPT login. Keychain and keyring storage are later
subtraction targets. OpenAI API keys must come only from the environment and
must not be persisted.

Third-party login methods, alternate-provider authentication, Azure/compatible
endpoints, arbitrary proxies, and custom API base URLs are not protected by
this boundary. Before deleting shared transport code, prove it is not required
by either protected official path.

## Approved subtraction targets

- Remove the entire existing stateful MCP client/server implementation and all
  of its integrations as one dedicated subtraction. A new stateless MCP client
  is a later project; do not add a replacement during removal.
- Remove upstream telemetry, analytics, feedback upload, and remote diagnostic
  reporting while retaining ordinary local logs.
- Retain local `AGENTS.md` instructions and filesystem skills, but remove the
  current plugin catalog/system, connectors, hooks, and external-agent migration
  machinery until extensibility is deliberately redesigned.
- Remove alternate providers such as AWS/Bedrock, Ollama, and LM Studio.
- In a dedicated future phase, remove the retained macOS sandbox implementation,
  sandbox-mode configuration, sandbox/approval selection UI, command-approval
  prompts and single-purpose approval-request plumbing, plus enterprise-managed
  policy/requirements machinery. The steady state is unrestricted filesystem
  and process access with a never-ask approval policy.
- Preserve agent-internal safety logic that independently prevents accidental
  destructive behavior until it is separately reviewed.
- If a proposed deletion is coupled to protected authentication, official
  Responses traffic, web search, image inspection, sessions/resume, exec output,
  or native collaboration, stop and report the dependency before removing it.

## Repository workflow

- Apply the principle of subtraction: prefer removing unnecessary code and
  infrastructure over adding compatibility layers or replacement machinery.
- Keep each deletion coherent, with one coherent deletion per commit.
- Use Cargo or `just` for targeted, incremental checks of affected crates.
- Do not run full-workspace checks or release builds without explicit approval.
- Preserve `LICENSE`, `NOTICE`, and `UPSTREAM.md`.
- Never push to or merge from upstream. `/Users/akram/code/codex` is the
  dedicated upstream reference clone.
- Read and follow every more-specific nested `AGENTS.md` before changing files
  in its subtree.
