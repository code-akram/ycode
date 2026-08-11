> [!IMPORTANT]
> **ycode** is an independent experimental terminal-only project based on
> OpenAI Codex `rust-v0.147.0`. It is not official OpenAI software and is
> currently undergoing deliberate subtraction.

ycode is a Rust terminal agent for Apple Silicon macOS. The repository retains
upstream names in the source while the product is being reduced deliberately.

## Quickstart

### Installing and running ycode

Build from source with Cargo:

```shell
cd codex-rs
cargo build -p codex-cli
```

When native releases begin, the POSIX installer will install the Apple Silicon
macOS binary:

```shell
curl -fsSL https://raw.githubusercontent.com/code-akram/ycode/main/scripts/install/install.sh | sh
```

Then run `codex`; renaming the installed executable is deferred.

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Installing & building**](./docs/install.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
