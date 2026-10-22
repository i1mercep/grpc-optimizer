//! Compares direct and gRPC execution, including failed jobs in benchmark accounting.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use clap::{Parser, ValueEnum};
use futures::{StreamExt, stream};
use grpc_optimizer::{
    config::Config,
    engine::Engine,
    pb::{
        ExecutionSettings, JobOutcome, SolveRequest, SolveResult, SolveStatus, job_outcome,
        optimizer_client::OptimizerClient,
    },
    samples::{self, Family},
};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{
    Request,
    transport::{Channel, Endpoint},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Direct,
    Unary,
    Bidi,
    All,
}

#[derive(Parser)]
#[command(about = "Compare direct and gRPC solving; emits JSON, not a latency guarantee")]
struct Args {
    #[arg(long, env = "OPT_ENDPOINT", default_value = "http://127.0.0.1:50051")]
    endpoint: String,
    #[arg(long, value_enum, default_value = "all")]
    mode: Mode,
    #[arg(long, value_enum, default_value = "production")]
    family: Family,
    #[arg(long, default_value_t = 100)]
    size: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long, default_value_t = 100)]
    jobs: usize,
    #[arg(long, default_value_t = 4)]
    warmup: usize,
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    #[arg(long, default_value_t = 30)]
    deadline_seconds: u64,
}

/// Client-observed latency plus optional server timings for one submitted job.
#[derive(Serialize)]
struct Measurement {
    latency_ms: f64,
    success: bool,
    validation_ms: Option<f64>,
    build_ms: Option<f64>,
    solve_ms: Option<f64>,
    execution: Option<ExecutionSettings>,
    error: Option<String>,
}

impl Measurement {
    /// Records elapsed time; only an optimal result containing a solution counts as success.
    fn result(start: Instant, result: Result<SolveResult, String>) -> Self {
        let mut measurement = Self {
            latency_ms: start.elapsed().as_secs_f64() * 1000.0,
            success: false,
            validation_ms: None,
            build_ms: None,
            solve_ms: None,
            execution: None,
            error: None,
        };
        match result {
            Ok(result) => {
                measurement.success =
                    result.status == SolveStatus::Optimal as i32 && result.solution.is_some();
                if !measurement.success {
                    measurement.error = Some(format!("solver status {}", result.solver_status));
                }
                measurement.execution = result.execution;
                if let Some(timing) = result.timings {
                    measurement.validation_ms = Some(timing.validation_seconds * 1000.0);
                    measurement.build_ms = Some(timing.build_seconds * 1000.0);
                    measurement.solve_ms = Some(timing.solve_seconds * 1000.0);
                }
            }
            Err(error) => measurement.error = Some(error),
        }
        measurement
    }
}

fn unpack(outcome: JobOutcome) -> Result<SolveResult, String> {
    match outcome.outcome {
        Some(job_outcome::Outcome::Result(result)) => Ok(result),
        Some(job_outcome::Outcome::Error(error)) => {
            Err(format!("gRPC {}: {}", error.code, error.message))
        }
        None => Err("empty job outcome".into()),
    }
}

fn timed<T>(value: T, seconds: u64) -> Request<T> {
    let mut request = Request::new(value);
    request.set_timeout(Duration::from_secs(seconds));
    request
}

