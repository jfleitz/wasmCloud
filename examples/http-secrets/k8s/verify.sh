#!/usr/bin/env bash
# Deploys http-secrets.yaml and checks that every key in the Secret comes back
# through the component, byte for byte — the evidence that `secretFrom` reached
# the host and the `wasmcloud-secrets` plugin served it to the right component.
#
#   ./verify.sh                # namespace `default`
#   NAMESPACE=wasmcloud ./verify.sh
#   IMAGE=registry.internal/http-secrets:dev ./verify.sh   # a build of your own
#
# Needs kubectl pointed at a cluster running the runtime-operator with a host
# group named `default`. Requests are made from a short-lived curl pod inside
# the cluster, over the Service the operator points at the host pods — the
# same path an in-cluster caller takes. Leaves the workload deployed; remove
# it with `kubectl delete -f http-secrets.yaml`.
set -euo pipefail

ns=${NAMESPACE:-default}
cd "$(dirname "$0")"

# `IMAGE` swaps the published component for one you pushed yourself.
sed "s|image: ghcr.io/wasmcloud/components/http-secrets:.*|image: ${IMAGE:-ghcr.io/wasmcloud/components/http-secrets:0.1.0}|" http-secrets.yaml \
  | kubectl -n "$ns" apply -f -

# The host routes by hostname, so every request carries the `host` the
# manifest's `wasi:http` entry declares, whatever address it arrives on.
host=$(sed -n 's/^ *host: *//p' http-secrets.yaml | head -1)

probe=http-secrets-verify
kubectl -n "$ns" delete pod "$probe" --ignore-not-found --wait >/dev/null
kubectl -n "$ns" run "$probe" --image=curlimages/curl:8.14.1 --restart=Never --command -- sleep 600 >/dev/null
trap 'kubectl -n "$ns" delete pod "$probe" --ignore-not-found --wait=false >/dev/null' EXIT
kubectl -n "$ns" wait --for=condition=Ready "pod/$probe" --timeout=120s >/dev/null
get() { local path=$1; shift; kubectl -n "$ns" exec "$probe" -- curl -s -H "Host: $host" "$@" "http://http-secrets$path"; }

echo "waiting for the workload to answer..."
for _ in $(seq 1 60); do
  if [[ "$(get / -o /dev/null -w '%{http_code}')" == "404" ]]; then break; fi
  sleep 2
done
kubectl -n "$ns" get workloaddeployment http-secrets

failed=0
check() {
  local what=$1 want=$2 got=$3
  if [[ "$got" == "$want" ]]; then
    echo "ok    $what"
  else
    echo "FAIL  $what: want '$want', got '$got'"
    failed=1
  fi
}

# Every key of the Secret, decoded, against what `store.get` + `reveal` return.
for key in $(kubectl -n "$ns" get secret http-secrets -o go-template='{{range $k, $_ := .data}}{{$k}} {{end}}'); do
  want=$(kubectl -n "$ns" get secret http-secrets -o go-template="{{index .data \"$key\" | base64decode}}")
  check "store.get($key)" "$want" "$(get "/secrets/$key")"
done

# The labeled import resolves the same `api-key` entry, without naming it.
want=$(kubectl -n "$ns" get secret http-secrets -o go-template='{{index .data "api-key" | base64decode}}')
check "api-key (labeled import)" "$want" "$(get /api-key)"

# A key the Secret does not carry is `not-found`, never a stale or shared value.
check "store.get(no-such-key) -> 404" "404" "$(get /secrets/no-such-key -o /dev/null -w '%{http_code}')"

exit $failed
