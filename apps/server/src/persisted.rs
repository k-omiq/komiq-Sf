//! Persisted GraphQL documents: `GET /graphql?id=<sha256>&variables=<json>`.
//!
//! The build (`packages/api/scripts/persisted-operations.ts`) hashes every document in
//! `packages/api/src/operations.ts` into `persisted-operations.json`, compiled in here: the
//! key is the SHA-256 (hex) of the document's normalised text, the value that text. The
//! apps compute the same id for the document they are about to send.
//!
//! A GET that names an id runs the stored document; an unknown id is refused with 400
//! before anything is admitted or run. The URL stays short, and the cache key space of the
//! cacheable endpoint is the fixed set of documents. With `KOMIKA_PERSISTED_ONLY=on` a GET
//! can run nothing else: a GET with the document text is refused too, so the cacheable
//! endpoint cannot be used for arbitrary queries. That refusal is a 405, on which apps of
//! every version send the document by POST. POST keeps taking full documents, for older
//! apps, the admin console and documents the server does not know.
//!
//! The edge keys on the whole query string, so a GET by id must arrive in its one
//! canonical form, `id=<id>[&variables=<compact JSON>][&nsfw=<pass>]`, encoded the way
//! the apps encode it. Unknown parameters are refused, and any other spelling of a valid
//! request is redirected to the canonical URL before it runs. Otherwise every extra
//! parameter or re-spaced JSON would be a separate cache entry that runs the query.
//! Parameter order is the exception: the edge ignores it and forwards the parameters
//! sorted, so any order is accepted ([`same_parameters`]).
use std::collections::HashMap;
use std::sync::LazyLock;

use axum::extract::{Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// `{ id: document }` as the build wrote it.
static DOCUMENTS: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../persisted-operations.json"))
        .expect("persisted-operations.json is a JSON object of strings")
});

/// The stored document for `id`, if the build knows it.
pub(crate) fn document(id: &str) -> Option<&'static str> {
    DOCUMENTS.get(id).map(String::as_str)
}

/// `KOMIKA_PERSISTED_ONLY`: a GraphQL GET must name a persisted document.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PersistedOnly(pub bool);

/// The request a GET's `id` resolved to. `AdmittedGraphQLRequest` takes it instead of
/// parsing the query string.
#[derive(Clone)]
pub(crate) struct PersistedRequest {
    document: &'static str,
    variables: async_graphql::Variables,
    /// The shared NSFW pass (`nsfw_pass.rs`), checked where the NSFW preference is read.
    nsfw: Option<String>,
}

impl PersistedRequest {
    pub(crate) fn into_request(self) -> async_graphql::Request {
        let request = async_graphql::Request::new(self.document).variables(self.variables);
        match self.nsfw {
            Some(pass) => request.data(crate::graphql::NsfwPass(pass)),
            None => request,
        }
    }
}

/// The `nsfw` pass a GET by id carried, for the handler to check before anything runs.
#[derive(Clone)]
pub(crate) struct NsfwPassParam(pub(crate) String);

/// Refusal codes. The apps read them to fall back: on `PERSISTED_QUERY_NOT_FOUND` (400)
/// they send the document text, on `PERSISTED_QUERY_REQUIRED` (405) they POST it.
pub(crate) const NOT_FOUND: &str = "PERSISTED_QUERY_NOT_FOUND";
pub(crate) const REQUIRED: &str = "PERSISTED_QUERY_REQUIRED";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GetParams {
    id: Option<String>,
    query: Option<String>,
    variables: Option<String>,
    #[serde(rename = "operationName")]
    operation_name: Option<String>,
    nsfw: Option<String>,
}

/// `encodeURIComponent` as the apps apply it, after `new URL(...).href` has also encoded
/// `'`: every byte except `A-Z a-z 0-9 - _ . ! ~ * ( )` as uppercase `%XX`. The WHATWG
/// URL parser leaves this form unchanged, so a redirect to it cannot loop.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The canonical query string of a GET by id.
fn canonical_query(id: &str, variables: Option<&serde_json::Value>, nsfw: Option<&str>) -> String {
    let mut query = format!("id={}", encode_component(id));
    if let Some(variables) = variables {
        query.push_str("&variables=");
        query.push_str(&encode_component(&variables.to_string()));
    }
    if let Some(pass) = nsfw {
        query.push_str("&nsfw=");
        query.push_str(&encode_component(pass));
    }
    query
}

/// Whether `raw` holds the parameters of `canonical`, spelled the same, in any order.
/// The edge cache key ignores parameter order (cache rule R4), and Cloudflare forwards
/// the query string sorted, so `id=..&variables=..&nsfw=..` reaches this server as
/// `id=..&nsfw=..&variables=..`. Redirecting on order would send every such request
/// round in a loop; order mints no extra edge entry, so it is not checked.
fn same_parameters(raw: &str, canonical: &str) -> bool {
    let mut raw: Vec<&str> = raw.split('&').collect();
    let mut canonical: Vec<&str> = canonical.split('&').collect();
    raw.sort_unstable();
    canonical.sort_unstable();
    raw == canonical
}

/// A pass is the hex of an HMAC-SHA256 tag.
fn pass_shape_ok(pass: &str) -> bool {
    pass.len() == 64 && pass.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        // Never kept by a cache: a later deploy may know the id.
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "errors": [{ "message": message, "extensions": { "code": code } }]
        })),
    )
        .into_response()
}

