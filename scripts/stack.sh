#!/usr/bin/env bash
# Runs the demo on a podman network. PROFILE=dev|prod selects env/<profile>.env.
#
#   scripts/stack.sh build              build the app image
#   scripts/stack.sh up                 start everything
#   scripts/stack.sh infra              start only valkey/jaeger/collector/prometheus (for cargo run)
#   scripts/stack.sh load [args]        run the load generator (args go to loadgen, e.g. -n 1000 -c 16)
#   scripts/stack.sh ctl <svc> <cmd>    talk to a service's control socket, e.g. ctl store log debug
#   scripts/stack.sh logs <svc>         follow a container's logs
#   scripts/stack.sh down               remove everything
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=${PROFILE:-dev}
ENV_FILE=env/$PROFILE.env
NET=otel-demo
IMAGE=localhost/otel-demo:latest
APPS=(store stats gateway)
INFRA=(valkey jaeger otel-collector prometheus)

[[ -f $ENV_FILE ]] || { echo "no $ENV_FILE" >&2; exit 1; }

run() { podman run -d --replace --network "$NET" --name "$@" >/dev/null; }

infra() {
  podman network exists "$NET" || podman network create "$NET" >/dev/null
  run valkey -p 6379:6379 docker.io/valkey/valkey:8
  run jaeger -p 16686:16686 docker.io/jaegertracing/jaeger:2.21.0
  run otel-collector -p 4317:4317 -p 4318:4318 -p 8889:8889 \
    --env-file "$ENV_FILE" \
    -v "$PWD/deploy/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro,Z" \
    docker.io/otel/opentelemetry-collector-contrib:0.161.0
  run prometheus -p 9090:9090 \
    -v "$PWD/deploy/prometheus.yaml:/etc/prometheus/prometheus.yml:ro,Z" \
    docker.io/prom/prometheus:latest
}

app() {
  local name=$1 port=$2; shift 2
  run "$name" -p "$port:$port" --env-file "$ENV_FILE" \
    -e SERVICE_NAME="$name" -e LISTEN_ADDR="0.0.0.0:$port" \
    -e VALKEY_URL=redis://valkey:6379 "$@" "$IMAGE" "$name"
}

case ${1:-} in
  build) podman build -t "$IMAGE" -f Containerfile . ;;
  infra) infra ;;
  up)
    infra
    app store 8081
    app stats 8082
    app gateway 8080 -e STORE_URL=http://store:8081 -e STATS_URL=http://stats:8082
    echo "profile=$PROFILE  gateway http://localhost:8080  jaeger http://localhost:16686  prometheus http://localhost:9090"
    ;;
  load)
    shift
    podman run --rm --network "$NET" --env-file "$ENV_FILE" \
      -e SERVICE_NAME=loadgen -e LOG_FORMAT=line -e LOG_FILTER=warn -e CONTROL_SOCKET=off \
      -e TARGET_URL=http://gateway:8080 "$IMAGE" loadgen "$@"
    ;;
  ctl) shift; svc=$1; shift; podman exec "$svc" ctl "$@" ;;
  logs) podman logs -f "$2" ;;
  down)
    podman rm -f "${APPS[@]}" "${INFRA[@]}" >/dev/null 2>&1 || true
    podman network rm "$NET" >/dev/null 2>&1 || true
    ;;
  *) sed -n '2,11p' "$0"; exit 1 ;;
esac
