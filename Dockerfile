FROM --platform=linux/amd64 rust:1.77.0 AS helix

RUN apt update -y
RUN apt install -y clang
RUN apt install -y protobuf-compiler

# Install sccache
RUN cargo install sccache

# Set environment variables for sccache
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/usr/local/sccache

# Copy necessary contents into the container at /app
COPY . /app/

# Set the working directory to /app
WORKDIR /app

# Fetch dependencies and build in a single RUN command
RUN --mount=type=cache,target=/root/.cargo \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/sccache \
    --mount=type=cache,target=/app/target \
    cargo fetch && \
    cargo build -p helix-cmd --release && \
    cp target/release/helix-cmd /app/helix-cmd


# our final base
FROM --platform=linux/amd64 debian:stable-slim

RUN mkdir /root/logs

RUN apt-get update -y
RUN apt-get install -y ca-certificates

WORKDIR /app

COPY --from=helix /app/helix-cmd* ./

# set the startup command to run your binary
ENTRYPOINT ["/app/helix-cmd"]