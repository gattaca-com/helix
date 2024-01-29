FROM rust:1.75.0 as helix

RUN apt update -y
RUN apt install -y clang
RUN apt install -y protobuf-compiler

RUN wget https://github.com/mozilla/sccache/releases/download/v0.3.1/sccache-v0.3.1-x86_64-unknown-linux-musl.tar.gz \
    && tar xzf sccache-v0.3.1-x86_64-unknown-linux-musl.tar.gz \
    && mv sccache-v0.3.1-x86_64-unknown-linux-musl/sccache /usr/local/bin/sccache \
    && chmod +x /usr/local/bin/sccache

ARG AWS_ACCESS_KEY_ID
ARG AWS_SECRET_ACCESS_KEY
ARG REPO_NAME

RUN echo "REPO_NAME: $REPO_NAME"

# Test to make sure that aws access is set correctly
RUN test -n "$AWS_ACCESS_KEY_ID" || (echo "AWS_ACCESS_KEY_ID  not set" && false)
RUN test -n "$AWS_SECRET_ACCESS_KEY" || (echo "AWS_SECRET_ACCESS_KEY  not set" && false)

ENV SCCACHE_BUCKET=sccache-gtc
ENV SCCACHE_REGION=eu-west-1
ENV SCCACHE_S3_USE_SSL=true

# Copy necessary contents into the container at /app
ADD ./repos /app/

RUN ls -lah /app
RUN ls -lah /app/${REPO_NAME}

# Set the working directory to /app
WORKDIR /app/${REPO_NAME}

RUN --mount=type=cache,target=/root/.cargo \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo fetch

# Run build
RUN --mount=type=cache,target=/root/.cargo \
    --mount=type=cache,target=/usr/local/cargo/registry \
    RUSTC_WRAPPER=/usr/local/bin/sccache cargo build -p helix-cmd --release

# Copy binary into the workdir
RUN mv /app/$REPO_NAME/target/release/helix-cmd /app/helix-cmd

# our final base
FROM debian:bullseye-slim

RUN mkdir /root/logs

RUN apt-get update
RUN apt-get install -y ca-certificates

WORKDIR /app

COPY --from=helix /app/helix-cmd* ./

# set the startup command to run your binary
ENTRYPOINT ["/app/helix-cmd"]

