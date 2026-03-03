FROM rust:1.85-bookworm AS builder
WORKDIR /app
COPY bench/actix/ ./
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/bench-actix-gateway /usr/local/bin/gateway
COPY --from=builder /app/target/release/bench-actix-compute /usr/local/bin/compute
COPY --from=builder /app/target/release/bench-actix-store /usr/local/bin/store
CMD ["gateway"]
