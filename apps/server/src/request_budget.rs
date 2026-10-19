//! Admission and deadlines for GraphQL and uploads. Image and health routes never
//! enter these lanes: they have independent resource limits (or must always answer).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{Arc, Mutex, Weak},
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

use crate::telemetry::{AdmissionStats, Deadline, Held, Lane, LaneStats};

pub(crate) const UPSTREAM_CLIENT_CONCURRENCY: usize = 3;

/// The upstream identity of every edge render (`GET /graphql/ssr`). The renders reach
/// the API from a handful of shared Workers addresses, so they get one budget of their
/// own: [`EDGE_RENDER_SHARE`] times one reader's miss rate, and [`EDGE_RENDER_CONCURRENCY`]
/// of the global upstream slots. Only the reader's Worker can reach that path on the
/// hosted service (a WAF rule); elsewhere the worst a caller can do with it is use up
/// this one shared budget.
pub(crate) const EDGE_RENDER_CLIENT: &str = "edge-render";
const EDGE_RENDER_SHARE: u32 = 10;
const EDGE_RENDER_CONCURRENCY: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Class {
    Local,
    Upstream,
    Federated,
    Maintenance,
}

impl Class {
    fn lane(self) -> Lane {
        match self {
            Self::Local | Self::Upstream => Lane::Local,
            Self::Federated => Lane::Federated,
            Self::Maintenance => Lane::Maintenance,
        }
    }

    fn deadline(self) -> Deadline {
        match self {
            Self::Local => Deadline::Local,
            Self::Upstream => Deadline::Upstream,
            Self::Federated => Deadline::Federated,
            Self::Maintenance => Deadline::Maintenance,
        }
    }

    fn timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Local => 20,
            Self::Upstream => 300,
            Self::Federated => 1800,
            Self::Maintenance => 3600,
        })
    }
}

/// HTTP admission and network work are different resources. Reader operations
/// share local admission, even when their deadline allows a cold source fetch.
/// Only actual foreground engine/API calls take an upstream slot.
pub(crate) struct RequestBudgets {
    ingress: Arc<Semaphore>,
    local: Arc<Semaphore>,
    /// Requests that may wait for a `local` permit at once ([`LOCAL_WAITERS`]).
    local_waiters: Arc<Semaphore>,
    upstream: Arc<Semaphore>,
    cache_waiters: Arc<Semaphore>,
    upstream_clients: Mutex<HashMap<String, Weak<Semaphore>>>,
    federated: Arc<Semaphore>,
    maintenance: Arc<Semaphore>,
    uploads: Arc<Semaphore>,
    /// Admission telemetry: the process global in production; a test may own one.
    stats: &'static AdmissionStats,
}

/// Requests that may hold a local permit at once. Not the database pools alone: a
/// reader waiting on a source holds one too. 64 covers the 16 upstream calls and 16
/// cache waiters that can be parked on the network plus the default 16 readers and
/// 8 foreground writers (56). Measured on the test host (2026-10-15): with 24
/// permits a 500-reader release burst was at most about 10 % faster, while 32
/// network-bound requests could have taken every permit.
const LOCAL_PERMITS: usize = 64;

/// Requests that may wait for a local permit at once, and for how long. The same
/// burst queued about 2,300 requests behind the 64 permits and served all of them,
/// the slowest series page in 3.7 s. 1,024 places is about what the pools serve
/// within the wait at the full read mix (about 250 requests a second).
const LOCAL_WAITERS: usize = 1024;
const LOCAL_WAIT: Duration = Duration::from_secs(5);

impl Default for RequestBudgets {
    fn default() -> Self {
        Self::with_stats(&crate::telemetry::ADMISSION)
    }
}

impl RequestBudgets {
    fn with_stats(stats: &'static AdmissionStats) -> Self {
        Self {
            ingress: Arc::new(Semaphore::new(64)),
            local: Arc::new(Semaphore::new(LOCAL_PERMITS)),
            local_waiters: Arc::new(Semaphore::new(LOCAL_WAITERS)),
            upstream: Arc::new(Semaphore::new(16)),
            cache_waiters: Arc::new(Semaphore::new(16)),
            upstream_clients: Mutex::new(HashMap::new()),
            federated: Arc::new(Semaphore::new(2)),
            maintenance: Arc::new(Semaphore::new(2)),
            uploads: Arc::new(Semaphore::new(4)),
            stats,
        }
    }

