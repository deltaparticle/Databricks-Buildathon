# Shopping Sessions: hybrid of Team 67 express fast path + our engine fallback

# Stage 1: build our shopping-sessions engine
FROM rust:1.91-slim-bookworm AS build-ours
WORKDIR /src
ENV CARGO_TERM_COLOR=never \
    RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
ARG TARGET=x86_64-unknown-linux-gnu

# Dependencies first, so source edits rebuild only the crates themselves.
COPY shopping-sessions/Cargo.toml shopping-sessions/Cargo.lock ./
COPY shopping-sessions/generic/Cargo.toml shopping-sessions/generic/Cargo.lock ./generic/
RUN mkdir -p src generic/src && echo 'fn main() {}' > src/main.rs && cp src/main.rs generic/src/main.rs \
 && cargo build --release --locked --target $TARGET \
 && cargo build --release --locked --target $TARGET --manifest-path generic/Cargo.toml \
 && rm -rf src generic/src target/$TARGET/release/.fingerprint/shopping-sessions-* \
           generic/target/$TARGET/release/.fingerprint/shopping-sessions-*

COPY shopping-sessions/src ./src
COPY shopping-sessions/generic/src ./generic/src
RUN cargo build --release --locked --target $TARGET \
 && cargo build --release --locked --target $TARGET --manifest-path generic/Cargo.toml \
 && cp target/$TARGET/release/shopping-sessions /shopping-sessions \
 && cp generic/target/$TARGET/release/shopping-sessions-generic /shopping-sessions-generic

# Stage 2: build express
FROM rust:1-alpine AS build-express
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY express/Cargo.toml express/Cargo.lock ./
COPY express/.cargo .cargo
COPY express/src src
RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/pipeline /pipeline

# Stage 3: final scratch image
FROM scratch
COPY --from=build-express /pipeline /pipeline
COPY --from=build-ours /shopping-sessions /shopping-sessions
COPY --from=build-ours /shopping-sessions-generic /shopping-sessions-generic
USER 1000:1000
ENTRYPOINT ["/pipeline"]
