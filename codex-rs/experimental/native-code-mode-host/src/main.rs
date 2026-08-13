#![cfg(target_os = "macos")]

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use codex_native_code_mode_host::{HostEventKind, Limits, NativeHost};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const EXPECTED_VERSION: &str = "rustc 1.95.0 (59807616e 2026-04-14)\nbinary: rustc\ncommit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860\ncommit-date: 2026-04-14\nhost: aarch64-apple-darwin\nrelease: 1.95.0\nLLVM version: 22.1.2\n";

#[tokio::main]
async fn main() {
    let arg0 = std::env::args_os().next().unwrap_or_default();
    let invoked_as = std::path::Path::new(&arg0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if invoked_as.contains("fake-rustc-sleep") {
        fake_rustc(true);
        return;
    }
    if invoked_as.contains("fake-rustc-wrong") {
        fake_rustc(false);
        return;
    }
    if invoked_as.contains("fake-rustc-invalid") {
        fake_rustc_invalid();
        return;
    }
    if invoked_as.contains("fake-rustc-partial") {
        fake_rustc_partial();
        return;
    }
    if let Err(error) = run_cli().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn fake_rustc_partial() {
    if std::env::args().any(|argument| argument == "--version") {
        print!("{EXPECTED_VERSION}");
        return;
    }
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let Some(output) = arguments
        .windows(2)
        .find(|pair| pair[0] == "-o")
        .map(|pair| PathBuf::from(&pair[1]))
    else {
        eprintln!("fake rustc expected -o OUTPUT");
        std::process::exit(2);
    };
    if let Err(error) = std::fs::write(output, b"partial compiler output") {
        eprintln!("fake rustc failed to write partial output: {error}");
        std::process::exit(2);
    }
    std::thread::sleep(Duration::from_secs(30));
}

fn fake_rustc_invalid() {
    if std::env::args().any(|argument| argument == "--version") {
        print!("{EXPECTED_VERSION}");
        return;
    }
    let prefix = b"error: invalid UTF-8 compiler diagnostic\n";
    let mut diagnostic = Vec::with_capacity(codex_native_code_mode_host::DIAGNOSTIC_BYTES);
    diagnostic.extend_from_slice(prefix);
    diagnostic.resize(codex_native_code_mode_host::DIAGNOSTIC_BYTES, 0xff);
    let _ = std::io::stderr().write_all(&diagnostic);
    std::process::exit(1);
}

fn fake_rustc(correct: bool) {
    if std::env::args().any(|argument| argument == "--version") {
        if correct {
            print!("{EXPECTED_VERSION}");
        } else {
            print!(
                "rustc 1.94.0\nrelease: 1.94.0\ncommit-hash: wrong\nhost: aarch64-apple-darwin\n"
            );
        }
        return;
    }
    std::thread::sleep(Duration::from_secs(30));
}

async fn run_cli() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let mode = args
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("usage: run|run-controlled RUSTC SDK ROOT SOURCE")?;
    if mode != "run" && mode != "run-controlled" {
        return Err("usage: run|run-controlled RUSTC SDK ROOT SOURCE".into());
    }
    let rustc = PathBuf::from(args.next().ok_or("missing rustc")?);
    let sdk = PathBuf::from(args.next().ok_or("missing SDK rlib")?);
    let root = PathBuf::from(args.next().ok_or("missing run root")?);
    let source_path = PathBuf::from(args.next().ok_or("missing source")?);
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let source = std::fs::read(&source_path).map_err(|error| error.to_string())?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let host = NativeHost::new(rustc, sdk, root, Limits::default())
        .await
        .map_err(|error| error.to_string())?
        .with_events(event_tx);
    let artifact = host.prepare(&source).map_err(|error| error.to_string())?;
    let cancellation = CancellationToken::new();
    let disconnect = async {
        if mode == "run-controlled" {
            let mut input = tokio::io::stdin();
            let mut buffer = [0_u8; 1];
            let _ = input.read(&mut buffer).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(disconnect);
    let mut disconnect_pending = true;
    let run_id = artifact.run_id().to_string();
    let run_dir = artifact.run_dir().to_path_buf();
    let run = host.execute(&artifact, cancellation.clone());
    tokio::pin!(run);
    let report = loop {
        tokio::select! {
            report = &mut run => break report,
            _ = &mut disconnect, if disconnect_pending => {
                disconnect_pending = false;
                cancellation.cancel();
            }
            event = event_rx.recv() => {
                let Some(event) = event else { continue };
                match event.kind {
                    HostEventKind::CompilerStarted(pid) => eprintln!("EVENT compiler {pid}"),
                    HostEventKind::Compiled => eprintln!("EVENT compiled"),
                    HostEventKind::WorkflowStarted(pid) => eprintln!("EVENT workflow {pid}"),
                    HostEventKind::FirstCapability => eprintln!("EVENT first-capability"),
                    HostEventKind::DescendantPid(pid) => eprintln!("EVENT descendant {pid}"),
                    HostEventKind::OwnedTasksDrained => eprintln!("EVENT tasks-drained"),
                    HostEventKind::Finished => eprintln!("EVENT finished"),
                }
                let _ = std::io::stderr().flush();
            }
        }
    }.map_err(|error| format!(
        "run failed: {error}; source_hash={}; owned_tasks_after={}; process_reaped={:?}; descendants={:?}; run_dir={}",
        error.source_hash,
        error.owned_tasks_after,
        error.process_reaped,
        error.observed_descendant_pids,
        run_dir.display(),
    ))?;
    println!(
        "OK run_id={run_id} first_us={} final_us={} peak={} calls={} evidence={} source_hash={} workflow_peak_bytes={} workflow_user_ns={} workflow_system_ns={} host_peak_bytes={} run_dir={}",
        report.compile_to_first_capability.as_micros(),
        report.compile_to_final_evidence.as_micros(),
        report.peak_concurrent_calls,
        report.total_calls,
        hex(&report.evidence),
        report.source_hash,
        report.workflow_peak_bytes,
        report.workflow_user_time_ns,
        report.workflow_system_time_ns,
        report.host_peak_bytes,
        run_dir.display(),
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
