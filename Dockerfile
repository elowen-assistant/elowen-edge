FROM rust:1.88-bookworm AS build
WORKDIR /app/elowen-edge

COPY elowen-edge/Cargo.toml Cargo.toml
COPY elowen-edge/Cargo.lock Cargo.lock
COPY elowen-edge/src src
COPY elowen-platform/contracts/rust/elowen-contracts ../elowen-platform/contracts/rust/elowen-contracts

RUN cargo build --release

FROM rust:1.88-bookworm
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/elowen-edge/target/release/elowen-edge /usr/local/bin/elowen-edge

CMD ["elowen-edge"]
