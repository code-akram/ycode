use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);
const THREAD_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
const SHELL_PROMPT: &str = "YCODE_SHELL_READY> ";

#[test]
fn mini_frontend_resume_resize_exit_restores_normal_shell() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_state(codex_home.path(), &repo_root)?;

    let mut terminal = PtyShell::start(&repo_root, codex_home)?;
    terminal.wait_for_output(SHELL_PROMPT, /*output_start*/ 0, TIMEOUT)?;
    let shell_modes = terminal.terminal_modes()?;
    let codex = codex_utils_cargo_bin::cargo_bin("codex")?;
    terminal.write_input(
        format!(
            "{} resume {} -C {}\n",
            shell_quote(&codex.to_string_lossy()),
            THREAD_ID,
            shell_quote(&repo_root.to_string_lossy()),
        )
        .as_bytes(),
    )?;
    terminal.wait_for_frontend()?;

    ensure!(
        !contains_bytes(&terminal.output, b"\x1b[?1049h")
            && !contains_bytes(&terminal.output, b"\x1b[?47h")
            && !contains_bytes(&terminal.output, b"\x1b[?1047h"),
        "default chat entered an alternate screen",
    );
    terminal.resize(/*rows*/ 24, /*cols*/ 90)?;
    terminal.wait_for_screen("resumed prompt", Duration::from_secs(3))?;

    let exit_output_start = terminal.output.len();
    terminal.write_input(&[0x04])?;
    terminal.wait_for_output(SHELL_PROMPT, exit_output_start, TIMEOUT)?;
    let exit_output = String::from_utf8_lossy(&terminal.output[exit_output_start..]);
    ensure!(
        exit_output.contains("ycode"),
        "missing permanent ycode exit identity"
    );
    ensure!(
        exit_output.contains("Continue")
            && exit_output.contains("codex resume")
            && exit_output.contains(THREAD_ID),
        "missing working continuation command after exit: {exit_output:?}",
    );
    ensure!(
        contains_bytes(&terminal.output[exit_output_start..], b"\x1b[?2004l")
            && contains_bytes(&terminal.output[exit_output_start..], b"\x1b[?25h"),
        "exit did not disable bracketed paste and restore the cursor",
    );
    let restored_modes = terminal.terminal_modes()?;
    ensure!(
        restored_modes == shell_modes,
        "terminal modes changed across frontend handoff: before={shell_modes:?}, after={restored_modes:?}"
    );
    Ok(())
}

struct PtyShell {
    master: File,
    slave_terminal: File,
    child: Child,
    parser: vt100::Parser,
    output: Vec<u8>,
    cursor_answered: bool,
    palette_answered: bool,
    keyboard_answered: bool,
    _codex_home: TempDir,
}

#[derive(Debug, PartialEq, Eq)]
struct TerminalModeSnapshot {
    input: libc::tcflag_t,
    output: libc::tcflag_t,
    control: libc::tcflag_t,
    local: libc::tcflag_t,
    characters: Vec<libc::cc_t>,
}

