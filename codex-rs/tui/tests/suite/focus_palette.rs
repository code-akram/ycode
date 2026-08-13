use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use base64::Engine;
use tempfile::TempDir;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 15);
const FOCUS_INPUT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);
const FOCUS_PROBE_INPUT: &str = "focus-palette-24527";
const STATUS_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 8);

#[test]
fn focus_gained_with_unanswered_palette_queries_preserves_immediate_input() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;

    let mut terminal = PtyCodex::start(&repo_root, codex_home)?;
    terminal.wait_for_startup()?;

    let startup_output_len = terminal.output.len();
    let focus_started = Instant::now();
    terminal.write_input(format!("\u{1b}[I{FOCUS_PROBE_INPUT}").as_bytes())?;
    terminal.wait_for_focus_input(FOCUS_PROBE_INPUT, focus_started, startup_output_len)?;

    let delayed_input = format!("{FOCUS_PROBE_INPUT}-delayed");
    let delayed_focus_started = Instant::now();
    terminal.write_input(b"\x1b[I")?;
    terminal.read_output(Duration::from_millis(/*millis*/ 20))?;
    terminal.write_input(delayed_input.as_bytes())?;
    terminal.wait_for_focus_input(&delayed_input, delayed_focus_started, startup_output_len)?;

    Ok(())
}

#[test]
fn quiet_mini_pty_lifecycle_ticks_without_backend_events_and_restores_terminal() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;

    let mut terminal = PtyCodex::start(&repo_root, codex_home)?;
    terminal.wait_for_startup()?;

    let fresh = compact_screen(&terminal.screen_contents());
    insta::assert_snapshot!("quiet_mini_fresh_pty", fresh);
    ensure!(!terminal.screen_contains("context left"));
    ensure!(!terminal.screen_contains("/ commands"));
    ensure!(!terminal.screen_contains("gpt-5.6-terra"));
    ensure!(
        !contains_bytes(&terminal.output, b"\x1b[?1049h"),
        "fresh inline chat unexpectedly entered the alternate screen"
    );

    terminal.write_input(b"!sleep 2.4\r")?;
    let started = Instant::now();
    let mut observed = [None, None, None];
    while started.elapsed() < STATUS_TIMEOUT && observed[2].is_none() {
        terminal.read_output(Duration::from_millis(/*millis*/ 20))?;
        let screen = terminal.screen_contents();
        for second in 0..=2 {
            let suffix = format!(" {second}s");
            if status_line(&screen).is_some_and(|line| line.ends_with(&suffix))
                && observed[second].is_none()
            {
                observed[second] = Some(started.elapsed());
            }
        }
    }
    ensure!(
        observed.iter().all(Option::is_some),
        "silent shell work did not visibly progress through 0s/1s/2s: {observed:?}; screen:\n{}",
        terminal.screen_contents(),
    );
    let zero_seconds = observed[0].expect("checked above");
    let one_second = observed[1].expect("checked above");
    let two_seconds = observed[2].expect("checked above");
    ensure!(
        (Duration::from_millis(850)..=Duration::from_millis(1_250))
            .contains(&(one_second - zero_seconds)),
        "0s→1s tick cadence was not near one second: {:?}",
        one_second - zero_seconds,
    );
    ensure!(
        (Duration::from_millis(850)..=Duration::from_millis(1_250))
            .contains(&(two_seconds - one_second)),
        "2s tick cadence was not near one second: {:?}",
        two_seconds - one_second,
    );

    let screen = terminal.screen_contents();
    let status = status_line(&screen).context("missing Mini status line")?;
    ensure!(
        status.starts_with('•'),
        "reduced-motion glyph missing: {status:?}"
    );
    ensure!(!status.contains("Working"));
    ensure!(!status.contains('(') && !status.contains(')'));
    let status_row = screen
        .lines()
        .position(|line| line.trim() == status)
        .context("status row not found")?;
    let (cursor_row, _) = terminal.parser.screen().cursor_position();
    ensure!(
        status_row + 1 == usize::from(cursor_row),
        "status row must sit immediately above editor cursor: status={status_row}, cursor={cursor_row}"
    );
    insta::assert_snapshot!(
        "quiet_mini_active_pty",
        compact_screen(&terminal.screen_contents())
    );

    terminal.resize(/*rows*/ 24, /*cols*/ 80)?;
    terminal.read_output(Duration::from_millis(/*millis*/ 250))?;
    ensure!(terminal.parser.screen().size() == (24, 80));
    ensure!(!contains_bytes(&terminal.output, b"\x1b[?1049h"));

    terminal.wait_for_screen_text("(no output)", STATUS_TIMEOUT)?;
    terminal.wait_for_screen_text("gpt-5.6-terra default ·", STATUS_TIMEOUT)?;
    ensure!(
        status_line(&terminal.screen_contents()).is_none(),
        "status row remained visible after the completed turn"
    );
    let completed = compact_screen(&terminal.screen_contents());
    insta::assert_snapshot!("quiet_mini_shell_trace_pty", completed);
    terminal.exit_and_verify_restoration()?;
    Ok(())
}

