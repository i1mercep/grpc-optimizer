//! gRPC handlers with bounded streaming, per-job errors, and RPC-wide deadlines.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures::{
    Stream, StreamExt,
    stream::{self, BoxStream},
};
use prost::Message;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::{Code, Request, Response, Status};

use crate::{
    engine::Engine,
    model::{self, Error},
    pb::{
        JobError, JobOutcome, LinearProgram, SessionRequest, SolveRequest, SolveResult,
        job_outcome,
        optimizer_server::{Optimizer, OptimizerServer},
        session_request,
    },
};

/// A stream job whose request may have failed before reaching the engine.
struct Job {
    request_id: String,
    request: model::Result<SolveRequest>,
}

impl From<SolveRequest> for Job {
    fn from(request: SolveRequest) -> Self {
        Self {
            request_id: request.request_id.clone(),
            request: Ok(request),
        }
    }
}

/// Validates the leading session model once, then expands each input into a job.
///
/// Protocol violations and an invalid model terminate the stream; bad overrides
/// become per-job errors.
fn session_jobs(
    input: BoxStream<'static, Result<SessionRequest, Status>>,
    engine: Engine,
) -> BoxStream<'static, Result<Job, Status>> {
    let mut base: Option<LinearProgram> = None;
    input
        .filter_map(move |item| {
            let job = item.and_then(|message| match message.kind {
                None => Err(Status::invalid_argument("session message is empty")),
                Some(session_request::Kind::Model(_)) if base.is_some() => Err(
                    Status::invalid_argument("session model is already set; open a new session"),
                ),
                Some(session_request::Kind::Model(model)) => {
                    model::validate_model(&model, &engine.config).map_err(to_status)?;
                    base = Some(model);
                    Ok(None)
                }
                Some(session_request::Kind::Input(input)) => {
                    let base = base.as_ref().ok_or_else(|| {
                        Status::invalid_argument("first session message must contain the model")
                    })?;
                    Ok(Some(Job {
                        request_id: input.request_id.clone(),
                        request: model::apply(base, input),
                    }))
                }
            });
            futures::future::ready(job.transpose())
        })
        .boxed()
}

/// Serves unary and bidirectional RPCs using one engine's shared resource limits.
#[derive(Clone)]
pub struct Service {
    engine: Engine,
}

impl Service {
    /// Wraps an engine, sharing its admission limits with existing clones.
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    /// Builds the Tonic service with configured message limits in both directions.
    pub fn into_server(self) -> OptimizerServer<Self> {
        let limit = self.engine.config.max_message_bytes;
        OptimizerServer::new(self)
            .max_decoding_message_size(limit)
            .max_encoding_message_size(limit)
    }

