# unloot — build once, then a small runtime that also carries ffmpeg.
#
#   docker build -t unloot .
#   docker run --rm -v "$PWD/input:/workdir" -e UNLOOT_LLM_URL=... unloot
#
# The image is only useful if you mount your photos in; see README.md.

FROM docker.io/library/rust:1.99-slim AS build
WORKDIR /src

# Manifest first: dependencies stay cached when only sources change.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && cargo build --release

COPY src src
COPY tests tests
RUN touch src/main.rs \
 && cargo build --release \
 && strip target/release/unloot

FROM docker.io/library/debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.title="unloot" \
      org.opencontainers.image.description="Sort and compress Vinted listing photos into one folder per item, with a CSV summary."

# ffmpeg is here for the container even though unloot itself links libjpeg
# directly; it makes the image usable for the odd video or batch job, and it is
# what the single-binary install assumes is already present.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/unloot /usr/local/bin/unloot

# The two paths a container almost always wants, overridable as usual.
ENV UNLOOT_INPUT_DIR=/workdir \
    UNLOOT_OUTPUT_DIR=/workdir \
    UNLOOT_LOG=info
RUN mkdir -p /workdir && chown 1000:1000 /workdir
USER 1000:1000
WORKDIR /workdir
VOLUME ["/workdir"]

ENTRYPOINT ["/usr/local/bin/unloot"]
CMD []
