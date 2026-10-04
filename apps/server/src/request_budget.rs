//! Admission and deadlines for GraphQL and uploads. Image and health routes never
//! enter these lanes: they have independent resource limits (or must always answer).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::Arc,
    time::Duration,
};

use async_graphql::parser::types::{
    DocumentOperations, ExecutableDocument, Selection, SelectionSet,
};
use async_graphql_axum::GraphQLRequest;
use axum::{
    extract::{FromRef, FromRequest, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Class {
    Local,
    Upstream,
    Federated,
    Maintenance,
}

impl Class {
    fn timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Local => 20,
            Self::Upstream => 300,
            Self::Federated => 1800,
            Self::Maintenance => 3600,
        })
    }
}

/// Separate slots keep a slow engine or maintenance operation from occupying
/// every ordinary catalogue/auth request. Admission never creates a waiting queue.
pub(crate) struct RequestBudgets {
    ingress: Arc<Semaphore>,
    local: Arc<Semaphore>,
    upstream: Arc<Semaphore>,
    federated: Arc<Semaphore>,
    maintenance: Arc<Semaphore>,
    uploads: Arc<Semaphore>,
}

impl Default for RequestBudgets {
    fn default() -> Self {
        Self {
            ingress: Arc::new(Semaphore::new(64)),
            local: Arc::new(Semaphore::new(64)),
            upstream: Arc::new(Semaphore::new(16)),
            federated: Arc::new(Semaphore::new(2)),
            maintenance: Arc::new(Semaphore::new(2)),
            uploads: Arc::new(Semaphore::new(4)),
        }
    }
}

impl RequestBudgets {
    fn lane(&self, class: Class) -> &Arc<Semaphore> {
        match class {
            Class::Local => &self.local,
            Class::Upstream => &self.upstream,
            Class::Federated => &self.federated,
            Class::Maintenance => &self.maintenance,
        }
    }

    #[cfg(test)]
    pub(crate) async fn exhaust_for_test(&self) -> Vec<OwnedSemaphorePermit> {
        let mut held = Vec::new();
        for lane in [
            &self.ingress,
            &self.local,
            &self.upstream,
            &self.federated,
            &self.maintenance,
            &self.uploads,
        ] {
            held.push(
                lane.clone()
                    .acquire_many_owned(lane.available_permits() as u32)
                    .await
                    .unwrap(),
            );
        }
        held
    }
}

pub(crate) struct Lease {
    _permit: OwnedSemaphorePermit,
    timeout: Duration,
}

impl Lease {
    pub(crate) async fn run(self, request: impl Future<Output = Response>) -> Response {
        match tokio::time::timeout(self.timeout, request).await {
            Ok(response) => response,
            Err(_) => crate::busy_response(),
        }
    }
}

/// Preserve async-graphql's GET/JSON/multipart parsing and its rejection responses.
/// Reading a GraphQL body has its own 10s / 64-slot budget before classification;
/// a slow sender cannot reserve a one-hour maintenance slot. The route's existing
/// 2MiB body cap also applies during extraction.
pub(crate) struct AdmittedGraphQLRequest {
    pub(crate) request: GraphQLRequest,
    pub(crate) lease: Lease,
}

impl<S> FromRequest<S> for AdmittedGraphQLRequest
where
    S: Send + Sync,
    Arc<RequestBudgets>: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let budgets = Arc::<RequestBudgets>::from_ref(state);
        let ingress = budgets
            .ingress
            .clone()
            .try_acquire_owned()
            .map_err(|_| crate::busy_response())?;
        let request = match tokio::time::timeout(
            Duration::from_secs(10),
            <GraphQLRequest as FromRequest<S>>::from_request(request, state),
        )
        .await
        {
            Ok(Ok(request)) => request,
            Ok(Err(rejection)) => return Err(rejection.into_response()),
            Err(_) => return Err(crate::busy_response()),
        };
        let class =
            classify(&request.0.query, request.0.operation_name.as_deref()).map_err(|error| {
                async_graphql_axum::GraphQLResponse::from(async_graphql::Response::from_errors(
                    vec![error],
                ))
                .into_response()
            })?;
        drop(ingress);
        let permit = budgets
            .lane(class)
            .clone()
            .try_acquire_owned()
            .map_err(|_| crate::busy_response())?;
        Ok(Self {
            request,
            lease: Lease {
                _permit: permit,
                timeout: class.timeout(),
            },
        })
    }
}

