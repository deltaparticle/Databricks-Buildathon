# Shopping Sessions: one statically linked Rust binary in an empty image.
# The pipeline in lane/ is Team 67's MIT-licensed Express Lane
# (github.com/jason-scheffel/express-lane, commit 3493fdd), used unchanged;
# see lane/LICENSE and README. The scorer builds this natively for
# linux/amd64 and runs CMD as uid 1000 with a read-only root, no network,
# /data read-only, /output writable.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY lane/Cargo.toml lane/Cargo.lock ./
COPY lane/.cargo .cargo
COPY lane/src src
RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/pipeline /pipeline

FROM scratch
COPY --from=build /pipeline /pipeline
ENV DATA_DIR=/data OUTPUT_DIR=/output NCPU=8
USER 1000:1000
CMD ["/pipeline"]
