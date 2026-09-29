# One image, all binaries: gateway, store, stats, loadgen, ctl.
FROM docker.io/library/rust:1-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && mkdir /out \
 && cp target/release/gateway target/release/store target/release/stats \
       target/release/loadgen target/release/ctl /out/

FROM docker.io/library/debian:trixie-slim
COPY --from=build /out/ /usr/local/bin/
ENV CONTROL_SOCKET=/run/ctl.sock
