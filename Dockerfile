# Build stage: compile a static musl binary.
FROM rust:1.98-alpine AS build

# ring needs a C compiler and the musl headers.
RUN apk add --no-cache musl-dev

WORKDIR /src

# Build the dependencies first, so they stay cached while only src changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Touch main.rs so cargo rebuilds it instead of reusing the stub's output.
RUN touch src/main.rs && cargo build --release --locked

# Runtime stage: just the binary on plain Alpine. TLS uses rustls with
# bundled root certificates, so ca-certificates is not needed.
FROM alpine:3.22

COPY --from=build /src/target/release/hetzner-fw-updater /usr/local/bin/hetzner-fw-updater
COPY --chmod=755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# Mount the directory holding the config file here. Mount the directory, not
# the file: download saves through a temporary file that it renames.
WORKDIR /config
USER nobody

# With arguments, runs the tool with them; without, runs check every
# CHECK_INTERVAL seconds.
ENV CHECK_INTERVAL=900
ENTRYPOINT ["docker-entrypoint.sh"]