pub(crate) async fn limit_uploads(
    State(budgets): State<Arc<RequestBudgets>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(permit) = budgets.uploads.clone().try_acquire_owned() else {
        return crate::busy_response();
    };
    Lease {
        _permit: permit,
        timeout: Duration::from_secs(120),
    }
    .run(next.run(request))
    .await
}

/// Images retain their own cover/page/resize/proxy semaphores. A route-only
/// deadline bounds their queue and redirect lifetime without borrowing API slots.
pub(crate) async fn limit_image_time(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => crate::busy_response(),
    }
}

fn field_class(name: &str) -> Class {
    match name {
        // A numeric library write may have to fetch a missing series for its echo.
        "series" | "chapters" | "seriesPage" | "pages" | "canonicalPages" | "chapterOrigin"
        | "discovery" | "browseSources"
        | "mark" | "setLibraryStatus" | "setFavorite"
        // Even an engine-local lookup has a 30s client deadline; extension refresh
        // and source browse additionally wait on remote extension/source servers.
        | "extensionRepos" | "extensions" | "sources" | "sourceScanHealth"
        | "workSourceRows" | "sourceBrowse" | "accessPreflight" => Class::Upstream,
        "searchAllSources" => Class::Federated,
        // Foreground maintenance, plus job kickoffs whose validation consults the
        // engine or whose initial count/writes can traverse the whole catalogue.
        "updateSeriesAdmin" | "setSeriesPaused" | "triggerScan" | "updateSeriesMetadata"
        | "addSeriesAltTitle" | "removeSeriesAltTitle" | "consolidateExactDuplicates"
        | "markSourceNsfw" | "rederiveSuwayomiNsfw" | "rescanWork" | "retryCover"
        | "setAccessMode" | "checkCloudflareToken" | "startCloudflareSetup"
        | "retryCloudflareSetup" | "removeCloudflareSetup" | "addSourceSeries"
        | "mergeWorks" | "splitSourceSeries" | "resolveMergeCandidate"
        | "addExtensionRepo" | "removeExtensionRepo" | "installExtension"
        | "uninstallExtension" | "updateExtension" | "backfillMangadexMetadata"
        | "persistCatalogue" | "materializeCatalogueCovers" | "reconcileCatalogue"
        | "startSourceIngest" | "startExtensionIngest" | "cancelExtensionIngest"
        | "bulkAddSourceSeries" => Class::Maintenance,
        _ => Class::Local,
    }
}

/// Classify schema field names, never aliases or client-chosen operation labels.
/// Only the selected operation counts. Invalid/missing operation selection stays
/// in the short lane and async-graphql produces the normal validation error.
fn classify(query: &str, operation_name: Option<&str>) -> async_graphql::ServerResult<Class> {
    let Ok(document) = async_graphql::parser::parse_query(query) else {
        return Ok(Class::Local);
    };
    validate_fragment_expansion(&document)?;
    let operation = match (&document.operations, operation_name) {
        (DocumentOperations::Single(operation), None) => Some(operation),
        (DocumentOperations::Multiple(operations), Some(name)) => operations.get(name),
        (DocumentOperations::Multiple(operations), None) if operations.len() == 1 => {
            operations.values().next()
        }
        _ => None,
    };
    let Some(operation) = operation else {
        return Ok(Class::Local);
    };
    let mut class = Class::Local;
    let mut pending = vec![&operation.node.selection_set.node];
    let mut seen = HashSet::new();
    while let Some(set) = pending.pop() {
        for selection in &set.items {
            match &selection.node {
                Selection::Field(field) => {
                    class = class.max(field_class(field.node.name.node.as_str()))
                }
                Selection::InlineFragment(fragment) => {
                    pending.push(&fragment.node.selection_set.node)
                }
                Selection::FragmentSpread(spread) => {
                    let name = spread.node.fragment_name.node.as_str();
                    if seen.insert(name) {
                        if let Some(fragment) = document.fragments.get(name) {
                            pending.push(&fragment.node.selection_set.node);
                        }
                    }
                }
            }
        }
    }
    Ok(class)
}

