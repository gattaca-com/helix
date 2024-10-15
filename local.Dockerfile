FROM rust:1.72.0 AS helix

RUN apt update -y
RUN apt install -y clang
RUN apt install -y protobuf-compiler

RUN wget https://github.com/mozilla/sccache/releases/download/v0.3.1/sccache-v0.3.1-x86_64-unknown-linux-musl.tar.gz \
  && tar xzf sccache-v0.3.1-x86_64-unknown-linux-musl.tar.gz \
  && mv sccache-v0.3.1-x86_64-unknown-linux-musl/sccache /usr/local/bin/sccache \
  && chmod +x /usr/local/bin/sccache

# Copy necessary contents into the container at /app
ADD ./ /app/

RUN ls -lah /app

# Set the working directory to /app
WORKDIR /app/

RUN --mount=type=cache,target=/root/.cargo \
  --mount=type=cache,target=/usr/local/cargo/registry \
  cargo fetch

# Run build
RUN --mount=type=cache,target=/root/.cargo \
  --mount=type=cache,target=/usr/local/cargo/registry \
  RUSTC_WRAPPER=/usr/local/bin/sccache cargo build -p helix-cmd --release

# Copy binary into the workdir
RUN mv /app/target/release/helix-cmd /app/helix-cmd

# our final base
FROM debian:stable-slim

RUN mkdir /root/logs

RUN apt-get update
RUN apt-get install -y ca-certificates

WORKDIR /app

COPY --from=helix /app/helix-cmd* ./

# set the startup command to run your binary
ENTRYPOINT ["/app/helix-cmd"]
