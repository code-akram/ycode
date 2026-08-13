#![forbid(unsafe_code)]

use ycode_native_sdk::{Error, Evidence, Outcome, Request, run};

fn main() {
    run(|context| {
        let discovered = (0_u8..12)
            .filter(|item| item % 5 != 4)
            .map(|item| format!("resource-{item}"))
            .collect::<Vec<_>>();
        let mut tasks = Vec::new();
        for (index, resource) in discovered.iter().enumerate() {
            let attempt = if index == 3 { 1 } else { 0 };
            let request = match index % 3 {
                0 => Request::Fetch {
                    query: resource.clone(),
                    attempt,
                },
                1 => Request::Inspect {
                    resource: resource.clone(),
                    attempt,
                },
                _ => Request::Summarize {
                    content: resource.clone(),
                    attempt,
                },
            };
            tasks.push(context.spawn(request)?);
        }
        let mut aggregate = 0xcbf29ce484222325_u64;
        for task in tasks {
            let outcome = context.join(task)?;
            let value = match outcome {
                Outcome::Success(value) => value,
                Outcome::Retry {
                    reason,
                    next_attempt,
                } => match context.call(Request::Retry {
                    prior: reason,
                    attempt: next_attempt,
                })? {
                    Outcome::Success(value) => value,
                    Outcome::Retry { .. } => {
                        return Err(Error::Host("retry budget exhausted".into()));
                    }
                    Outcome::Failure(message) => return Err(Error::Host(message)),
                },
                Outcome::Failure(message) => return Err(Error::Host(message)),
            };
            for byte in value {
                    aggregate = (aggregate ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
        let remaining = context.budget()?;
        let cancelled = context.cancelled()?;
        context.finish(Evidence(
            format!(
                "native-evidence:v1:items={}:remaining={remaining}:cancelled={cancelled}:aggregate={aggregate:016x}",
                discovered.len()
            )
            .into_bytes(),
        ))
    })
    .expect("native workflow failed");
}
