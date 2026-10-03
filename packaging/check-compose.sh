#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
: "${DOCKER_HOST:?Set DOCKER_HOST to the disposable test runtime}"
work="$PWD/target/install-checks"
mkdir -p "$work"
cat > "$work/compose.yaml" <<'YAML'
services:
  gafctl:
    ports: !override ["127.0.0.1:0:8787"]
    env_file: !reset []
YAML
compose() {
    docker-compose -p gafctl-install-check -f compose.yaml -f "$work/compose.yaml" "$@"
}
ready() {
    compose exec -T gafctl curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
}
created_secret=false
cleanup() {
    if [ "$created_secret" = true ]; then
        compose run --rm --no-deps --entrypoint /bin/sh \
            -v "$GAFCTL_CHECK_SECRET_DIR:/fixtures" gafctl -c 'rm /fixtures/quickconnect-password'
    fi
    compose down --volumes >/dev/null
}
trap cleanup EXIT HUP INT TERM
compose up -d --no-build --pull never
endpoint=$(compose port gafctl 8787)
docker run --rm --network host --read-only --cap-drop ALL --entrypoint curl gafctl:local \
    --fail --silent --retry 5 --retry-connrefused --retry-delay 1 "http://$endpoint/health"
ready
identity=$(compose exec -T gafctl sha256sum /data/identities.json)
compose restart
ready
test "$(compose exec -T gafctl sha256sum /data/identities.json)" = "$identity"
compose stop --timeout 3
compose start
ready
test "$(compose exec -T gafctl sha256sum /data/identities.json)" = "$identity"
printf '\nCompose start, HTTP, restart, stop/start, persistent identity: passed\n'
if [ -n "${GAFCTL_CHECK_SECRET_DIR:-}" ]; then
    compose run --rm --no-deps --entrypoint /bin/sh \
        -v "$GAFCTL_CHECK_SECRET_DIR:/fixtures" gafctl -ec \
        'test ! -e /fixtures/quickconnect-password; umask 077; printf "synthetic secret\n" > /fixtures/quickconnect-password'
    created_secret=true
    cat > "$work/compose-secret.yaml" <<YAML
services:
  gafctl:
    volumes:
      - type: bind
        source: "$GAFCTL_CHECK_SECRET_DIR/quickconnect-password"
        target: /run/secrets/quickconnect-password
        read_only: true
        bind:
          create_host_path: false
YAML
    docker-compose -p gafctl-install-check -f compose.yaml -f compose.quickconnect.yaml \
        -f "$work/compose.yaml" -f "$work/compose-secret.yaml" run --rm --no-deps -T gafctl sh -es <<'CHECK'
test "$(cat "$GAFCTL_QUICKCONNECT_PASSWORD_FILE")" = "synthetic secret"
test "$(stat -c %a "$GAFCTL_QUICKCONNECT_PASSWORD_FILE")" = 600
CHECK
    printf 'QuickConnect overlay secret mount, ownership, private mode: passed\n'
fi
