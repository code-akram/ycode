#![forbid(unsafe_code)]

use ycode_native_sdk::{Error, Evidence, Outcome, Request, run};

fn main() {
    run(|context| {
        let discovered = (0_u8..12)
            .filter(|item| item % 5 != 4)
            .map(|item| format!("workspace/item-{item}.txt"))
            .collect::<Vec<_>>();
        let mut tasks = Vec::new();
        for (index, path) in discovered.iter().enumerate() {
            let request = match index % 4 {
                0 => Request::Shell {
                    command: format!("inspect:{path}"),
                    workdir: Some("workspace".into()),
                    timeout_ms: 2_000,
                },
                1 => Request::ApplyPatch {
                    patch: format!("*** Begin Patch\n*** Update File: {path}\n@@\n-old\n+new\n*** End Patch"),
                },
                _ => Request::Shell {
                    command: format!("summarize:{path}"),
                    workdir: None,
                    timeout_ms: 1_000,
                },
            };
            tasks.push(context.spawn(request)?);
        }
        let mut aggregate = 0xcbf29ce484222325_u64;
        let mut provenance = Vec::new();
        let mut partial_failures = Vec::new();
        for task in tasks {
            let outcome = context.join(task)?;
            let (call_id, value) = match outcome {
                Outcome::Success { call_id, output } => (call_id, output),
                Outcome::Retry { call_id, reason } => {
                    partial_failures.push(format!("retry:{call_id}:{reason}"));
                    match context.call(Request::Shell {
                        command: format!("retry:{reason}"),
                        workdir: None,
                        timeout_ms: 1_000,
                    })? {
                        Outcome::Success { call_id, output } => (call_id, output),
                        Outcome::Retry { .. } => {
                            return Err(Error::Host("retry budget exhausted".into()));
                        }
                        Outcome::Failure { message, .. } => return Err(Error::Host(message)),
                    }
                }
                Outcome::Failure { message, .. } => return Err(Error::Host(message)),
            };
            provenance.push(call_id);
            for byte in value {
                aggregate = (aggregate ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
        let remaining = context.budget()?;
        let cancelled = context.cancelled()?;
        context.finish(Evidence {
            version: 1,
            summary: format!(
                "items={}:remaining={remaining}:cancelled={cancelled}:aggregate={aggregate:016x}",
                discovered.len()
            ),
            verified: vec!["runtime discovery completed".into(), "typed aggregation completed".into()],
            disputed: Vec::new(),
            unresolved: Vec::new(),
            artifact_refs: vec![],
            partial_failures,
            provenance_ids: provenance,
        })
    })
    .expect("native workflow failed");
}
