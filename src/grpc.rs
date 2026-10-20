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
    model::Error,
    pb::{
        BatchRequest, BatchResponse, JobError, JobOutcome, SolveRequest, SolveResult, job_outcome,
        optimizer_server::{Optimizer, OptimizerServer},
    },
};

#[derive(Clone)]
pub struct Service {
    engine: Engine,
}

impl Service {
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }

    pub fn into_server(self) -> OptimizerServer<Self> {
        let limit = self.engine.config.max_message_bytes;
        OptimizerServer::new(self)
            .max_decoding_message_size(limit)
            .max_encoding_message_size(limit)
    }

    fn start_stream(
        &self,
        input: BoxStream<'static, Result<SolveRequest, Status>>,
        deadline: Option<Instant>,
        permit: OwnedSemaphorePermit,
        finite: bool,
    ) -> ResponseStream {
        let engine = self.engine.clone();
        let window = engine
            .config
            .stream_window
            .min(engine.config.max_concurrent_solves);
        let batch_limit = engine.config.max_batch_jobs;
        let slots = Arc::new(Semaphore::new(window));
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let shutdown = engine.shutdown_token();
        let (tx, rx) = mpsc::channel(window);
        let terminal = Arc::new(Mutex::new(None));
        let worker_terminal = terminal.clone();

        tokio::spawn(async move {
            // A permit follows each job through execution and the output channel. It
            // is released only when the consumer polls the result (or disconnects).
            let jobs = stream::unfold(
                (input, slots, 0usize),
                move |(mut input, slots, index): (
                    BoxStream<'static, Result<SolveRequest, Status>>,
                    Arc<Semaphore>,
                    usize,
                )| async move {
                    let permit = slots.clone().acquire_owned().await.ok()?;
                    let item = input.next().await?;
                    let item = item.and_then(|job| {
                        if finite && index >= batch_limit {
                            Err(Status::resource_exhausted(
                                "client stream exceeds batch job limit",
                            ))
                        } else {
                            Ok((index, job, permit))
                        }
                    });
                    Some((item, (input, slots, index.saturating_add(1))))
                },
            )
            .boxed();
            let outcomes = jobs.map(|item| {
                let engine = engine.clone();
                let cancel = worker_cancel.clone();
                async move {
                    let (index, job, permit) = item?;
                    let request_id = job.request_id.clone();
                    let outcome = match engine.solve(job, deadline, cancel).await.and_then(|result| {
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
                    Ok(Delivery { index, outcome: JobOutcome { request_id, outcome: Some(outcome) }, _permit: permit })
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
            worker_cancel.cancel();
        });
        ResponseStream {
            rx,
            _rpc_permit: permit,
            _cancel: cancel.drop_guard(),
            deadline,
            terminal,
        }
    }
}

struct Delivery {
    index: usize,
    outcome: JobOutcome,
    _permit: OwnedSemaphorePermit,
}

pub struct ResponseStream {
    rx: mpsc::Receiver<Result<Delivery, Status>>,
    _rpc_permit: OwnedSemaphorePermit,
    _cancel: DropGuard,
    deadline: Option<Instant>,
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
    type SolveBatchStream = ResponseStream;
    type SolveStreamStream = ResponseStream;

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

    async fn solve_batch(
        &self,
        request: Request<BatchRequest>,
    ) -> Result<Response<Self::SolveBatchStream>, Status> {
        let permit = self.engine.admit_rpc().map_err(to_status)?;
        let deadline = deadline(&request)?;
        if request.get_ref().jobs.len() > self.engine.config.max_batch_jobs {
            return Err(Status::resource_exhausted("batch exceeds job limit"));
        }
        let input = stream::iter(request.into_inner().jobs.into_iter().map(Ok)).boxed();
        Ok(Response::new(
            self.start_stream(input, deadline, permit, true),
        ))
    }

    async fn collect(
        &self,
        request: Request<tonic::Streaming<SolveRequest>>,
    ) -> Result<Response<BatchResponse>, Status> {
        let permit = self.engine.admit_rpc().map_err(to_status)?;
        let deadline = deadline(&request)?;
        let mut stream = self.start_stream(request.into_inner().boxed(), deadline, permit, true);
        let mut ordered = Vec::new();
        let mut bytes = 0usize;
        while let Some(item) = stream.rx.recv().await {
            let delivery = item?;
            let size = delivery.outcome.encoded_len();
            bytes =
                bytes.saturating_add(1 + prost::encoding::encoded_len_varint(size as u64) + size);
            if bytes
                > self
                    .engine
                    .config
                    .max_collect_bytes
                    .min(self.engine.config.max_message_bytes)
            {
                return Err(Status::resource_exhausted(
                    "collected response exceeds byte limit",
                ));
            }
            ordered.push((delivery.index, delivery.outcome));
        }
        if let Some(error) = stream.terminal.lock().unwrap().take() {
            return Err(error);
        }
        if stream.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Status::deadline_exceeded("RPC deadline exceeded"));
        }
        if self.engine.shutdown_token().is_cancelled() {
            return Err(Status::unavailable("server is shutting down"));
        }
        ordered.sort_unstable_by_key(|(index, _)| *index);
        Ok(Response::new(BatchResponse {
            outcomes: ordered.into_iter().map(|(_, outcome)| outcome).collect(),
        }))
    }

    async fn solve_stream(
        &self,
        request: Request<tonic::Streaming<SolveRequest>>,
    ) -> Result<Response<Self::SolveStreamStream>, Status> {
        let permit = self.engine.admit_rpc().map_err(to_status)?;
        let deadline = deadline(&request)?;
        Ok(Response::new(self.start_stream(
            request.into_inner().boxed(),
            deadline,
            permit,
            false,
        )))
    }
}

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

pub async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d.into()).await,
        None => std::future::pending().await,
    }
}

// Tonic enforces this metadata at the transport layer too; retaining an absolute
// deadline here lets each later job in a stream receive only the remaining budget.
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
            Ok(job)
        }))
        .inspect(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .boxed();
        let mut output = service.start_stream(input, None, engine.admit_rpc().unwrap(), false);
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