/// The parser's syntax-depth bound does not constrain a flat chain of named
/// fragments. Bound expansion BEFORE async-graphql's recursive validation walks.
/// Compute each dependency once, never expand a fragment DAG into all its paths.
pub(crate) fn validate_fragment_expansion(
    document: &ExecutableDocument,
) -> async_graphql::ServerResult<()> {
    const MAX_EXPANDED_DEPTH: usize = 32;
    const MAX_EXPANDED_SELECTIONS: usize = 50_000;
    let error = |message| async_graphql::ServerError::new(message, None);
    let mut sets: Vec<&SelectionSet> = document
        .operations
        .iter()
        .map(|(_, operation)| &operation.node.selection_set.node)
        .collect();
    let mut fragments = HashMap::new();
    for (name, fragment) in &document.fragments {
        fragments.insert(name.as_str(), sets.len());
        sets.push(&fragment.node.selection_set.node);
    }
    let mut parents = vec![Vec::new(); sets.len()];
    let mut dependencies = vec![0usize; sets.len()];
    let mut depths = vec![1usize; sets.len()];
    let mut sizes = vec![0usize; sets.len()];
    for (index, root) in sets.into_iter().enumerate() {
        let mut pending = vec![(root, 1usize)];
        while let Some((set, depth)) = pending.pop() {
            if depth > MAX_EXPANDED_DEPTH {
                return Err(error("Fragment expansion may be at most 32 levels deep"));
            }
            depths[index] = depths[index].max(depth);
            sizes[index] = sizes[index].saturating_add(set.items.len());
            if sizes[index] > MAX_EXPANDED_SELECTIONS {
                return Err(error(
                    "Fragment expansion may contain at most 50000 selections",
                ));
            }
            for selection in &set.items {
                match &selection.node {
                    Selection::Field(field) if !field.node.selection_set.node.items.is_empty() => {
                        pending.push((&field.node.selection_set.node, depth + 1));
                    }
                    Selection::InlineFragment(fragment) => {
                        pending.push((&fragment.node.selection_set.node, depth + 1))
                    }
                    Selection::FragmentSpread(spread) => {
                        if let Some(&child) = fragments.get(spread.node.fragment_name.node.as_str())
                        {
                            parents[child].push((index, depth));
                            dependencies[index] += 1;
                        }
                        // Undefined fragments remain the schema validator's error.
                    }
                    _ => {}
                }
            }
        }
    }
    let mut ready: VecDeque<usize> = dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| (count == 0).then_some(index))
        .collect();
    let mut completed = 0;
    while let Some(child) = ready.pop_front() {
        completed += 1;
        for &(parent, depth) in &parents[child] {
            depths[parent] = depths[parent].max(depth.saturating_add(depths[child]));
            sizes[parent] = sizes[parent].saturating_add(sizes[child]);
            if depths[parent] > MAX_EXPANDED_DEPTH {
                return Err(error("Fragment expansion may be at most 32 levels deep"));
            }
            if sizes[parent] > MAX_EXPANDED_SELECTIONS {
                return Err(error(
                    "Fragment expansion may contain at most 50000 selections",
                ));
            }
            dependencies[parent] -= 1;
            if dependencies[parent] == 0 {
                ready.push_back(parent);
            }
        }
    }
    if completed != dependencies.len() {
        return Err(error("Fragment cycles are not allowed"));
    }
    Ok(())
}

/// A cancelled request must not strand a process-global maintenance flag. It can
/// also move into a background task, keeping kickoff errors and task panics safe.
pub(crate) struct RunningGuard<T: std::ops::Deref<Target = std::sync::atomic::AtomicBool>>(T);

impl<T: std::ops::Deref<Target = std::sync::atomic::AtomicBool>> RunningGuard<T> {
    pub(crate) fn try_acquire(flag: T) -> Option<Self> {
        flag.compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .ok()
        .map(|_| Self(flag))
    }
}

