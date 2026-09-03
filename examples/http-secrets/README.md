# http-secrets

An HTTP component that reads secrets through `wasmcloud:secrets` and serves
them back, so a deployment can confirm that what it bound — a Kubernetes
`Secret` named by `secretFrom`, or a `wash dev` config entry — is exactly what
the component receives. [`k8s/verify.sh`](./k8s/verify.sh) deploys it to a
cluster and checks every key round-trips, byte for byte.

It is a diagnostic: it reveals its secrets to whoever can reach it. Run it
where that is acceptable and take it down afterwards.

| Path                 | Resolves through             | Response                                |
| -------------------- | ---------------------------- | --------------------------------------- |
| `GET /secrets/<key>` | `store.get(key)` + `reveal`  | 200 the value, 404 not bound, 502 error |
| `GET /api-key`       | the labeled `api-key` import | 200 the value                           |

## What it demonstrates

Both ways a component can ask for a secret, from one
[`wit/world.wit`](./wit/world.wit):

```wit
import wasmcloud:secrets/store@2.1.0;
import wasmcloud:secrets/reveal@2.1.0;
import api-key: wasmcloud:secrets/secret@2.1.0;
```

- **`store` + `reveal`** is dynamic, keyed by a runtime string. A key the bind
  did not carry is only discovered when `store.get` returns `not-found`. `get`
  hands back an opaque `secret` handle; `reveal` is a separate interface so a
  host can audit or gate reveals apart from lookups.
- **`api-key`** is a labeled import of `secret`: the label *is* the secret's
  name, so there is no `key` argument. Because the label is part of the
  component's own type, the host checks it resolves against the bind's config
  before instantiating the component — a missing `api-key` entry refuses the
  bind, naming the key, instead of surfacing as a runtime `not-found`.

Neither path touches a file or an environment variable. The platform resolves
`secretFrom` before the workload deploys and hands the host the decoded pairs
as bind-time config; the host's built-in `wasmcloud-secrets` plugin serves
them to the component that was bound to them, and to no other component in
the workload. A value leaves the host only through `reveal`.

## Run it locally

```shell
wash dev
```

[`.wash/config.yaml`](./.wash/config.yaml) supplies inline what a cluster
supplies from a `Secret`: an unlabeled `wasmcloud:secrets` entry whose keys
answer `store.get`, and a `name: api-key` entry the labeled import routes to.

```shell
curl localhost:8000/secrets/password   # dev-password
curl localhost:8000/api-key            # dev-api-key
curl -i localhost:8000/secrets/nope    # 404: no secret bound at "nope"
```

Delete the `api-key` key from the named entry and `wash dev` refuses to start
the workload — that is the labeled import's bind-time check.

## Build

```shell
wash build
```

Every `wasmcloud:secrets` function is an `async func`, so this is a **WASI P3**
component: it exports `wasi:http/handler@0.3.0` and awaits its imports from
inside the request. `wash build` runs the `cargo build --target wasm32-wasip2`
from `.wash/config.yaml`; the linker emits the P3 component directly.

## Deploy to Kubernetes

[`k8s/http-secrets.yaml`](./k8s/http-secrets.yaml) is three objects:

1. A `Secret` with the values — `username`, `password`, and `api-key`.
2. A selectorless `Service`. The operator writes its `EndpointSlice`, pointing
   at the host pods running this workload, and registers the Service's DNS
   names with the host's HTTP router.
3. The `WorkloadDeployment`, whose `hostInterfaces` bind the Secret to the
   component's two secrets imports:

```yaml
hostInterfaces:
  - namespace: wasmcloud
    package: secrets
    version: "2.1.0"
    interfaces: [store, reveal]
    secretFrom:
      - name: http-secrets
  - namespace: wasmcloud
    package: secrets
    version: "2.1.0"
    interfaces: [secret]
    name: api-key                # routes the labeled `api-key` import here
    secretFrom:
      - name: http-secrets
```

`secretFrom` merges like `envFrom`: every key of every named Secret, later
Secrets winning on a conflict, base64- and UTF-8-decoded. The plain entry
makes each key of the Secret gettable by name; the named entry serves the one
key that matches its label.

The manifest references `ghcr.io/wasmcloud/components/http-secrets`, which
the `examples` workflow publishes from `main`. To deploy a local build
instead, push it somewhere the cluster can pull from and point `image` at it:

```shell
wash oci push ghcr.io/<you>/http-secrets:0.1.0 target/wasm32-wasip2/release/http_secrets.wasm
```

### Verify

```shell
./k8s/verify.sh                                   # namespace `default`
NAMESPACE=wasmcloud ./k8s/verify.sh
IMAGE=ghcr.io/<you>/http-secrets:0.1.0 ./k8s/verify.sh   # a build of your own
```

The script applies the manifest, starts a short-lived curl pod, waits for the
workload to answer over the Service, then compares what `GET /secrets/<key>`
returns for every key in the Secret against the Secret's own decoded data —
plus the labeled import and a key the Secret does not carry:

```
ok    store.get(api-key)
ok    store.get(password)
ok    store.get(username)
ok    api-key (labeled import)
ok    store.get(no-such-key) -> 404
```

A non-zero exit means a value did not round-trip. It leaves the workload
running; remove it with `kubectl delete -f k8s/http-secrets.yaml`.

Requests are made from inside the cluster because the Service is
selectorless: the operator writes its EndpointSlice, so `kubectl port-forward
svc/…`, which resolves pods through a selector, has nothing to connect to.
Each request carries `Host: http-secrets.example.com`, the `config.host` of
the manifest's `wasi:http` entry — the host routes by hostname, not by the
address a request arrived on. An ingress in front of the Service forwards
that name; the operator also registers the Service's DNS aliases for
in-cluster callers.

### Things to try

- **Remove `api-key` from the Secret** and re-apply. The workload never
  becomes Ready: the host refuses the bind naming the missing key, before the
  component is instantiated. Removing `password` instead deploys fine, and
  `GET /secrets/password` is a 404 — the difference between the two import
  styles.
- **Deploy it twice** with two Secrets that share a key but differ in value.
  Each workload answers with its own — the plugin keys bind-time config by
  the calling workload and component, never one global map.
