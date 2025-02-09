# Build stage

FROM rust:1.74.0 AS builder
WORKDIR /root/workdir

COPY Cargo.toml Cargo.lock ./
RUN \
    mkdir /root/workdir/src && \
    echo 'fn main() {}' > /root/workdir/src/main.rs && \
    cargo build --release && \
    rm -Rvf /root/workdir/src && \
    rm -Rvf /root/workdir/target/release/deps/gcal_pull_view*

COPY src ./src
RUN cargo build --release


# Bundle stage

FROM debian:bookworm-20231218-slim
COPY --from=builder /root/workdir/target/release/gcal_pull_view /usr/bin/gcal_pull_view

CMD ["/usr/bin/gcal_pull_view"]