impl<T: std::ops::Deref<Target = std::sync::atomic::AtomicBool>> Drop for RunningGuard<T> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request as HttpRequest, StatusCode},
        routing::{get, post},
        Router,
    };
    use tower::ServiceExt;

    fn classify(query: &str, operation_name: Option<&str>) -> Class {
        super::classify(query, operation_name).unwrap()
    }

    pub(crate) fn fragment_chain(count: usize, duplicate: bool) -> String {
        let mut query = String::from("query { ...F0 }");
        for index in 0..count {
            query.push_str(&format!(" fragment F{index} on QueryRoot {{ "));
            if index + 1 == count {
                query.push_str("pages(chapterId:\"1\") { sourceUrl }");
            } else {
                query.push_str(&format!("...F{}", index + 1));
                if duplicate {
                    query.push_str(&format!(" ...F{}", index + 1));
                }
            }
            query.push_str(" }");
        }
        query
    }

    #[test]
    fn fragment_preflight_bounds_flat_chains_cycles_and_exponential_dags() {
        let valid = fragment_chain(8, false);
        assert_eq!(classify(&valid, None), Class::Upstream);
        for (query, expected) in [
            (fragment_chain(10_000, false), "32 levels"),
            (fragment_chain(20, true), "50000 selections"),
            ("query { ...A } fragment A on QueryRoot { ...A }".to_string(), "cycles"),
            ("query { ...A } fragment A on QueryRoot { ...B } fragment B on QueryRoot { ...C } fragment C on QueryRoot { ...A }".to_string(), "cycles"),
        ] {
            let error = super::classify(&query, None).unwrap_err();
            assert!(error.message.contains(expected), "{}", error.message);
        }
    }

    #[tokio::test]
    async fn fragment_preflight_rejects_before_handler_or_maintenance_admission() {
        let budgets = Arc::new(RequestBudgets::default());
        let _held = budgets
            .maintenance
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let query = fragment_chain(1000, false).replace(
            "pages(chapterId:\"1\") { sourceUrl }",
            "retryCover(workId:\"w\")",
        );
        let response = router(budgets.clone())
            .oneshot(request(&query, 0))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "validation is a GraphQL error, not a saturation response"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(response["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("32 levels"));
        assert_eq!(budgets.ingress.available_permits(), 64);
    }

    async fn execute(request: AdmittedGraphQLRequest) -> Response {
        request
            .lease
            .run(async move {
                let seconds = request
                    .request
                    .0
                    .variables
                    .get("seconds")
                    .and_then(|value| match value {
                        async_graphql::Value::Number(n) => n.as_u64(),
                        _ => None,
                    })
                    .unwrap_or_default();
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                "done".into_response()
            })
            .await
    }

    fn router(budgets: Arc<RequestBudgets>) -> Router {
        Router::new()
            .route("/graphql", get(execute).post(execute))
            .route("/health", get(crate::health))
            .route("/health/ready", get(|| async { "ready" }))
            .route("/covers/{file}", get(|| async { "cover" }))
            .route("/api/v1/manga/{*rest}", get(|| async { "image" }))
            .route("/img", get(|| async { "proxy" }))
            .route(
                "/avatar",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "uploaded"
                })
                .layer(axum::middleware::from_fn_with_state(
                    budgets.clone(),
                    limit_uploads,
                )),
            )
            .route(
                "/upload-hang",
                post(|| async { std::future::pending::<Response>().await }).layer(
                    axum::middleware::from_fn_with_state(budgets.clone(), limit_uploads),
                ),
            )
            .with_state(budgets)
    }

    fn request(query: &str, seconds: u64) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "query": query, "variables": { "seconds": seconds } })
                    .to_string(),
            ))
            .unwrap()
    }

    #[test]
    fn selected_operation_and_schema_names_choose_budget_including_fragments() {
        assert_eq!(
            classify(
                "query Fast { session { token } } mutation Slow { retryCover(workId:\"w\") }",
                Some("Fast")
            ),
            Class::Local
        );
        assert_eq!(
            classify(
                "query Fast { session { token } } mutation Slow { retryCover(workId:\"w\") }",
                Some("Slow")
            ),
            Class::Maintenance
        );
        for selected in [None, Some("missing")] {
            assert_eq!(
                classify(
                    "query A { session { token } } mutation B { retryCover(workId:\"w\") }",
                    selected
                ),
                Class::Local
            );
        }
        assert_eq!(
            classify(
                "query Alias { innocent: pages(chapterId:\"1\") { sourceUrl } }",
                Some("Alias")
            ),
            Class::Upstream
        );
        assert_eq!(classify("mutation { ...A } fragment A on MutationRoot { ...B } fragment B on MutationRoot { innocent: backfillMangadexMetadata }", None), Class::Maintenance);
        assert_eq!(
            classify(
                "query { ... on QueryRoot { innocent: searchAllSources(query:\"x\") { page } } }",
                None
            ),
            Class::Federated
        );
        assert!(super::classify("query { ...Cycle } fragment Cycle on QueryRoot { ...Cycle pages(chapterId:\"1\") { sourceUrl } }", None).is_err());
        // Child fields and operation labels cannot request a larger budget.
        assert_eq!(
            classify(
                "query retryCover { search(query:\"x\") { items { alias: metadata { year } } } }",
                Some("retryCover")
            ),
            Class::Local
        );
        assert_eq!(classify("invalid", None), Class::Local);
    }

    #[tokio::test]
    async fn saturation_is_retryable_and_every_graphql_lane_is_independent() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone());
        for (class, query) in [
            (Class::Local, "{ session { token } }"),
            (Class::Upstream, "{ pages(chapterId:\"1\") { sourceUrl } }"),
            (
                Class::Federated,
                "{ searchAllSources(query:\"x\") { page } }",
            ),
            (Class::Maintenance, "mutation { retryCover(workId:\"w\") }"),
        ] {
            let lane = budgets.lane(class);
            let held = lane
                .clone()
                .acquire_many_owned(lane.available_permits() as u32)
                .await
                .unwrap();
            let response = app.clone().oneshot(request(query, 0)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{class:?}"
            );
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            for (other, other_query) in [
                (Class::Local, "{ session { token } }"),
                (Class::Upstream, "{ series(id:\"1\") { id } }"),
                (
                    Class::Federated,
                    "{ searchAllSources(query:\"x\") { page } }",
                ),
                (Class::Maintenance, "mutation { retryCover(workId:\"w\") }"),
            ] {
                if other != class {
                    assert_eq!(
                        app.clone()
                            .oneshot(request(other_query, 0))
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::OK,
                        "{class:?} cannot block {other:?}"
                    );
                }
            }
            drop(held);
        }
    }

    #[tokio::test]
    async fn health_and_images_bypass_all_graphql_and_upload_admission() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone());
        let mut held = Vec::new();
        for lane in [
            &budgets.ingress,
            &budgets.local,
            &budgets.upstream,
            &budgets.federated,
            &budgets.maintenance,
            &budgets.uploads,
        ] {
            held.push(
                lane.clone()
                    .acquire_many_owned(lane.available_permits() as u32)
                    .await
                    .unwrap(),
            );
        }
        for path in [
            "/health",
            "/health/ready",
            "/covers/w.webp",
            "/api/v1/manga/1/thumbnail",
            "/api/v1/manga/1/chapter/2/page/3",
            "/img?src=example",
        ] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        assert_eq!(
            app.oneshot(request("{ session { token } }", 0))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn execution_deadlines_allow_cold_fetches_and_maintenance_past_twenty_seconds() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone());
        for (class, query, timeout) in [
            (Class::Local, "{ session { token } }", 20),
            (
                Class::Upstream,
                "{ seriesPage(workId:\"w\") { series { id } } }",
                300,
            ),
            (
                Class::Federated,
                "{ searchAllSources(query:\"x\") { page } }",
                1800,
            ),
            (
                Class::Maintenance,
                "mutation { backfillMangadexMetadata }",
                3600,
            ),
        ] {
            if class != Class::Local {
                let started = tokio::time::Instant::now();
                assert_eq!(
                    app.clone()
                        .oneshot(request(query, 30))
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::OK
                );
                assert_eq!(started.elapsed(), Duration::from_secs(30));
            }
            let started = tokio::time::Instant::now();
            let response = app
                .clone()
                .oneshot(request(query, timeout + 1))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(started.elapsed(), Duration::from_secs(timeout));
            let expected = match class {
                Class::Local => 64,
                Class::Upstream => 16,
                _ => 2,
            };
            assert_eq!(budgets.lane(class).available_permits(), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn uploads_have_separate_admission_and_a_two_minute_deadline() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone());
        let _held = budgets.local.clone().acquire_many_owned(64).await.unwrap();
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/avatar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let started = tokio::time::Instant::now();
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/upload-hang")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(started.elapsed(), Duration::from_secs(120));
        assert_eq!(budgets.uploads.available_permits(), 4);
        let _uploads = budgets.uploads.clone().acquire_many_owned(4).await.unwrap();
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/avatar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_graphql_body_cannot_reserve_a_long_execution_slot() {
        let budgets = Arc::new(RequestBudgets::default());
        let stream = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(stream))
            .unwrap();
        let started = tokio::time::Instant::now();
        let response = router(budgets.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert_eq!(budgets.ingress.available_permits(), 64);
        assert_eq!(budgets.maintenance.available_permits(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn image_burst_does_not_consume_graphql_slots_and_has_its_own_deadline() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone()).route(
            "/slow-cover",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "cover"
            })
            .layer(axum::middleware::from_fn_with_state(
                Duration::from_secs(120),
                limit_image_time,
            )),
        );
        let image_app = app.clone();
        let images = tokio::spawn(async move {
            futures::future::join_all((0..140).map(|_| {
                image_app.clone().oneshot(
                    HttpRequest::builder()
                        .uri("/slow-cover")
                        .body(Body::empty())
                        .unwrap(),
                )
            }))
            .await
        });
        tokio::task::yield_now().await;
        assert!(!images.is_finished());
        assert_eq!(budgets.local.available_permits(), 64);
        assert_eq!(
            app.clone()
                .oneshot(request("{ session { token } }", 0))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        assert!(images
            .await
            .unwrap()
            .into_iter()
            .all(|response| response.unwrap().status() == StatusCode::OK));
        for seconds in [60, 120, 180] {
            let app = Router::new()
                .route(
                    "/image",
                    get(|| async { std::future::pending::<Response>().await }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    Duration::from_secs(seconds),
                    limit_image_time,
                ));
            let started = tokio::time::Instant::now();
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .uri("/image")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(started.elapsed(), Duration::from_secs(seconds));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn get_and_multipart_requests_use_the_same_selected_operation_budget() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone());
        let _held = budgets
            .maintenance
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let query = "mutation Long { retryCover(workId:\"w\") } query Fast { session { token } }";
        let mut url = reqwest::Url::parse("http://localhost/graphql").unwrap();
        url.query_pairs_mut()
            .append_pair("query", query)
            .append_pair("operationName", "Fast");
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/graphql?{}", url.query().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = format!("--gql\r\nContent-Disposition: form-data; name=\"operations\"\r\n\r\n{}\r\n--gql\r\nContent-Disposition: form-data; name=\"map\"\r\n\r\n{{}}\r\n--gql--\r\n", serde_json::json!({"query": query, "operationName": "Long"}));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/graphql")
                    .header(header::CONTENT_TYPE, "multipart/form-data; boundary=gql")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_releases_single_flight_guard_and_admission() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let permits = Arc::new(Semaphore::new(1));
        let lease = Lease {
            _permit: permits.clone().acquire_owned().await.unwrap(),
            timeout: Duration::from_secs(1),
        };
        let flag_for_request = flag.clone();
        let response = lease
            .run(async move {
                let _running = RunningGuard::try_acquire(flag_for_request).unwrap();
                std::future::pending::<Response>().await
            })
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!flag.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(permits.available_permits(), 1);
    }
}
