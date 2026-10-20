FROM rust:1.99-slim-trixie AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake g++ libclang-dev make protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
COPY examples ./examples
RUN CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --locked --release \
    --bin grpc-optimizer-server --bin grpc-optimizer-client

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/grpc-optimizer-server /build/target/release/grpc-optimizer-client ./
USER 65532:65532
ENV OPT_LISTEN_ADDR=0.0.0.0:50051
EXPOSE 50051
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/app/grpc-optimizer-client", "--health", "--endpoint", "http://127.0.0.1:50051"]
ENTRYPOINT ["/app/grpc-optimizer-server"]