struct PtyCodex {
    master: File,
    child: Child,
    parser: vt100::Parser,
    output: Vec<u8>,
    cursor_answered: bool,
    palette_answered: bool,
    keyboard_answered: bool,
    slave_probe: File,
    initial_lflag: libc::tcflag_t,
    _codex_home: TempDir,
}

impl PtyCodex {
    fn start(repo_root: &Path, codex_home: TempDir) -> Result<Self> {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut window_size = libc::winsize {
            ws_row: 32,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: `openpty` initializes both file descriptors on success, and the supplied window
        // size remains valid for the duration of the call.
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                /*name*/ std::ptr::null_mut(),
                /*termp*/ std::ptr::null_mut(),
                &raw mut window_size,
            )
        };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("open focus-test pseudo-terminal");
        }

        // SAFETY: a successful `openpty` transfers ownership of both unique file descriptors.
        let master = File::from(unsafe { OwnedFd::from_raw_fd(master_fd) });
        set_nonblocking(&master)?;
        // SAFETY: `slave_fd` is the second unique descriptor initialized by `openpty`.
        let slave = File::from(unsafe { OwnedFd::from_raw_fd(slave_fd) });
        let stdin = slave.try_clone().context("clone pseudo-terminal stdin")?;
        let stdout = slave.try_clone().context("clone pseudo-terminal stdout")?;
        let slave_probe = slave
            .try_clone()
            .context("clone pseudo-terminal restoration probe")?;
        let initial_lflag = terminal_lflag(&slave_probe)?;

        let codex = codex_utils_cargo_bin::cargo_bin("codex-tui")
            .or_else(|_| codex_utils_cargo_bin::cargo_bin("codex"))?;
        let child = Command::new(codex)
            .arg("-C")
            .arg(repo_root)
            .env("TERM", "xterm-256color")
            .env("CODEX_API_KEY", "focus-palette-test")
            .env("CODEX_HOME", codex_home.path())
            .stdin(stdin)
            .stdout(stdout)
            .stderr(slave)
            .spawn()
            .context("start Codex in focus-test pseudo-terminal")?;

        Ok(Self {
            master,
            child,
            parser: vt100::Parser::new(
                /*rows*/ 32, /*cols*/ 120, /*scrollback_len*/ 0,
            ),
            output: Vec::new(),
            cursor_answered: false,
            palette_answered: false,
            keyboard_answered: false,
            slave_probe,
            initial_lflag,
            _codex_home: codex_home,
        })
    }

    fn wait_for_startup(&mut self) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 50))?;
            self.answer_startup_queries()?;

            if self.palette_answered && self.screen_contains("Ask anything...") {
                return Ok(());
            }

            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "Codex exited before the focus test started ({status}); screen:\n{}",
                    self.screen_contents(),
                );
            }
        }

        bail!(
            "Codex did not initialize within {:?}; screen:\n{}",
            STARTUP_TIMEOUT,
            self.screen_contents(),
        );
    }

    fn wait_for_focus_input(
        &mut self,
        input: &str,
        focus_started: Instant,
        startup_output_len: usize,
    ) -> Result<()> {
        while focus_started.elapsed() < FOCUS_INPUT_TIMEOUT {
            self.read_output(Duration::from_millis(/*millis*/ 20))?;
            let focus_output = &self.output[startup_output_len..];
            ensure!(
                !contains_bytes(focus_output, b"\x1b]10;?")
                    && !contains_bytes(focus_output, b"\x1b]11;?"),
                "focus regain queried terminal colors after the startup palette was cached",
            );
            if self.screen_contains(input) {
                return Ok(());
            }
        }

        bail!(
            "focus-time palette refresh blocked or discarded {input:?} for more than {:?}; \
             screen:\n{}",
            FOCUS_INPUT_TIMEOUT,
            self.screen_contents(),
        );
    }

    fn wait_for_screen_text(&mut self, text: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 20))?;
            if self.screen_contains(text) {
                return Ok(());
            }
        }
        bail!(
            "screen never contained {text:?}:\n{}",
            self.screen_contents()
        )
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let window_size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the master is a valid PTY and `window_size` lives through the ioctl.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &window_size) } == -1 {
            return Err(std::io::Error::last_os_error()).context("resize pseudo-terminal");
        }
        // SAFETY: the child pid is live and SIGWINCH requests the ordinary terminal resize path.
        if unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGWINCH) } == -1 {
            return Err(std::io::Error::last_os_error()).context("signal terminal resize");
        }
        self.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    fn exit_and_verify_restoration(&mut self) -> Result<()> {
        self.write_input(b"\x03")?;
        self.read_output(Duration::from_millis(/*millis*/ 100))?;
        if self.child.try_wait()?.is_none() {
            self.write_input(b"\x03")?;
        }
        let deadline = Instant::now() + Duration::from_secs(/*secs*/ 5);
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 20))?;
            if self.child.try_wait()?.is_some() {
                let restored = terminal_lflag(&self.slave_probe)?;
                let restored_bits = libc::ICANON | libc::ECHO;
                ensure!(
                    restored & restored_bits == self.initial_lflag & restored_bits,
                    "terminal canonical/echo flags were not restored"
                );
                return Ok(());
            }
        }
        bail!(
            "Codex did not exit normally after Ctrl-C; screen:\n{}",
            self.screen_contents()
        )
    }

    fn answer_startup_queries(&mut self) -> Result<()> {
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

        // SAFETY: `descriptor` points to one initialized poll descriptor.
        let ready = unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, timeout_ms)
        };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error).context("poll focus-test pseudo-terminal");
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(());
        }

        let mut chunk = [0_u8; 8192];
        let count = match self.master.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("read focus-test pseudo-terminal"),
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