    /// Budgets whose admission counters belong to the calling test alone.
    #[cfg(test)]
    pub(crate) fn with_test_stats() -> (Self, &'static AdmissionStats) {
        let stats = AdmissionStats::leak();
        (Self::with_stats(stats), stats)
    }

    fn lane(&self, class: Class) -> &Arc<Semaphore> {
        match class {
            Class::Local | Class::Upstream => &self.local,
            Class::Federated => &self.federated,
            Class::Maintenance => &self.maintenance,
        }
    }

    /// A permit of `class`'s lane. The local lane waits for one, first come first
    /// served, while fewer than [`LOCAL_WAITERS`] requests wait and for at most
    /// [`LOCAL_WAIT`]; a burst of readers is queued instead of refused. The admin
    /// lanes refuse at once.
    async fn admit(&self, class: Class) -> Option<OwnedSemaphorePermit> {
        let lane = self.lane(class);
        if let Ok(permit) = lane.clone().try_acquire_owned() {
            return Some(permit);
        }
        if class.lane() != Lane::Local {
            return None;
        }
        let _waiting = self.local_waiters.clone().try_acquire_owned().ok()?;
        let started = tokio::time::Instant::now();
        let permit = tokio::time::timeout(LOCAL_WAIT, lane.clone().acquire_owned())
            .await
            .ok()?
            .ok()?;
        self.stats.lane(Lane::Local).waited(started.elapsed());
        Some(permit)
    }

    /// The trusted, resolved client identity follows the request's foreground
    /// futures. Detached background/image work retains its existing own limits.
    pub(crate) async fn scope_upstream<F: Future>(
        self: &Arc<Self>,
        client: String,
        work: F,
    ) -> F::Output {
        UPSTREAM_REQUEST
            .scope(
                UpstreamContext {
                    budgets: self.clone(),
                    client,
                },
                work,
            )
            .await
    }

    fn try_upstream(&self, client: &str) -> anyhow::Result<UpstreamPermit> {
        self.try_resource(&self.upstream, Lane::Upstream, client)
    }

    fn try_resource(
        &self,
        resource: &Arc<Semaphore>,
        lane: Lane,
        client: &str,
    ) -> anyhow::Result<UpstreamPermit> {
        let Ok(global) = resource.clone().try_acquire_owned() else {
            self.stats.lane(lane).refuse();
            return Err(UpstreamBusy.into());
        };
        let per_client = {
            let mut clients = self
                .upstream_clients
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // A live permit owns its semaphore, so active client limits cannot
            // be evicted. At most 16 calls plus 16 cache waiters own live keys.
            clients.retain(|_, semaphore| semaphore.strong_count() != 0);
            match clients.get(client).and_then(Weak::upgrade) {
                Some(semaphore) => semaphore,
                None => {
                    let limit = if client == EDGE_RENDER_CLIENT {
                        EDGE_RENDER_CONCURRENCY
                    } else {
                        UPSTREAM_CLIENT_CONCURRENCY
                    };
                    let semaphore = Arc::new(Semaphore::new(limit));
                    clients.insert(client.to_owned(), Arc::downgrade(&semaphore));
                    semaphore
                }
            }
        };
        let Ok(client) = per_client.try_acquire_owned() else {
            self.stats.lane(Lane::UpstreamClient).refuse();
            return Err(UpstreamBusy.into());
        };
        Ok(UpstreamPermit {
            _held: (
                self.stats.lane(lane).admit(),
                self.stats.lane(Lane::UpstreamClient).admit(),
            ),
            _global: global,
            _client: client,
        })
    }

    #[cfg(test)]
    pub(crate) fn upstream_available_for_test(&self) -> usize {
        self.upstream.available_permits()
    }

    #[cfg(test)]
    pub(crate) fn upstream_waiters_available_for_test(&self) -> usize {
        self.cache_waiters.available_permits()
    }

