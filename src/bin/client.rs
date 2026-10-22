//! Example client for unary, bidirectional, and session RPCs, fixture export, and health checks.

use std::{path::PathBuf, time::Duration};

use clap::{Parser, ValueEnum};
use grpc_optimizer::{
    pb::{
        JobOutcome, SessionRequest, SolveInput, SolveRequest, job_outcome,
        optimizer_client::OptimizerClient, session_request,
    },
    samples::{self, Family},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, transport::Endpoint};

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Unary,
    Bidi,
    /// Send the model once, then only per-job inputs.
    Session,
}

#[derive(Parser)]
#[command(about = "Solve sample or generated continuous LPs over gRPC")]
struct Args {
    #[arg(long, env = "OPT_ENDPOINT", default_value = "http://127.0.0.1:50051")]
    endpoint: String,
    #[arg(long, value_enum, default_value = "unary")]
    mode: Mode,
    #[arg(long, default_value = "production")]
    model: String,
    #[arg(long)]
    fixture: Option<PathBuf>,
    #[arg(long, value_enum)]
    generate: Option<Family>,
    #[arg(long, default_value_t = 100)]
    size: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long, default_value_t = 3)]
    jobs: usize,
    #[arg(long)]
    diagnostics: bool,
    #[arg(long, default_value_t = 30.0)]
    deadline: f64,
    /// Make the first job invalid to demonstrate per-job errors: UNSPECIFIED sense,
    /// or an out-of-range objective index in session mode.
    #[arg(long)]
    invalid_first: bool,
    /// Export a generated model as the shared fixture format, without connecting.
    #[arg(long)]
    export: Option<PathBuf>,
    #[arg(long)]
    health: bool,
}

fn timed<T>(value: T, seconds: f64) -> Request<T> {
    let mut request = Request::new(value);
    request.set_timeout(Duration::from_secs_f64(seconds));
    request
}

fn print_outcome(outcome: &JobOutcome) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(outcome)?);
    if let Some(job_outcome::Outcome::Error(error)) = &outcome.outcome {
        eprintln!(
            "job {} failed: gRPC code {}: {}",
            outcome.request_id, error.code, error.message
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.deadline.is_finite() && args.deadline > 0.0 && args.deadline <= 86400.0,
        "deadline must be in (0,86400] seconds"
    );
    anyhow::ensure!((1..=64).contains(&args.jobs), "jobs must be in 1..=64");
    if args.health {
        let channel = Endpoint::from_shared(args.endpoint)?
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await?;
        let response = tonic_health::pb::health_client::HealthClient::new(channel)
            .check(timed(
                tonic_health::pb::HealthCheckRequest {
                    service: "optimization.v1.Optimizer".into(),
                },
                2.0,
            ))
            .await?
            .into_inner();
        anyhow::ensure!(
            response.status
                == tonic_health::pb::health_check_response::ServingStatus::Serving as i32,
            "service is not serving"
        );
        println!("SERVING");
        return Ok(());
    }
    let sample = if let Some(family) = args.generate {
        samples::Sample {
            name: "generated".into(),
            description: "Seeded generated LP".into(),
            request: samples::generate(family, args.size, args.seed)?,
            expected_status: 1,
            expected_objective: None,
            expected_values: None,
        }
    } else if let Some(path) = &args.fixture {
        serde_json::from_str(&std::fs::read_to_string(path)?)?
    } else {
        samples::fixture(&args.model)?
    };
    if let Some(path) = &args.export {
        std::fs::write(path, serde_json::to_string_pretty(&sample)?)?;
        return Ok(());
    }
    let jobs: Vec<SolveRequest> = (0..args.jobs)
        .map(|i| {
            let mut job = sample.request.clone();
            job.request_id = format!("{}-{i}", sample.name);
            job.diagnostics = args.diagnostics;
            if args.invalid_first && i == 0 {
                job.model.as_mut().expect("fixture model").sense = 0;
            }
            job
        })
        .collect();
    let channel = Endpoint::from_shared(args.endpoint)?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await?;
    let mut client = OptimizerClient::new(channel)
        .max_decoding_message_size(16 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024);
    match args.mode {
        Mode::Unary => {
            for job in jobs {
                let result = client.solve(timed(job, args.deadline)).await?.into_inner();
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Mode::Bidi => {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let producer = tokio::spawn(async move {
                for job in jobs {
                    if tx.send(job).await.is_err() {
                        break;
                    }
                }
                // Dropping tx half-closes input. The response is drained below.
            });
            let mut stream = client
                .solve_stream(timed(ReceiverStream::new(rx), args.deadline))
                .await?
                .into_inner();
            while let Some(outcome) = stream.message().await? {
                print_outcome(&outcome)?;
            }
            producer.await?;
        }
        Mode::Session => {
            let model = sample.request.model.clone();
            let first = SessionRequest {
                kind: model.map(session_request::Kind::Model),
            };
            let inputs = jobs.into_iter().enumerate().map(|(i, job)| {
                let mut input = SolveInput {
                    request_id: job.request_id,
                    diagnostics: job.diagnostics,
                    time_limit_seconds: job.time_limit_seconds,
                    ..SolveInput::default()
                };
                if args.invalid_first && i == 0 {
                    input.objective_indices.push(u32::MAX);
                    input.objective_values.push(0.0);
                }
                SessionRequest {
                    kind: Some(session_request::Kind::Input(input)),
                }
            });
            let messages: Vec<_> = std::iter::once(first).chain(inputs).collect();
            let mut stream = client
                .solve_session(timed(tokio_stream::iter(messages), args.deadline))
                .await?
                .into_inner();
            while let Some(outcome) = stream.message().await? {
                print_outcome(&outcome)?;
            }
        }
    }
    Ok(())
}