fn compact_screen(screen: &str) -> String {
    screen
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn status_line(screen: &str) -> Option<String> {
    screen.lines().find_map(|line| {
        let line = line.trim();
        let has_activity_glyph = line.starts_with('•')
            || line
                .chars()
                .next()
                .is_some_and(|character| ('⠁'..='⣿').contains(&character));
        (has_activity_glyph
            && line
                .split_whitespace()
                .nth(1)
                .is_some_and(|elapsed| elapsed.ends_with('s')))
        .then(|| line.to_string())
    })
}

impl Drop for PtyCodex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn contains_bytes(buffer: &[u8], needle: &[u8]) -> bool {
    buffer.windows(needle.len()).any(|window| window == needle)
}

fn set_nonblocking(file: &File) -> Result<()> {
    // SAFETY: `file` owns a valid descriptor and F_GETFL/F_SETFL do not outlive this call.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("read pseudo-terminal flags");
    }
    // SAFETY: the descriptor and flags are valid and O_NONBLOCK is a supported status flag.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("set pseudo-terminal nonblocking");
    }
    Ok(())
}

fn terminal_lflag(file: &File) -> Result<libc::tcflag_t> {
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `attributes` points to writable storage and the file owns a valid terminal fd.
    if unsafe { libc::tcgetattr(file.as_raw_fd(), attributes.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).context("read pseudo-terminal attributes");
    }
    // SAFETY: tcgetattr succeeded and initialized the structure.
    Ok(unsafe { attributes.assume_init() }.c_lflag)
}

fn write_test_config(codex_home: &Path, repo_root: &Path) -> Result<()> {
    let repo_root = repo_root.display();
    let config = format!(
        "model = \"gpt-5.6-terra\"\nmodel_provider = \"openai\"\n\
         suppress_unstable_features_warning = true\n\n\
         [projects.\"{repo_root}\"]\ntrust_level = \"trusted\"\n"
    );
    std::fs::write(codex_home.join("config.toml"), config)
        .context("write focus-test Codex configuration")?;
    // This isolated ChatGPT fixture lets the PTY smoke reach the frontend without running login
    // or submitting a model request.
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let id_token = format!(
        "{}.{}.{}",
        encode(br#"{"alg":"none","typ":"JWT"}"#),
        encode(
            br#"{"email":"pty@example.com","https://api.openai.com/auth":{"chatgpt_user_id":"pty-user","user_id":"pty-user","chatgpt_plan_type":"pro","chatgpt_account_id":"pty-account"}}"#,
        ),
        encode(b"sig"),
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
    )
    .context("write focus-test authentication fixture")?;
    Ok(())
}
