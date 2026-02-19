FROM rust:1.91-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY config/ config/

RUN cargo build --features telegram --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/bash cherub
USER cherub
WORKDIR /home/cherub

COPY --from=builder /build/target/release/cherub-telegram /usr/local/bin/
COPY --from=builder /build/config/ /home/cherub/config/

CMD ["cherub-telegram"]