    #[cfg(test)]
    pub(crate) async fn exhaust_for_test(&self) -> Vec<OwnedSemaphorePermit> {
        let mut held = Vec::new();
        for lane in [
            &self.ingress,
            &self.local,
            &self.local_waiters,
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

struct UpstreamContext {
    budgets: Arc<RequestBudgets>,
    client: String,
}

tokio::task_local! {
    static UPSTREAM_REQUEST: UpstreamContext;
}

/// Owned through the complete network response/body, with normal drop releasing
/// both bounds after completion, cancellation or a request deadline.
pub(crate) struct UpstreamPermit {
    // Fields drop in order: the in-flight gauges fall BEFORE the permits are
    // released, so a waiter woken by the release cannot raise the gauge (and its
    // high water) past the lane's capacity.
    _held: (Held, Held),
    _global: OwnedSemaphorePermit,
    _client: OwnedSemaphorePermit,
}

/// Contended cache initializers have a separate bound. They share the client's
/// three slots with real calls, and cannot fill all 64 local request permits.
/// A waiter holds this only while it waits. Its wait is counted once it obtains
/// the initializer ([`Self::obtained`]); a cancelled wait is not counted, as in
/// the other waiting lanes.
pub(crate) struct CacheWaitPermit {
    _held: UpstreamPermit,
    started: std::time::Instant,
    lane: &'static LaneStats,
}

impl CacheWaitPermit {
    /// The wait succeeded: count it and release the waiter's shares.
    pub(crate) fn obtained(self) {
        self.lane.waited(self.started.elapsed());
    }
}

#[derive(Debug)]
pub(crate) struct UpstreamBusy;

impl std::fmt::Display for UpstreamBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Upstream capacity is busy; retry soon")
    }
}

impl std::error::Error for UpstreamBusy {}

pub(crate) fn is_upstream_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<UpstreamBusy>().is_some()
}

/// Called only at a real engine/API fetch, after all cache/single-flight checks.
/// Background jobs and image transports keep their independent resource bounds.
pub(crate) fn upstream_permit() -> anyhow::Result<Option<UpstreamPermit>> {
    UPSTREAM_REQUEST
        .try_with(|context| context.budgets.try_upstream(&context.client).map(Some))
        .unwrap_or(Ok(None))
}

pub(crate) fn cache_wait_permit() -> anyhow::Result<Option<CacheWaitPermit>> {
    UPSTREAM_REQUEST
        .try_with(|context| {
            let budgets = &context.budgets;
            budgets
                .try_resource(&budgets.cache_waiters, Lane::CacheWaiters, &context.client)
                .map(|held| {
                    Some(CacheWaitPermit {
                        _held: held,
                        started: std::time::Instant::now(),
                        lane: budgets.stats.lane(Lane::CacheWaiters),
                    })
                })
        })
        .unwrap_or(Ok(None))
}

pub(crate) fn upstream_concurrency(preferred: usize) -> usize {
    UPSTREAM_REQUEST
        .try_with(|_| preferred.min(UPSTREAM_CLIENT_CONCURRENCY))
        .unwrap_or(preferred)
}

pub(crate) async fn lock_cold_cache<T>(
    lock: &tokio::sync::Mutex<T>,
) -> anyhow::Result<tokio::sync::MutexGuard<'_, T>> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(_) => {
            let waiter = cache_wait_permit()?;
            let guard = lock.lock().await;
            if let Some(waiter) = waiter {
                waiter.obtained();
            }
            Ok(guard)
        }
    }
}

/// Existing public-reader miss rate limit, separate from concurrent network work.
/// Admin jobs keep their existing rate policies. Cache hits never spend this budget.
static UPSTREAM_LIMITER: std::sync::LazyLock<crate::rate_limit::RateLimiter> =
    std::sync::LazyLock::new(|| crate::rate_limit::RateLimiter::new(60, 60));

#[derive(Debug)]
pub(crate) struct UpstreamRateLimit(pub(crate) u64);

impl std::fmt::Display for UpstreamRateLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Too many upstream requests; retry in {}s",
            self.0
        )
    }
}

impl std::error::Error for UpstreamRateLimit {}

pub(crate) fn check_upstream_rate(client: Option<&str>) -> anyhow::Result<()> {
    if let Some(client) = client {
        let lane = crate::telemetry::ADMISSION.lane(Lane::UpstreamRate);
        let share = if client == EDGE_RENDER_CLIENT {
            EDGE_RENDER_SHARE
        } else {
            1
        };
        match UPSTREAM_LIMITER.check_scaled(&format!("upstream:{client}"), share) {
            Ok(()) => lane.admitted(),
            Err(retry) => {
                lane.refuse();
                return Err(UpstreamRateLimit(retry).into());
            }
        }
    }
    Ok(())
}

