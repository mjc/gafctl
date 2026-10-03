#!/bin/sh
set -eu
app=${1:?Pass the Supervisor app slug}
container="app_$app"
api() {
    docker exec hassio_cli sh -c 'curl --fail --silent -H "Authorization: Bearer $SUPERVISOR_TOKEN" -H "Content-Type: application/json" -X POST "http://supervisor$1" -d "$2"' sh "$1" "$2"
}
json_field() {
    docker exec -i homeassistant python -c 'import json,sys; value=json.load(sys.stdin); [value := value[key] for key in sys.argv[1].split(".")]; print(value)' "$1"
}
ready() {
    docker exec "$container" curl --fail --silent --retry 10 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
}
ha apps start "$app"
ready
identity=$(docker exec "$container" sha256sum /data/identities.json)
ha apps restart "$app"
ready
test "$(docker exec "$container" sha256sum /data/identities.json)" = "$identity"
api "/addons/$app/options" '{"watchdog":true}'
docker kill --signal KILL "$container"
for _attempt in $(seq 1 30); do
    if docker exec "$container" curl --fail --silent http://127.0.0.1:8787/health 2>/dev/null; then break; fi
    sleep 5
done
ready
test "$(docker exec "$container" sha256sum /data/identities.json)" = "$identity"
docker exec "$container" sh -c 'umask 077; printf "synthetic backup fixture\n" > /data/install-check-secret'
backup=$(ha backups new --app "$app" --name gafctl-install-check --raw-json | json_field data.slug)
docker exec "$container" rm /data/install-check-secret
ha backups restore "$backup" --app "$app" --homeassistant=false
ready
test "$(docker exec "$container" cat /data/install-check-secret)" = 'synthetic backup fixture'
test "$(docker exec "$container" stat -c %a /data/install-check-secret)" = 600
test "$(docker exec "$container" sha256sum /data/identities.json)" = "$identity"
ha backups remove "$backup"
docker exec "$container" rm /data/install-check-secret
printf '\nSupervisor install, start, restart, watchdog recovery, cold backup/restore: passed\n'