/// Measures one mode, retaining a measurement for every input even if an RPC fails.
///
/// Direct/unary latency starts at job execution; bidi latency starts after a window
/// slot is acquired. `All` must be expanded by the caller, and every remote mode
/// requires `client` to be present.
async fn run(
    mode: Mode,
    input: Vec<SolveRequest>,
    engine: &Engine,
    client: Option<&OptimizerClient<Channel>>,
    args: &Args,
) -> Vec<Measurement> {
    match mode {
        Mode::Direct | Mode::Unary => {
            stream::iter(input)
                .map(|job| {
                    let engine = engine.clone();
                    let mut client = client.cloned();
                    async move {
                        let start = Instant::now();
                        let result = if matches!(mode, Mode::Direct) {
                            engine
                                .solve(
                                    job,
                                    Some(start + Duration::from_secs(args.deadline_seconds)),
                                    CancellationToken::new(),
                                )
                                .await
                                .map_err(|e| e.to_string())
                        } else {
                            client
                                .as_mut()
                                .expect("remote client")
                                .solve(timed(job, args.deadline_seconds))
                                .await
                                .map(|r| r.into_inner())
                                .map_err(|e| e.to_string())
                        };
                        Measurement::result(start, result)
                    }
                })
                .buffer_unordered(args.concurrency)
                .collect()
                .await
        }
        Mode::Bidi => {
            // Correlation retains each slot until its response arrives, bounding in-flight jobs.
            type Pending = Arc<Mutex<HashMap<String, (Instant, OwnedSemaphorePermit)>>>;
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let producer_pending = pending.clone();
            let count = input.len();
            let slots = Arc::new(Semaphore::new(args.concurrency));
            let (tx, rx) = tokio::sync::mpsc::channel(args.concurrency);
            let mut client = client.expect("remote client").clone();
            let start = Instant::now();
            let producer = tokio::spawn(async move {
                for job in input {
                    let permit = slots.clone().acquire_owned().await.expect("open slots");
                    producer_pending
                        .lock()
                        .unwrap()
                        .insert(job.request_id.clone(), (Instant::now(), permit));
                    if tx.send(job).await.is_err() {
                        break;
                    }
                }
            });
            let mut measurements = Vec::with_capacity(count);
            let mut error = "stream ended before all results arrived".to_owned();
            // Deadline covers the entire persistent stream, not each message.
            match client
                .solve_stream(timed(ReceiverStream::new(rx), args.deadline_seconds))
                .await
            {
                Ok(response) => {
                    let mut output = response.into_inner();
                    loop {
                        match output.message().await {
                            Ok(Some(outcome)) => {
                                let submitted = pending.lock().unwrap().remove(&outcome.request_id);
                                if let Some((submitted, _permit)) = submitted {
                                    measurements
                                        .push(Measurement::result(submitted, unpack(outcome)));
                                } else {
                                    error = "unexpected response ID".into();
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                error = e.to_string();
                                break;
                            }
                        }
                    }
                }
                Err(e) => error = e.to_string(),
            }
            producer.abort();
            let _ = producer.await;
            // Include unsent/unfinished jobs as failures, never hide lost work.
            for _ in measurements.len()..count {
                measurements.push(Measurement::result(start, Err(error.clone())));
            }
            measurements
        }
        Mode::All => unreachable!(),
    }
}

/// Reports nearest-rank p50/p95/p99 values, or JSON null for an empty sample.
fn distribution(values: impl Iterator<Item = f64>) -> serde_json::Value {
    let mut values: Vec<_> = values.collect();
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return serde_json::Value::Null;
    }
    let percentile = |p: f64| {
        values[((values.len() as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(values.len() - 1)]
    };
    serde_json::json!({"p50": percentile(0.50), "p95": percentile(0.95), "p99": percentile(0.99)})
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.jobs > 0
            && args.concurrency > 0
            && args.concurrency <= 64
            && args.deadline_seconds > 0
            && args.deadline_seconds <= 86400,
        "jobs must be positive, concurrency 1..64, deadline 1..86400 seconds"
    );
    let config = Config::from_env()?;
    let engine = Engine::new(config)?;
    let base = samples::generate(args.family, args.size, args.seed)?;
    let model = base.model.as_ref().unwrap();
    let dimensions = serde_json::json!({"variables": model.variables.len(), "constraints": model.constraints.len(), "nonzeros": model.constraints.iter().map(|r| r.indices.len()).sum::<usize>()});
    let connection_start = Instant::now();
    let client = if matches!(args.mode, Mode::Direct) {
        None
    } else {
        Some(
            OptimizerClient::new(
                Endpoint::from_shared(args.endpoint.clone())?
                    .connect_timeout(Duration::from_secs(5))
                    .connect()
                    .await?,
            )
            .max_decoding_message_size(16 * 1024 * 1024)
            .max_encoding_message_size(16 * 1024 * 1024),
        )
    };
    let connection_setup_seconds = if client.is_some() {
        connection_start.elapsed().as_secs_f64()
    } else {
        0.0
    };
    let modes = if matches!(args.mode, Mode::All) {
        vec![Mode::Direct, Mode::Unary, Mode::Bidi]
    } else {
        vec![args.mode]
    };
    let make_jobs = |count: usize| {
        (0..count)
            .map(|i| {
                let mut job = base.clone();
                job.request_id = format!("bench-{i}");
                job
            })
            .collect::<Vec<_>>()
    };
    for mode in modes {
        let warmup_start = Instant::now();
        let warmup = run(
            mode,
            make_jobs(args.warmup),
            &engine,
            client.as_ref(),
            &args,
        )
        .await;
        let warmup_seconds = warmup_start.elapsed().as_secs_f64();
        let input = make_jobs(args.jobs);
        let start = Instant::now();
        let measurements = run(mode, input, &engine, client.as_ref(), &args).await;
        let elapsed = start.elapsed().as_secs_f64();
        let successes = measurements.iter().filter(|m| m.success).count();
        println!(
            "{}",
            serde_json::json!({
                "mode": format!("{mode:?}"), "family": format!("{:?}", args.family), "size": args.size, "seed": args.seed,
                "dimensions": dimensions, "jobs": args.jobs, "concurrency": args.concurrency,
                "client_available_cpus": std::thread::available_parallelism().map_or(1, usize::from),
                "execution_settings": measurements.iter().find_map(|m| m.execution.as_ref()),
                "connection_setup_seconds": connection_setup_seconds, "warmup_jobs": args.warmup, "warmup_seconds": warmup_seconds,
                "warmup_failures": warmup.iter().filter(|m| !m.success).count(), "measured_seconds": elapsed,
                "successes": successes, "failures": args.jobs - successes,
                "completed_jobs_per_second": args.jobs as f64 / elapsed, "successful_jobs_per_second": successes as f64 / elapsed,
                "latency_ms": distribution(measurements.iter().map(|m| m.latency_ms)),
                "validation_ms": distribution(measurements.iter().filter_map(|m| m.validation_ms)),
                "build_ms": distribution(measurements.iter().filter_map(|m| m.build_ms)),
                "solve_ms": distribution(measurements.iter().filter_map(|m| m.solve_ms)),
                "errors": measurements.iter().filter_map(|m| m.error.as_ref()).take(5).collect::<Vec<_>>()
            })
        );
    }
    engine.shutdown();
    engine.drain().await;
    Ok(())
}