pub(crate) fn check_current_upstream_rate() -> anyhow::Result<()> {
    UPSTREAM_REQUEST
        .try_with(|context| check_upstream_rate(Some(&context.client)))
        .unwrap_or(Ok(()))
}

pub(crate) fn is_upstream_rate_limited(error: &anyhow::Error) -> bool {
    error.downcast_ref::<UpstreamRateLimit>().is_some()
}

pub(crate) struct Lease {
    // Drops before `_permit` (field order), as in `UpstreamPermit`.
    _held: Option<Held>,
    _permit: OwnedSemaphorePermit,
    timeout: Duration,
    deadline: Option<(&'static AdmissionStats, Deadline)>,
}

impl Lease {
    pub(crate) async fn run(self, request: impl Future<Output = Response>) -> Response {
        match tokio::time::timeout(self.timeout, request).await {
            Ok(response) => response,
            Err(_) => {
                if let Some((stats, class)) = self.deadline {
                    stats.deadline(class);
                }
                crate::busy_response()
            }
        }
    }
}

/// Preserve async-graphql's GET/JSON/multipart parsing and its rejection responses.
/// Reading a GraphQL body has its own 10s / 64-slot budget before classification;
/// a slow sender cannot reserve a one-hour maintenance slot. The route's existing
/// 2MiB body cap also applies during extraction.
pub(crate) struct AdmittedGraphQLRequest {
    pub(crate) request: async_graphql::Request,
    pub(crate) lease: Lease,
    pub(crate) upstream: Arc<RequestBudgets>,
}

impl<S> FromRequest<S> for AdmittedGraphQLRequest
where
    S: Send + Sync,
    Arc<RequestBudgets>: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request(mut request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let budgets = Arc::<RequestBudgets>::from_ref(state);
        let stats = budgets.stats;
        let Ok(ingress) = budgets.ingress.clone().try_acquire_owned() else {
            stats.lane(Lane::Ingress).refuse();
            return Err(crate::busy_response());
        };
        // Tuple elements drop in order: the gauge falls before the permit is released.
        let ingress = (stats.lane(Lane::Ingress).admit(), ingress);
        // A GET that named a persisted document was resolved by `persisted::resolve_get`.
        let persisted = request
            .extensions_mut()
            .remove::<crate::persisted::PersistedRequest>();
        let request = if let Some(persisted) = persisted {
            persisted.into_request()
        } else {
            match tokio::time::timeout(
                Duration::from_secs(10),
                <GraphQLRequest as FromRequest<S>>::from_request(request, state),
            )
            .await
            {
                Ok(Ok(request)) => request.into_inner(),
                Ok(Err(rejection)) => return Err(rejection.into_response()),
                Err(_) => {
                    stats.deadline(Deadline::GraphqlBody);
                    return Err(crate::busy_response());
                }
            }
        };
        let class =
            classify(&request.query, request.operation_name.as_deref()).map_err(|error| {
                async_graphql_axum::GraphQLResponse::from(async_graphql::Response::from_errors(
                    vec![error],
                ))
                .into_response()
            })?;
        drop(ingress);
        let Some(permit) = budgets.admit(class).await else {
            stats.lane(class.lane()).refuse();
            return Err(crate::busy_response());
        };
        Ok(Self {
            request,
            lease: Lease {
                _held: Some(stats.lane(class.lane()).admit()),
                _permit: permit,
                timeout: class.timeout(),
                deadline: Some((stats, class.deadline())),
            },
            upstream: budgets,
        })
    }
}