impl PtyShell {
    fn start(repo_root: &Path, codex_home: TempDir) -> Result<Self> {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut window_size = libc::winsize {
            ws_row: 32,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both descriptors on success and only borrows this size.
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut window_size,
            )
        };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("open frontend pseudo-terminal");
        }

        // SAFETY: successful openpty transferred ownership of both unique descriptors.
        let master = File::from(unsafe { OwnedFd::from_raw_fd(master_fd) });
        set_nonblocking(&master)?;
        // SAFETY: slave_fd is the other unique descriptor returned by openpty.
        let slave = File::from(unsafe { OwnedFd::from_raw_fd(slave_fd) });
        let stdin = slave.try_clone().context("clone pseudo-terminal stdin")?;
        let stdout = slave.try_clone().context("clone pseudo-terminal stdout")?;
        let slave_terminal = slave
            .try_clone()
            .context("clone pseudo-terminal mode descriptor")?;

        let mut command = Command::new("/bin/zsh");
        command
            .arg("-f")
            .env("PS1", SHELL_PROMPT)
            .env("TERM", "xterm-256color")
            .env("CODEX_HOME", codex_home.path())
            .current_dir(repo_root)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(slave);
        // SAFETY: Command has installed the PTY slave on fd 0. The new session has no controlling
        // terminal, so assigning it here reproduces normal interactive job control and SIGWINCH.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(/*fd*/ 0, libc::TIOCSCTTY.into(), /*arg*/ 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().context("start shell in pseudo-terminal")?;

        Ok(Self {
            master,
            slave_terminal,
            child,
            parser: vt100::Parser::new(32, 120, /*scrollback_len*/ 200),
            output: Vec::new(),
            cursor_answered: false,
            palette_answered: false,
            keyboard_answered: false,
            _codex_home: codex_home,
        })
    }

    fn wait_for_frontend(&mut self) -> Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(50))?;
            self.answer_terminal_queries()?;
            if self.palette_answered
                && self.screen_contains("resumed prompt")
                && self.screen_contains("gpt-5.6-terra")
            {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "shell exited before frontend startup ({status}); screen:\n{}",
                    self.screen_contents()
                );
            }
        }
        bail!(
            "frontend did not initialize within {TIMEOUT:?}; screen:\n{}",
            self.screen_contents()
        )
    }

    fn wait_for_screen(&mut self, text: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(50))?;
            if self.screen_contains(text) {
                return Ok(());
            }
        }
        bail!(
            "timed out waiting for {text:?} on resized screen; screen:\n{}",
            self.screen_contents()
        )
    }

    fn wait_for_output(
        &mut self,
        text: &str,
        output_start: usize,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(50))?;
            if contains_bytes(&self.output[output_start..], text.as_bytes()) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                bail!("shell exited before {text:?} appeared ({status})");
            }
        }
        bail!("timed out waiting for {text:?}")
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the master descriptor is valid and size has the ioctl's expected layout.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(std::io::Error::last_os_error()).context("resize pseudo-terminal");
        }
        self.parser = vt100::Parser::new(rows, cols, /*scrollback_len*/ 200);
        Ok(())
    }

    fn terminal_modes(&self) -> Result<TerminalModeSnapshot> {
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: attrs is writable and the master descriptor remains open.
        if unsafe { libc::tcgetattr(self.slave_terminal.as_raw_fd(), attrs.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error()).context("read restored terminal modes");
        }
        // SAFETY: successful tcgetattr initialized attrs.
        let attrs = unsafe { attrs.assume_init() };
        Ok(TerminalModeSnapshot {
            input: attrs.c_iflag,
            output: attrs.c_oflag,
            control: attrs.c_cflag,
            local: attrs.c_lflag,
            characters: attrs.c_cc.to_vec(),
        })
    }

    fn answer_terminal_queries(&mut self) -> Result<()> {
        if !self.cursor_answered && contains_bytes(&self.output, b"\x1b[6n") {
            self.write_input(b"\x1b[1;1R")?;
            self.cursor_answered = true;
        }
        if !self.keyboard_answered && contains_bytes(&self.output, b"\x1b[?u") {
            self.write_input(b"\x1b[?0u\x1b[?1;2c")?;
            self.keyboard_answered = true;
        }
        if !self.palette_answered
            && contains_bytes(&self.output, b"\x1b]10;?")
            && contains_bytes(&self.output, b"\x1b]11;?")
        {
            self.write_input(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\")?;
            self.palette_answered = true;
        }
        Ok(())
    }

    fn read_output(&mut self, timeout: Duration) -> Result<()> {
        let timeout_ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized poll descriptor.
        let ready = unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, timeout_ms)
        };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error).context("poll frontend pseudo-terminal");
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(());
        }
        let mut chunk = [0_u8; 8192];
        let count = match self.master.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("read frontend pseudo-terminal"),
        };
        self.output.extend_from_slice(&chunk[..count]);
        self.parser.process(&chunk[..count]);
        Ok(())
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()?;
        Ok(())
    }

    fn screen_contains(&self, text: &str) -> bool {
        self.parser.screen().contents().contains(text)
    }

    fn screen_contents(&self) -> String {
        self.parser.screen().contents()
    }
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn write_test_state(codex_home: &Path, repo_root: &Path) -> Result<()> {
    let config = format!(
        "model = \"gpt-5.6-terra\"\nmodel_provider = \"openai\"\n\
         suppress_unstable_features_warning = true\n\n\
         [projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        repo_root.display(),
    );
    std::fs::write(codex_home.join("config.toml"), config)?;
    let id_token = concat!(
        "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.",
        "eyJlbWFpbCI6InB0eUBleGFtcGxlLmNvbSIsImh0dHBzOi8vYXBpLm9wZW5haS5jb20v",
        "YXV0aCI6eyJjaGF0Z3B0X3VzZXJfaWQiOiJwdHktdXNlciIsInVzZXJfaWQiOiJwdHktdXNl",
        "ciIsImNoYXRncHRfcGxhbl90eXBlIjoicHJvIiwiY2hhdGdwdF9hY2NvdW50X2lkIjoicHR5",
        "LWFjY291bnQifX0.c2ln",
    );
    let auth = serde_json::json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token,
            "access_token": "pty-access-token",
            "refresh_token": "pty-refresh-token"
        },
        "last_refresh": "2026-08-12T00:00:00Z"
    });
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::to_vec_pretty(&auth)?,
    )?;

    let thread_id = ThreadId::from_string(THREAD_ID)?;
    let timestamp = "2026-08-12T00:00:00Z";
    let session_meta = SessionMeta {
        session_id: thread_id.into(),
        id: thread_id,
        timestamp: timestamp.to_string(),
        cwd: repo_root.to_path_buf(),
        originator: "codex".to_string(),
        cli_version: "0.0.0".to_string(),
        source: codex_protocol::protocol::SessionSource::Cli,
        model_provider: Some("openai".to_string()),
        ..Default::default()
    };
    let payload = serde_json::to_value(SessionMetaLine {
        meta: session_meta,
        git: None,
    })?;
    let lines = [
        serde_json::json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": payload,
        }),
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "message": "resumed prompt",
                "kind": "plain",
            },
        }),
    ];
    let rollout = codex_home.join(format!(
        "sessions/2026/08/12/rollout-2026-08-12T00-00-00-{THREAD_ID}.jsonl"
    ));
    std::fs::create_dir_all(rollout.parent().context("rollout parent")?)?;
    let contents = lines
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(rollout, contents)?;
    Ok(())
}

fn set_nonblocking(file: &File) -> Result<()> {
    // SAFETY: file owns a valid descriptor and F_GETFL/F_SETFL do not outlive this call.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("read pseudo-terminal flags");
    }
    // SAFETY: descriptor and flags are valid and O_NONBLOCK is a supported status flag.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("set pseudo-terminal nonblocking");
    }
    Ok(())
}

fn contains_bytes(buffer: &[u8], needle: &[u8]) -> bool {
    buffer.windows(needle.len()).any(|window| window == needle)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