/// Resolve `?id=` on a GraphQL GET (or HEAD) into a [`PersistedRequest`]. Other methods
/// pass through, and so does a GET with the document text unless persisted-only refuses it.
pub(crate) async fn resolve_get(
    State(PersistedOnly(only)): State<PersistedOnly>,
    mut request: Request,
    next: Next,
) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }
    let Ok(Query(params)) = Query::<GetParams>::try_from_uri(request.uri()) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "Malformed query string, or a parameter this endpoint does not take",
        );
    };
    let Some(id) = params.id else {
        if params.nsfw.is_some() {
            return refuse(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "nsfw needs a persisted document id",
            );
        }
        if only {
            // 405, the answer on which apps that predate persisted documents already fall
            // back to POST.
            let mut response = refuse(
                StatusCode::METHOD_NOT_ALLOWED,
                REQUIRED,
                "This server runs only persisted documents over GET; send the document by POST",
            );
            response
                .headers_mut()
                .insert(header::ALLOW, header::HeaderValue::from_static("POST"));
            return response;
        }
        return next.run(request).await;
    };
    if params.query.is_some() || params.operation_name.is_some() {
        return refuse(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "With id, send only variables and nsfw",
        );
    }
    if params
        .nsfw
        .as_deref()
        .is_some_and(|pass| !pass_shape_ok(pass))
    {
        return refuse(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "Malformed nsfw pass",
        );
    }
    let Some(document) = document(&id) else {
        return refuse(StatusCode::BAD_REQUEST, NOT_FOUND, "PersistedQueryNotFound");
    };
    let json = match params
        .variables
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
    {
        None => None,
        Some(Ok(value)) => Some(value),
        Some(Err(_)) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "variables is not JSON",
            )
        }
    };
    let canonical = canonical_query(&id, json.as_ref(), params.nsfw.as_deref());
    if !same_parameters(request.uri().query().unwrap_or(""), &canonical) {
        // A valid request in another spelling: send it to the one URL the edge keeps,
        // before anything runs. Never cached, so a later canonical form can change it.
        let location = format!("{}?{canonical}", request.uri().path());
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [
                (header::LOCATION, location),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
        )
            .into_response();
    }
    let variables = json
        .map(async_graphql::Variables::from_json)
        .unwrap_or_default();
    if let Some(pass) = &params.nsfw {
        request.extensions_mut().insert(NsfwPassParam(pass.clone()));
    }
    request.extensions_mut().insert(PersistedRequest {
        document,
        variables,
        nsfw: params.nsfw,
    });
    next.run(request).await
}

/// Every stored document, for tests elsewhere.
#[cfg(test)]
pub(crate) fn documents() -> impl Iterator<Item = &'static str> {
    DOCUMENTS.values().map(String::as_str)
}

/// The id of the one stored document that starts with `prefix`, for tests elsewhere.
#[cfg(test)]
pub(crate) fn id_for(prefix: &str) -> String {
    let mut found = DOCUMENTS.iter().filter(|(_, doc)| doc.starts_with(prefix));
    let (id, _) = found
        .next()
        .unwrap_or_else(|| panic!("no document {prefix}"));
    assert!(found.next().is_none(), "more than one document {prefix}");
    id.clone()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn every_id_is_the_hash_of_its_document_and_every_document_parses() {
        assert!(DOCUMENTS.len() > 100, "the build wrote {}", DOCUMENTS.len());
        for (id, doc) in DOCUMENTS.iter() {
            assert_eq!(*id, hex::encode(Sha256::digest(doc.as_bytes())), "{doc}");
            async_graphql::parser::parse_query(doc)
                .unwrap_or_else(|e| panic!("{id} does not parse: {e}"));
        }
        assert!(document("0".repeat(64).as_str()).is_none());
    }

    /// The canonical form is what the apps send: `encodeURIComponent` of compact JSON in
    /// key order, with `'` encoded by the URL parser.
    #[test]
    fn the_canonical_query_matches_what_the_apps_send() {
        let variables: serde_json::Value =
            serde_json::from_str(r#"{ "q": "it's (a) ~test~!*", "page": 1, "b": null }"#).unwrap();
        assert_eq!(
            canonical_query("abc", Some(&variables), None),
            "id=abc&variables=%7B%22q%22%3A%22it%27s%20(a)%20~test~!*%22%2C%22page%22%3A1%2C%22b%22%3Anull%7D"
        );
        assert_eq!(
            canonical_query("abc", None, Some("ff")),
            "id=abc&nsfw=ff",
            "nsfw comes last"
        );
        assert_eq!(encode_component("ü/é"), "%C3%BC%2F%C3%A9");
        assert!(same_parameters(
            "id=a&nsfw=p&variables=v",
            "id=a&variables=v&nsfw=p"
        ));
        assert!(!same_parameters("id=a&variables=v&x=1", "id=a&variables=v"));
        assert!(!same_parameters("id=a&variables=%7b", "id=a&variables=%7B"));
        assert!(pass_shape_ok(&"0a".repeat(32)));
        assert!(!pass_shape_ok(&"0A".repeat(32)));
        assert!(!pass_shape_ok("0a"));
    }

    /// The apps tell a server that predates persisted documents by this answer to an
    /// `id`-only GET: it parses the missing `query` as an empty document. Keep the words
    /// the reader looks for (`executable_definition`) in sight of the dependency.
    #[test]
    fn an_empty_document_fails_to_parse_with_the_words_older_servers_send() {
        let error = async_graphql::parser::parse_query("").unwrap_err();
        assert!(
            error.to_string().contains("executable_definition"),
            "{error}"
        );
    }
}