pub(crate) async fn limit_uploads(
    State(budgets): State<Arc<RequestBudgets>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(permit) = budgets.uploads.clone().try_acquire_owned() else {
        budgets.stats.lane(Lane::Uploads).refuse();
        return crate::busy_response();
    };
    Lease {
        _held: Some(budgets.stats.lane(Lane::Uploads).admit()),
        _permit: permit,
        timeout: Duration::from_secs(120),
        deadline: Some((budgets.stats, Deadline::Upload)),
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
        Err(_) => {
            crate::telemetry::ADMISSION.deadline(Deadline::Image);
            let mut response = crate::busy_response();
            crate::telemetry::mark(&mut response, crate::telemetry::MediaOutcome::Deadline);
            response
        }
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

    #[tokio::test]
    async fn real_upstream_work_has_global_and_per_client_bounds() {
        let budgets = Arc::new(RequestBudgets::default());
        assert!(upstream_permit().unwrap().is_none());
        assert_eq!(upstream_concurrency(6), 6);
        let first = budgets
            .scope_upstream("client-a".into(), async {
                assert_eq!(upstream_concurrency(6), 3);
                let held: Vec<_> = (0..3)
                    .map(|_| upstream_permit().unwrap().unwrap())
                    .collect();
                assert!(is_upstream_busy(&upstream_permit().err().unwrap()));
                held
            })
            .await;
        assert_eq!(budgets.upstream.available_permits(), 13);
        let other = budgets
            .scope_upstream("client-b".into(), async {
                upstream_permit().unwrap().unwrap()
            })
            .await;
        assert_eq!(budgets.upstream.available_permits(), 12);
        drop((first, other));
        assert_eq!(budgets.upstream.available_permits(), 16);

        let mut held = Vec::new();
        for index in 0..16 {
            held.push(
                budgets
                    .scope_upstream(format!("client-{}", index / 3), async {
                        upstream_permit().unwrap().unwrap()
                    })
                    .await,
            );
        }
        budgets
            .scope_upstream("another-client".into(), async {
                assert!(is_upstream_busy(&upstream_permit().err().unwrap()));
            })
            .await;
        assert_eq!(budgets.upstream.available_permits(), 0);
        drop(held);
        assert_eq!(budgets.upstream.available_permits(), 16);
    }

    #[tokio::test(start_paused = true)]
    async fn upstream_cancellation_releases_both_bounds_and_idle_clients_are_reclaimed() {
        let budgets = Arc::new(RequestBudgets::default());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            budgets.scope_upstream("unknown".into(), async {
                let _permit = upstream_permit().unwrap().unwrap();
                std::future::pending::<()>().await;
            }),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(budgets.upstream.available_permits(), 16);
        for index in 0..1000 {
            budgets
                .scope_upstream(format!("idle-{index}"), async {
                    let _permit = upstream_permit().unwrap().unwrap();
                })
                .await;
        }
        assert!(budgets.upstream_clients.lock().unwrap().len() <= 1);
        assert_eq!(budgets.upstream.available_permits(), 16);
    }

    #[tokio::test]
    async fn cache_waiters_are_separately_bounded_and_share_the_client_call_cap() {
        let budgets = Arc::new(RequestBudgets::default());
        assert!(cache_wait_permit().unwrap().is_none());
        let held = budgets
            .scope_upstream("client".into(), async {
                let call = upstream_permit().unwrap().unwrap();
                let waits: Vec<_> = (0..2)
                    .map(|_| cache_wait_permit().unwrap().unwrap())
                    .collect();
                assert!(is_upstream_busy(&cache_wait_permit().err().unwrap()));
                assert!(is_upstream_busy(&upstream_permit().err().unwrap()));
                (call, waits)
            })
            .await;
        assert_eq!(budgets.upstream.available_permits(), 15);
        assert_eq!(budgets.cache_waiters.available_permits(), 14);
        drop(held);
        let mut waits = Vec::new();
        for index in 0..16 {
            waits.push(
                budgets
                    .scope_upstream(format!("waiting-{}", index / 3), async {
                        cache_wait_permit().unwrap().unwrap()
                    })
                    .await,
            );
        }
        budgets
            .scope_upstream("another".into(), async {
                assert!(is_upstream_busy(&cache_wait_permit().err().unwrap()));
                let _call = upstream_permit().unwrap().unwrap();
            })
            .await;
        assert_eq!(budgets.upstream.available_permits(), 16);
        assert_eq!(budgets.cache_waiters.available_permits(), 0);
        drop(waits);
        assert_eq!(budgets.cache_waiters.available_permits(), 16);
    }

    #[tokio::test(start_paused = true)]
    async fn long_deadline_readers_use_local_admission_without_reserving_upstream() {
        let budgets = Arc::new(RequestBudgets::default());
        let _all_network = budgets
            .upstream
            .clone()
            .acquire_many_owned(16)
            .await
            .unwrap();
        let app = router(budgets.clone());
        let response = app
            .oneshot(request("{pages(chapterId:\"1\"){sourceUrl}}", 30))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(budgets.local.available_permits(), 64);
        assert_eq!(budgets.upstream.available_permits(), 0);
    }

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
        assert_eq!(
            classify(
                "mutation { ...A } fragment A on MutationRoot { ...B } fragment B on MutationRoot { innocent: backfillMangadexMetadata }",
                None
            ),
            Class::Maintenance
        );
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

    #[tokio::test(start_paused = true)]
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
                if !Arc::ptr_eq(budgets.lane(other), budgets.lane(class)) {
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
                Class::Local | Class::Upstream => 64,
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
        let body = format!(
            "--gql\r\nContent-Disposition: form-data; name=\"operations\"\r\n\r\n{}\r\n--gql\r\nContent-Disposition: form-data; name=\"map\"\r\n\r\n{{}}\r\n--gql--\r\n",
            serde_json::json!({"query": query, "operationName": "Long"})
        );
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
    async fn eight_blocked_upstream_reads_leave_cached_mutation_and_upload_lanes_available() {
        let budgets = Arc::new(RequestBudgets::default());
        let app = router(budgets.clone());
        let (started, mut reached) = tokio::sync::mpsc::channel(8);
        let mut blocked = Vec::new();
        for i in 0..8 {
            let budgets = budgets.clone();
            let started = started.clone();
            blocked.push(tokio::spawn(async move {
                // A cold reader holds ordinary request admission and upstream
                // capacity through its pending engine read, exactly as production.
                let _admission = budgets.local.clone().acquire_owned().await.unwrap();
                let _upstream = budgets.try_upstream(&format!("client-{i}")).unwrap();
                started.send(()).await.unwrap();
                std::future::pending::<()>().await;
            }));
        }
        for _ in 0..8 {
            reached.recv().await.unwrap();
        }
        for query in [
            "{ session { token } }",
            "mutation { setFavorite(seriesId:\"w\",favorite:true) { id } }",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(query, 0))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            app.oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/avatar")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        for task in blocked {
            task.abort();
            let _ = task.await;
        }
        assert_eq!(budgets.upstream.available_permits(), 16);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_releases_admission() {
        // A cancelled request gives its admission permit back.
        let permits = Arc::new(Semaphore::new(1));
        let lease = Lease {
            _held: None,
            _permit: permits.clone().acquire_owned().await.unwrap(),
            timeout: Duration::from_secs(1),
            deadline: None,
        };
        let response = lease
            .run(async move { std::future::pending::<Response>().await })
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn telemetry_names_the_refusing_graphql_lane_and_in_flight_returns_to_zero() {
        let (budgets, stats) = RequestBudgets::with_test_stats();
        let budgets = Arc::new(budgets);
        let app = router(budgets.clone());
        let held = budgets.local.clone().acquire_many_owned(64).await.unwrap();
        let response = app
            .clone()
            .oneshot(request("{ session { token } }", 0))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // (admitted, refused, in flight, in-flight high water)
        assert_eq!(stats.lane(Lane::Local).snapshot(), (0, 1, 0, 0));
        assert_eq!(stats.lane(Lane::Ingress).snapshot(), (1, 0, 0, 1));
        assert_eq!(stats.lane(Lane::Maintenance).snapshot(), (0, 0, 0, 0));
        drop(held);
        let response = app
            .clone()
            .oneshot(request("{ session { token } }", 0))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(stats.lane(Lane::Local).snapshot(), (1, 1, 0, 1));
        let _maintenance = budgets
            .maintenance
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let response = app
            .oneshot(request("mutation { retryCover(workId:\"w\") }", 0))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(stats.lane(Lane::Maintenance).snapshot(), (0, 1, 0, 0));
        assert_eq!(stats.lane(Lane::Local).snapshot(), (1, 1, 0, 1));
    }

    /// A burst past the local lane's permits is queued, not refused: a waiting
    /// request takes the next permit that frees and its wait is counted. A request
    /// that gets no permit within the wait, or finds every waiting place taken, is
    /// refused as before, and the admin lanes still refuse at once.
    #[tokio::test(start_paused = true)]
    async fn local_lane_queues_a_burst_for_a_bounded_wait() {
        let (budgets, stats) = RequestBudgets::with_test_stats();
        let budgets = Arc::new(budgets);
        let app = router(budgets.clone());
        let lane = stats.lane(Lane::Local);
        let permits = budgets.local.available_permits() as u32;
        let mut held = budgets
            .local
            .clone()
            .acquire_many_owned(permits)
            .await
            .unwrap();
        let waiting = tokio::spawn(app.clone().oneshot(request("{ session { token } }", 0)));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!waiting.is_finished());
        drop(held.split(1).unwrap());
        let response = waiting.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // (admitted, refused, in flight, in-flight high water)
        assert_eq!(lane.snapshot(), (1, 0, 0, 1));
        assert_eq!(lane.wait_micros(), 2_000_000);
        held.merge(budgets.local.clone().acquire_owned().await.unwrap());

        let started = tokio::time::Instant::now();
        let response = app
            .clone()
            .oneshot(request("{ session { token } }", 0))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(started.elapsed(), LOCAL_WAIT);
        assert_eq!(lane.snapshot(), (1, 1, 0, 1));

        let places = budgets.local_waiters.available_permits() as u32;
        let taken = budgets
            .local_waiters
            .clone()
            .acquire_many_owned(places)
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        let response = app
            .clone()
            .oneshot(request("{ session { token } }", 0))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(lane.snapshot(), (1, 2, 0, 1));
        drop(taken);

        let _maintenance = budgets
            .maintenance
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        let response = app
            .oneshot(request("mutation { retryCover(workId:\"w\") }", 0))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(started.elapsed(), Duration::ZERO);
        drop(held);
        assert_eq!(budgets.local.available_permits() as u32, permits);
        assert_eq!(budgets.local_waiters.available_permits() as u32, places);
    }

    #[tokio::test]
    async fn telemetry_separates_global_and_per_client_upstream_refusals() {
        let (budgets, stats) = RequestBudgets::with_test_stats();
        let budgets = Arc::new(budgets);
        let held = budgets
            .scope_upstream("client".into(), async {
                let held: Vec<_> = (0..3)
                    .map(|_| upstream_permit().unwrap().unwrap())
                    .collect();
                assert!(is_upstream_busy(&upstream_permit().err().unwrap()));
                held
            })
            .await;
        assert_eq!(stats.lane(Lane::Upstream).snapshot(), (3, 0, 3, 3));
        assert_eq!(stats.lane(Lane::UpstreamClient).snapshot(), (3, 1, 3, 3));
        drop(held);
        assert_eq!(stats.lane(Lane::Upstream).snapshot(), (3, 0, 0, 3));
        assert_eq!(stats.lane(Lane::UpstreamClient).snapshot(), (3, 1, 0, 3));
        let _all = budgets
            .upstream
            .clone()
            .acquire_many_owned(16)
            .await
            .unwrap();
        budgets
            .scope_upstream("other".into(), async {
                assert!(is_upstream_busy(&upstream_permit().err().unwrap()));
            })
            .await;
        assert_eq!(stats.lane(Lane::Upstream).snapshot(), (3, 1, 0, 3));
        assert_eq!(stats.lane(Lane::UpstreamClient).snapshot(), (3, 1, 0, 3));
    }

    #[tokio::test]
    async fn telemetry_records_cache_waiter_wait_and_refusal() {
        let (budgets, stats) = RequestBudgets::with_test_stats();
        let budgets = Arc::new(budgets);
        let lock = tokio::sync::Mutex::new(());
        let guard = lock.lock().await;
        let lane = stats.lane(Lane::CacheWaiters);
        budgets
            .scope_upstream("waiter".into(), async {
                let waiting = lock_cold_cache(&lock);
                tokio::pin!(waiting);
                assert!(futures::poll!(waiting.as_mut()).is_pending());
                assert_eq!(lane.snapshot(), (1, 0, 1, 1));
                std::thread::sleep(Duration::from_millis(5));
                drop(guard);
                drop(waiting.await.unwrap());
            })
            .await;
        assert_eq!(lane.snapshot(), (1, 0, 0, 1));
        assert!(lane.wait_micros() >= 5_000, "{}", lane.wait_micros());
        let _waiters = budgets
            .cache_waiters
            .clone()
            .acquire_many_owned(16)
            .await
            .unwrap();
        budgets
            .scope_upstream("waiter".into(), async {
                assert!(is_upstream_busy(&cache_wait_permit().err().unwrap()));
            })
            .await;
        assert_eq!(lane.snapshot(), (1, 1, 0, 1));
    }

    /// A woken waiter's waker reads the lane gauge at the instant the permit is
    /// released (Tokio wakes semaphore waiters inside the release).
    struct GaugeProbe {
        lane: &'static LaneStats,
        seen: std::sync::Mutex<Option<u64>>,
    }

    impl futures::task::ArcWake for GaugeProbe {
        fn wake_by_ref(probe: &Arc<Self>) {
            *probe.seen.lock().unwrap() = Some(probe.lane.snapshot().2);
        }
    }

    /// Queue a waiter on `semaphore` whose wake records `lane`'s in-flight gauge.
    fn gauge_at_release(
        semaphore: &Arc<Semaphore>,
        lane: &'static LaneStats,
    ) -> (
        Arc<GaugeProbe>,
        std::pin::Pin<
            Box<impl Future<Output = Result<OwnedSemaphorePermit, tokio::sync::AcquireError>>>,
        >,
    ) {
        let probe = Arc::new(GaugeProbe {
            lane,
            seen: std::sync::Mutex::new(None),
        });
        let waker = futures::task::waker(probe.clone());
        let mut waiting = Box::pin(semaphore.clone().acquire_owned());
        assert!(waiting
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending());
        (probe, waiting)
    }

    #[tokio::test]
    async fn telemetry_gauges_fall_before_permits_are_released() {
        let (budgets, stats) = RequestBudgets::with_test_stats();
        let budgets = Arc::new(budgets);
        // Upstream call: the global lane's gauge is already down when its permit
        // wakes the next acquirer.
        let call = budgets
            .scope_upstream("client".into(), async {
                upstream_permit().unwrap().unwrap()
            })
            .await;
        let _rest = budgets.upstream.clone().try_acquire_many_owned(15).unwrap();
        let (probe, _waiting) = gauge_at_release(&budgets.upstream, stats.lane(Lane::Upstream));
        assert_eq!(stats.lane(Lane::Upstream).snapshot().2, 1);
        drop(call);
        assert_eq!(*probe.seen.lock().unwrap(), Some(0));
        assert_eq!(stats.lane(Lane::Upstream).snapshot().3, 1);

        // GraphQL/upload lease: same order.
        let slots = Arc::new(Semaphore::new(1));
        let lane = stats.lane(Lane::Uploads);
        let lease = Lease {
            _held: Some(lane.admit()),
            _permit: slots.clone().acquire_owned().await.unwrap(),
            timeout: Duration::from_secs(1),
            deadline: None,
        };
        let (probe, _waiting) = gauge_at_release(&slots, lane);
        drop(lease);
        assert_eq!(*probe.seen.lock().unwrap(), Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn telemetry_counts_class_and_upload_deadlines() {
        let (budgets, stats) = RequestBudgets::with_test_stats();
        let budgets = Arc::new(budgets);
        let app = router(budgets.clone());
        let response = app
            .clone()
            .oneshot(request("{ session { token } }", 21))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(stats.deadlines(Deadline::Local), 1);
        assert_eq!(stats.deadlines(Deadline::Upstream), 0);
        assert_eq!(stats.lane(Lane::Local).snapshot(), (1, 0, 0, 1));
        let response = app
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
        assert_eq!(stats.deadlines(Deadline::Upload), 1);
        assert_eq!(stats.lane(Lane::Uploads).snapshot(), (1, 0, 0, 1));
    }

    /// Edge renders share one budget of their own, larger than one reader's.
    #[test]
    fn edge_renders_get_a_shared_budget_larger_than_one_readers() {
        for _ in 0..(60 * EDGE_RENDER_SHARE as usize) {
            check_upstream_rate(Some(EDGE_RENDER_CLIENT)).unwrap();
        }
        assert!(check_upstream_rate(Some(EDGE_RENDER_CLIENT)).is_err());
        let reader = "edge-render-test-reader";
        for _ in 0..60 {
            check_upstream_rate(Some(reader)).unwrap();
        }
        assert!(check_upstream_rate(Some(reader)).is_err());
    }
}