    /// Runs jobs in completion order with a window covering active and unread results.
    ///
    /// Job failures become outcomes; input, shutdown, and deadline failures terminate
    /// the stream.
    fn start_stream(
        &self,
        input: BoxStream<'static, Result<Job, Status>>,
        deadline: Option<Instant>,
        permit: OwnedSemaphorePermit,
    ) -> ResponseStream {
        let engine = self.engine.clone();
        let window = engine
            .config
            .stream_window
            .min(engine.config.max_concurrent_solves);
        let slots = Arc::new(Semaphore::new(window));
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let shutdown = engine.shutdown_token();
        let (tx, rx) = mpsc::channel(window);
        let terminal = Arc::new(Mutex::new(None));
        let worker_terminal = terminal.clone();

        tokio::spawn(async move {
            let _cancel_on_exit = worker_cancel.clone().drop_guard();
            // A permit follows each job through execution and the output channel. It
            // is released only when the consumer polls the result (or disconnects).
            let jobs = stream::unfold(
                (input, slots),
                move |(mut input, slots): (
                    BoxStream<'static, Result<Job, Status>>,
                    Arc<Semaphore>,
                )| async move {
                    let permit = slots.clone().acquire_owned().await.ok()?;
                    let item = input.next().await?.map(|job| (job, permit));
                    Some((item, (input, slots)))
                },
            )
            .boxed();
            let outcomes = jobs.map(|item| {
                let engine = engine.clone();
                let cancel = worker_cancel.clone();
                async move {
                    let (job, permit) = item?;
                    let request_id = job.request_id;
                    let solved = match job.request {
                        Ok(request) => engine.solve(request, deadline, cancel).await,
                        Err(error) => Err(error),
                    };
                    let outcome = match solved.and_then(|result| {
                        if result.encoded_len().saturating_add(request_id.len() + 32) > engine.config.max_message_bytes {
                            Err(Error::Exhausted("solution exceeds message limit".into()))
                        } else { Ok(result) }
                    }) {
                        Ok(result) => job_outcome::Outcome::Result(result),
                        Err(error) => {
                            let status = to_status(error);
                            tracing::warn!(%request_id, code = ?status.code(), message = status.message(), "optimization rejected");
                            job_outcome::Outcome::Error(JobError { code: status.code() as i32, message: status.message().to_owned() })
                        }
                    };
                    Ok(Delivery { outcome: JobOutcome { request_id, outcome: Some(outcome) }, _permit: permit })
                }
            }).buffer_unordered(window);
            tokio::pin!(outcomes);
            loop {
                let next = tokio::select! {
                    biased;
                    _ = worker_cancel.cancelled() => break,
                    _ = shutdown.cancelled() => { *worker_terminal.lock().unwrap() = Some(Status::unavailable("server is shutting down")); break; },
                    _ = wait_deadline(deadline) => { *worker_terminal.lock().unwrap() = Some(Status::deadline_exceeded("RPC deadline exceeded")); break; },
                    item = outcomes.next() => item,
                };
                let Some(item) = next else {
                    break;
                };
                let terminal = item.is_err();
                tokio::select! {
                    biased;
                    _ = worker_cancel.cancelled() => break,
                    _ = shutdown.cancelled() => { *worker_terminal.lock().unwrap() = Some(Status::unavailable("server is shutting down")); break; },
                    _ = wait_deadline(deadline) => { *worker_terminal.lock().unwrap() = Some(Status::deadline_exceeded("RPC deadline exceeded")); break; },
                    sent = tx.send(item) => { if sent.is_err() || terminal { break; } }
                }
            }
        });
        ResponseStream {
            rx,
            _rpc_permit: permit,
            _cancel: cancel.drop_guard(),
            terminal,
        }
    }
}

/// Retains a job's window slot until its outcome is consumed.
struct Delivery {
    outcome: JobOutcome,
    _permit: OwnedSemaphorePermit,
}

/// Holds RPC admission until dropped and cancels unfinished jobs on drop.
/// Queued outcomes are delivered before a stored terminal status.
pub struct ResponseStream {
    rx: mpsc::Receiver<Result<Delivery, Status>>,
    _rpc_permit: OwnedSemaphorePermit,
    _cancel: DropGuard,
    terminal: Arc<Mutex<Option<Status>>>,
}

impl Stream for ResponseStream {
    type Item = Result<JobOutcome, Status>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(None) => Poll::Ready(this.terminal.lock().unwrap().take().map(Err)),
            other => other.map(|value| value.map(|v| v.map(|d| d.outcome))),
        }
    }
}

#[tonic::async_trait]
impl Optimizer for Service {
    type SolveStreamStream = ResponseStream;
    type SolveSessionStream = ResponseStream;

    /// Returns a single result or a transport status for request/execution failure.
    async fn solve(&self, request: Request<SolveRequest>) -> Result<Response<SolveResult>, Status> {
        let _permit = self.engine.admit_rpc().map_err(to_status)?;
        let deadline = deadline(&request)?;
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        tokio::select! {
            biased;
            _ = wait_deadline(deadline) => Err(Status::deadline_exceeded("RPC deadline exceeded")),
            result = self.engine.solve(request.into_inner(), deadline, cancel) => {
                let result = result.map_err(to_status)?;
                if result.encoded_len() > self.engine.config.max_message_bytes {
                    return Err(Status::resource_exhausted("solution exceeds message limit"));
                }
                Ok(Response::new(result))
            }
        }
    }

