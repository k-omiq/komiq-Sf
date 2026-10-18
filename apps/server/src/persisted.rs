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
    operation_name: Option<String>,
}

impl PersistedRequest {
    pub(crate) fn into_request(self) -> async_graphql::Request {
        let request = async_graphql::Request::new(self.document).variables(self.variables);
        match self.operation_name {
            Some(name) => request.operation_name(name),
            None => request,
        }
    }
}

/// Refusal codes. The apps read them to fall back: on `PERSISTED_QUERY_NOT_FOUND` (400)
/// they send the document text, on `PERSISTED_QUERY_REQUIRED` (405) they POST it.
pub(crate) const NOT_FOUND: &str = "PERSISTED_QUERY_NOT_FOUND";
pub(crate) const REQUIRED: &str = "PERSISTED_QUERY_REQUIRED";

#[derive(serde::Deserialize)]
struct GetParams {
    id: Option<String>,
    query: Option<String>,
    variables: Option<String>,
    #[serde(rename = "operationName")]
    operation_name: Option<String>,
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
            "Malformed query string",
        );
    };
    let Some(id) = params.id else {
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
    if params.query.is_some() {
        return refuse(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "Send either id or query, not both",
        );
    }
    let Some(document) = document(&id) else {
        return refuse(StatusCode::BAD_REQUEST, NOT_FOUND, "PersistedQueryNotFound");
    };
    let variables = match params.variables.as_deref().map(serde_json::from_str) {
        None => async_graphql::Variables::default(),
        Some(Ok(value)) => async_graphql::Variables::from_json(value),
        Some(Err(_)) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "variables is not JSON",
            )
        }
    };
    request.extensions_mut().insert(PersistedRequest {
        document,
        variables,
        operation_name: params.operation_name,
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
