# codex-process-hardening

This crate provides `pre_main_hardening()`, which is designed to be called pre-`main()` (using `#[ctor::ctor]`) to perform various process hardening steps, such as

- disabling core dumps
- disabling ptrace attach on macOS
- removing dangerous `DYLD_*` environment variables