    /// Processes a persistent input stream with backpressure and one shared deadline.
    async fn solve_stream(
        &self,
        request: Request<tonic::Streaming<SolveRequest>>,
    ) -> Result<Response<Self::SolveStreamStream>, Status> {
        let permit = self.engine.admit_rpc().map_err(to_status)?;
        let deadline = deadline(&request)?;
        Ok(Response::new(self.start_stream(
            request.into_inner().map(|item| item.map(Job::from)).boxed(),
            deadline,
            permit,
        )))
    }

    /// Streams jobs against one model sent at the start of the call.
    async fn solve_session(
        &self,
        request: Request<tonic::Streaming<SessionRequest>>,
    ) -> Result<Response<Self::SolveSessionStream>, Status> {
        let permit = self.engine.admit_rpc().map_err(to_status)?;
        let deadline = deadline(&request)?;
        Ok(Response::new(self.start_stream(
            session_jobs(request.into_inner().boxed(), self.engine.clone()),
            deadline,
            permit,
        )))
    }
}

/// Maps service failures to transport codes, also used inside per-job error outcomes.
pub fn to_status(error: Error) -> Status {
    let code = match &error {
        Error::Invalid(_) => Code::InvalidArgument,
        Error::Exhausted(_) => Code::ResourceExhausted,
        Error::Deadline => Code::DeadlineExceeded,
        Error::Cancelled => Code::Cancelled,
        Error::Unavailable => Code::Unavailable,
        Error::Internal(_) => Code::Internal,
    };
    Status::new(code, error.to_string())
}

/// Waits until the absolute deadline, or remains pending when no deadline was supplied.
pub async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d.into()).await,
        None => std::future::pending().await,
    }
}

/// Parses `grpc-timeout` into one absolute deadline shared by every job in the RPC.
///
/// Missing metadata means no deadline; malformed or overflowing values are rejected.
/// Tonic also enforces the timeout at the transport layer.
pub fn deadline<T>(request: &Request<T>) -> Result<Option<Instant>, Status> {
    let Some(raw) = request.metadata().get("grpc-timeout") else {
        return Ok(None);
    };
    let value = raw
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    let invalid = || Status::invalid_argument("invalid grpc-timeout");
    if !(2..=9).contains(&value.len()) {
        return Err(invalid());
    }
    let (digits, unit) = value.split_at(value.len() - 1);
    if !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    let amount: u64 = digits.parse().map_err(|_| invalid())?;
    let duration = match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        "n" => Duration::from_nanos(amount),
        _ => return Err(invalid()),
    };
    Ok(Some(
        Instant::now().checked_add(duration).ok_or_else(invalid)?,
    ))
}

#[cfg(test)]
mod cancellation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, samples};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn output_backpressure_holds_the_entire_job_window() {
        let engine = Engine::new(Config {
            cpu_budget: 2,
            max_concurrent_solves: 2,
            stream_window: 2,
            ..Config::default()
        })
        .unwrap();
        let service = Service::new(engine.clone());
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        let input = stream::iter((0..10).map(|i| {
            let mut job = samples::fixture("production").unwrap().request;
            job.request_id = i.to_string();
            Ok(Job::from(job))
        }))
        .inspect(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .boxed();
        let mut output = service.start_stream(input, None, engine.admit_rpc().unwrap());
        tokio::time::timeout(Duration::from_secs(3), async {
            while reads.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "unconsumed outputs must retain window permits"
        );
        output.next().await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while reads.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(output);
        engine.shutdown();
        engine.drain().await;
    }

    #[tokio::test]
    async fn handler_rejects_expired_deadline_without_solving() {
        let service = Service::new(Engine::new(Config::default()).unwrap());
        let mut request = Request::new(samples::fixture("production").unwrap().request);
        request.set_timeout(Duration::ZERO);
        assert_eq!(
            Optimizer::solve(&service, request)
                .await
                .unwrap_err()
                .code(),
            Code::DeadlineExceeded
        );
    }
}
