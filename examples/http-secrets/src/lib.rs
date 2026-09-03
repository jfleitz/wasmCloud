//! Reads secrets through `wasmcloud:secrets` and serves them over HTTP.
//!
//! | Path                 | Resolves through                   | Response                          |
//! | -------------------- | ---------------------------------- | --------------------------------- |
//! | `GET /secrets/<key>` | `store.get(key)` + `reveal`        | 200 value, 404 not bound, 502 err |
//! | `GET /api-key`       | the labeled `api-key` import       | 200 value                         |
//! | anything else        |                                    | 404 usage                         |
//!
//! It is a diagnostic: it reveals its secrets to whoever can reach it. Run it
//! where that is acceptable and take it down afterwards.

mod bindings {
    wit_bindgen::generate!({
        world: "http-secrets",
        path: "wit",
        generate_all,
    });
}

use bindings::exports::wasi::http::handler::Guest;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};
use bindings::wasmcloud::secrets::reveal::reveal;
use bindings::wasmcloud::secrets::store::{self, Secret, SecretValue, SecretsError};

const USAGE: &str = "GET /secrets/<key>  the value bound at <key>, via store.get + reveal\n\
                     GET /api-key        the value behind the labeled `api-key` import\n";

struct Component;

impl Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path_and_query = request.get_path_with_query().unwrap_or_default();
        let path = path_and_query
            .split_once('?')
            .map_or(path_and_query.as_str(), |(path, _)| path)
            .trim_matches('/');

        let (status, body) = match path.split_once('/') {
            Some(("secrets", key)) if !key.is_empty() => lookup(key).await,
            None if path == "api-key" => {
                (200, reveal_string(&bindings::api_key::get().await).await)
            }
            _ => (404, USAGE.to_string()),
        };
        Ok(respond(status, body))
    }
}

/// `store.get` is dynamic: the key is only checked against what the bind
/// carried when it's asked for, so an unbound one is a runtime `not-found`.
async fn lookup(key: &str) -> (u16, String) {
    match store::get(key.to_string()).await {
        Ok(secret) => (200, reveal_string(&secret).await),
        Err(SecretsError::NotFound) => (404, format!("no secret bound at {key:?}\n")),
        Err(SecretsError::Upstream(detail) | SecretsError::Io(detail)) => {
            (502, format!("secrets backend: {detail}\n"))
        }
    }
}

/// A `secret` is an opaque handle until `reveal` unwraps it; that split is
/// what lets a host audit or gate reveals separately from lookups.
async fn reveal_string(secret: &Secret) -> String {
    match reveal(secret).await {
        SecretValue::String(value) => value,
        SecretValue::Bytes(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn respond(status: u16, body: String) -> Response {
    let headers = Fields::new();
    let _ = headers.append("content-type", b"text/plain; charset=utf-8");
    let (mut tx, rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| Ok(None));
    wit_bindgen::spawn_local(async move {
        if !body.is_empty() {
            tx.write_all(body.into_bytes()).await;
        }
        drop(tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });
    let (response, _result) = Response::new(headers, Some(rx), trailers_rx);
    let _ = response.set_status_code(status);
    response
}

bindings::export!(Component with_types_in bindings);
