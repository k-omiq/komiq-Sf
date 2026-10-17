//! Process-wide load-run counters for the loopback `/metrics` scrape: which
//! admission lane admitted or refused work, how the read-path caches behaved, what
//! the media routes answered and how much they sent. Same model as the database
//! counters in `db.rs`: static `AtomicU64`s with `Relaxed` ordering and fixed label
//! sets, so observing a path allocates nothing and never changes its ordering,
//! locking or outcome. Every value is since process start.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use axum::{
    http::{header, Method, StatusCode},
    response::Response,
};

/// A gauge plus its high-water mark since process start.
pub(crate) struct Level {
    current: AtomicU64,
    max: AtomicU64,
}

impl Level {
    pub(crate) const fn new() -> Self {
        Self {
            current: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }

    pub(crate) fn raise(&self, n: u64) {
        let now = self.current.fetch_add(n, Relaxed).saturating_add(n);
        self.max.fetch_max(now, Relaxed);
    }

    pub(crate) fn lower(&self, n: u64) {
        self.current.fetch_sub(n, Relaxed);
    }

    /// For a level measured elsewhere (a queue length read under its own lock).
    pub(crate) fn observe(&self, value: u64) {
        self.current.store(value, Relaxed);
        self.max.fetch_max(value, Relaxed);
    }

    pub(crate) fn current(&self) -> u64 {
        self.current.load(Relaxed)
    }

    pub(crate) fn max(&self) -> u64 {
        self.max.load(Relaxed)
    }
}

// ---- admission ------------------------------------------------------------------

/// Every bounded admission point a request can be refused at. `Local` is shared by
/// the local and upstream GraphQL classes (one semaphore); their deadlines differ
/// and are reported per [`Deadline`] class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lane {
    Ingress,
    Local,
    Federated,
    Maintenance,
    Uploads,
    Upstream,
    CacheWaiters,
    UpstreamClient,
    UpstreamRate,
    PageFetch,
    CoverFetch,
    ProxyFetch,
    Ingest,
}

const LANE_COUNT: usize = 13;

impl Lane {
    const ALL: [Lane; LANE_COUNT] = [
        Lane::Ingress,
        Lane::Local,
        Lane::Federated,
        Lane::Maintenance,
        Lane::Uploads,
        Lane::Upstream,
        Lane::CacheWaiters,
        Lane::UpstreamClient,
        Lane::UpstreamRate,
        Lane::PageFetch,
        Lane::CoverFetch,
        Lane::ProxyFetch,
        Lane::Ingest,
    ];

    fn label(self) -> &'static str {
        match self {
            Lane::Ingress => "ingress",
            Lane::Local => "local",
            Lane::Federated => "federated",
            Lane::Maintenance => "maintenance",
            Lane::Uploads => "uploads",
            Lane::Upstream => "upstream",
            Lane::CacheWaiters => "cache_waiters",
            Lane::UpstreamClient => "upstream_client",
            Lane::UpstreamRate => "upstream_rate",
            Lane::PageFetch => "page_fetch",
            Lane::CoverFetch => "cover_fetch",
            Lane::ProxyFetch => "proxy_fetch",
            Lane::Ingest => "ingest",
        }
    }

    /// Lanes whose permits are tracked while held. The rate limiter holds nothing;
    /// media fetch permits move into the response payload (see the retained-owner
    /// gauges instead).
    fn tracks_held(self) -> bool {
        !matches!(
            self,
            Lane::UpstreamRate
                | Lane::PageFetch
                | Lane::CoverFetch
                | Lane::ProxyFetch
                | Lane::Ingest
        )
    }

    /// Lanes that can wait for capacity (all others refuse immediately).
    fn waits(self) -> bool {
        matches!(
            self,
            Lane::Local | Lane::CacheWaiters | Lane::PageFetch | Lane::ProxyFetch
        )
    }
}

/// Request deadlines that answer a retryable 503.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Deadline {
    GraphqlBody,
    Local,
    Upstream,
    Federated,
    Maintenance,
    Upload,
    Image,
}

const DEADLINE_COUNT: usize = 7;

impl Deadline {
    const ALL: [Deadline; DEADLINE_COUNT] = [
        Deadline::GraphqlBody,
        Deadline::Local,
        Deadline::Upstream,
        Deadline::Federated,
        Deadline::Maintenance,
        Deadline::Upload,
        Deadline::Image,
    ];

    fn label(self) -> &'static str {
        match self {
            Deadline::GraphqlBody => "graphql_body",
            Deadline::Local => "local",
            Deadline::Upstream => "upstream",
            Deadline::Federated => "federated",
            Deadline::Maintenance => "maintenance",
            Deadline::Upload => "upload",
            Deadline::Image => "image",
        }
    }
}

pub(crate) struct LaneStats {
    admitted: AtomicU64,
    refused: AtomicU64,
    wait_micros: AtomicU64,
    held: Level,
}

impl LaneStats {
    const fn new() -> Self {
        Self {
            admitted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            wait_micros: AtomicU64::new(0),
            held: Level::new(),
        }
    }

    /// Count an admission and track the permit until the returned guard drops.
    pub(crate) fn admit(&'static self) -> Held {
        self.admitted.fetch_add(1, Relaxed);
        self.held.raise(1);
        Held(self)
    }

    /// Count an admission whose permit is not tracked here.
    pub(crate) fn admitted(&self) {
        self.admitted.fetch_add(1, Relaxed);
    }

    pub(crate) fn refuse(&self) {
        self.refused.fetch_add(1, Relaxed);
    }

    pub(crate) fn waited(&self, wait: Duration) {
        self.wait_micros.fetch_add(wait.as_micros() as u64, Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.admitted.load(Relaxed),
            self.refused.load(Relaxed),
            self.held.current(),
            self.held.max(),
        )
    }

    #[cfg(test)]
    pub(crate) fn wait_micros(&self) -> u64 {
        self.wait_micros.load(Relaxed)
    }
}

/// An admitted permit, counted in its lane's in-flight gauge until dropped.
pub(crate) struct Held(&'static LaneStats);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.held.lower(1);
    }
}

pub(crate) struct AdmissionStats {
    lanes: [LaneStats; LANE_COUNT],
    deadlines: [AtomicU64; DEADLINE_COUNT],
}

impl AdmissionStats {
    pub(crate) const fn new() -> Self {
        Self {
            lanes: [const { LaneStats::new() }; LANE_COUNT],
            deadlines: [const { AtomicU64::new(0) }; DEADLINE_COUNT],
        }
    }

    /// A private instance, so a test can assert exact deltas while other tests move
    /// the process globals.
    #[cfg(test)]
    pub(crate) fn leak() -> &'static Self {
        Box::leak(Box::new(Self::new()))
    }

    pub(crate) fn lane(&self, lane: Lane) -> &LaneStats {
        &self.lanes[lane as usize]
    }

    pub(crate) fn deadline(&self, class: Deadline) {
        self.deadlines[class as usize].fetch_add(1, Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn deadlines(&self, class: Deadline) -> u64 {
        self.deadlines[class as usize].load(Relaxed)
    }

    fn prometheus(&self, text: &mut String) {
        header(
            text,
            "komika_admission_admitted_total",
            "Requests or calls admitted by an admission lane",
            "counter",
        );
        for lane in Lane::ALL {
            let value = self.lane(lane).admitted.load(Relaxed);
            let _ = writeln!(
                text,
                "komika_admission_admitted_total{{lane=\"{}\"}} {value}",
                lane.label()
            );
        }
        header(
            text,
            "komika_admission_refused_total",
            "Requests or calls refused by an admission lane (retryable busy/rate answer)",
            "counter",
        );
        for lane in Lane::ALL {
            let value = self.lane(lane).refused.load(Relaxed);
            let _ = writeln!(
                text,
                "komika_admission_refused_total{{lane=\"{}\"}} {value}",
                lane.label()
            );
        }
        header(
            text,
            "komika_admission_wait_seconds_total",
            "Time successful waits spent waiting for capacity (lanes that wait instead of refusing; \
             refused or cancelled waits are not included)",
            "counter",
        );
        for lane in Lane::ALL.into_iter().filter(|lane| lane.waits()) {
            let micros = self.lane(lane).wait_micros.load(Relaxed);
            let _ = writeln!(
                text,
                "komika_admission_wait_seconds_total{{lane=\"{}\"}} {}",
                lane.label(),
                micros as f64 / 1_000_000.0
            );
        }
        header(
            text,
            "komika_admission_in_flight",
            "Permits currently held in an admission lane",
            "gauge",
        );
        for lane in Lane::ALL.into_iter().filter(|lane| lane.tracks_held()) {
            let _ = writeln!(
                text,
                "komika_admission_in_flight{{lane=\"{}\"}} {}",
                lane.label(),
                self.lane(lane).held.current()
            );
        }
        header(
            text,
            "komika_admission_in_flight_max",
            "Most permits held at once in an admission lane since process start",
            "gauge",
        );
        for lane in Lane::ALL.into_iter().filter(|lane| lane.tracks_held()) {
            let _ = writeln!(
                text,
                "komika_admission_in_flight_max{{lane=\"{}\"}} {}",
                lane.label(),
                self.lane(lane).held.max()
            );
        }
        header(
            text,
            "komika_request_deadline_exceeded_total",
            "Requests answered 503 because their class deadline expired",
            "counter",
        );
        for class in Deadline::ALL {
            let value = self.deadlines[class as usize].load(Relaxed);
            let _ = writeln!(
                text,
                "komika_request_deadline_exceeded_total{{class=\"{}\"}} {value}",
                class.label()
            );
        }
    }
}

pub(crate) static ADMISSION: AdmissionStats = AdmissionStats::new();

// ---- caches ----------------------------------------------------------------------

pub(crate) struct CacheStats {
    hits: AtomicU64,
    misses: AtomicU64,
    fills: AtomicU64,
    fill_errors: AtomicU64,
    evictions: AtomicU64,
    expired: AtomicU64,
    invalidated: AtomicU64,
    stale_refills_rejected: AtomicU64,
    coalesced: AtomicU64,
    waiter_refusals: AtomicU64,
    waiter_cancellations: AtomicU64,
    registry_full: AtomicU64,
    entries: AtomicU64,
}

macro_rules! cache_events {
    ($($name:ident),* $(,)?) => {
        impl CacheStats {
            $(
                pub(crate) fn $name(&self, n: u64) {
                    self.$name.fetch_add(n, Relaxed);
                }
            )*
        }
    };
}

cache_events!(
    hits,
    misses,
    fills,
    fill_errors,
    evictions,
    expired,
    invalidated,
    stale_refills_rejected,
    coalesced,
    waiter_refusals,
    waiter_cancellations,
    registry_full,
);

impl CacheStats {
    pub(crate) const fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            fills: AtomicU64::new(0),
            fill_errors: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            invalidated: AtomicU64::new(0),
            stale_refills_rejected: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            waiter_refusals: AtomicU64::new(0),
            waiter_cancellations: AtomicU64::new(0),
            registry_full: AtomicU64::new(0),
            entries: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn leak() -> &'static Self {
        Box::leak(Box::new(Self::new()))
    }

    /// Current entry count, measured by the cache under its own lock.
    pub(crate) fn set_entries(&self, entries: usize) {
        self.entries.store(entries as u64, Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> CacheSnapshot {
        CacheSnapshot {
            hits: self.hits.load(Relaxed),
            misses: self.misses.load(Relaxed),
            fills: self.fills.load(Relaxed),
            fill_errors: self.fill_errors.load(Relaxed),
            evictions: self.evictions.load(Relaxed),
            expired: self.expired.load(Relaxed),
            invalidated: self.invalidated.load(Relaxed),
            stale_refills_rejected: self.stale_refills_rejected.load(Relaxed),
            coalesced: self.coalesced.load(Relaxed),
            waiter_refusals: self.waiter_refusals.load(Relaxed),
            waiter_cancellations: self.waiter_cancellations.load(Relaxed),
            registry_full: self.registry_full.load(Relaxed),
            entries: self.entries.load(Relaxed),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CacheSnapshot {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) fills: u64,
    pub(crate) fill_errors: u64,
    pub(crate) evictions: u64,
    pub(crate) expired: u64,
    pub(crate) invalidated: u64,
    pub(crate) stale_refills_rejected: u64,
    pub(crate) coalesced: u64,
    pub(crate) waiter_refusals: u64,
    pub(crate) waiter_cancellations: u64,
    pub(crate) registry_full: u64,
    pub(crate) entries: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CacheKind {
    /// Generation-guarded value cache (`catalogue_cache::CatalogueCache`).
    Value,
    /// Single-flight upstream memo (`upstream_cache::TtlMemo`).
    SingleFlight,
    /// Freshness marker map (`series_cache` chapter-fetch marker).
    Marker,
}

pub(crate) static CACHE_BROWSE_COUNT: CacheStats = CacheStats::new();
pub(crate) static CACHE_BROWSE_ROWS: CacheStats = CacheStats::new();
pub(crate) static CACHE_FEED_DISCOVERY: CacheStats = CacheStats::new();
pub(crate) static CACHE_FEED_GENRE_FACETS: CacheStats = CacheStats::new();
pub(crate) static CACHE_FEED_TAGS: CacheStats = CacheStats::new();
pub(crate) static CACHE_FEED_BROWSE_SOURCES: CacheStats = CacheStats::new();
pub(crate) static CACHE_ENGINE_PAGES: CacheStats = CacheStats::new();
pub(crate) static CACHE_ENGINE_CHAPTER_OWNER: CacheStats = CacheStats::new();
pub(crate) static CACHE_MANGADEX_AT_HOME: CacheStats = CacheStats::new();
pub(crate) static CACHE_REGIONAL_MANGA: CacheStats = CacheStats::new();
pub(crate) static CACHE_REGIONAL_PAGES: CacheStats = CacheStats::new();
pub(crate) static CACHE_CHAPTER_FETCH_MARKER: CacheStats = CacheStats::new();

static CACHES: [(&str, CacheKind, &CacheStats); 12] = [
    ("browse_count", CacheKind::Value, &CACHE_BROWSE_COUNT),
    ("browse_rows", CacheKind::Value, &CACHE_BROWSE_ROWS),
    ("feed_discovery", CacheKind::Value, &CACHE_FEED_DISCOVERY),
    (
        "feed_genre_facets",
        CacheKind::Value,
        &CACHE_FEED_GENRE_FACETS,
    ),
    ("feed_tags", CacheKind::Value, &CACHE_FEED_TAGS),
    (
        "feed_browse_sources",
        CacheKind::Value,
        &CACHE_FEED_BROWSE_SOURCES,
    ),
    ("engine_pages", CacheKind::SingleFlight, &CACHE_ENGINE_PAGES),
    (
        "engine_chapter_owner",
        CacheKind::SingleFlight,
        &CACHE_ENGINE_CHAPTER_OWNER,
    ),
    (
        "mangadex_at_home",
        CacheKind::SingleFlight,
        &CACHE_MANGADEX_AT_HOME,
    ),
    (
        "regional_engine_manga",
        CacheKind::SingleFlight,
        &CACHE_REGIONAL_MANGA,
    ),
    (
        "regional_engine_pages",
        CacheKind::SingleFlight,
        &CACHE_REGIONAL_PAGES,
    ),
    (
        "chapter_fetch_marker",
        CacheKind::Marker,
        &CACHE_CHAPTER_FETCH_MARKER,
    ),
];

type CacheField = fn(&CacheStats) -> &AtomicU64;
/// Metric name, help, type, field, and which cache kinds report it.
type CacheRow = (
    &'static str,
    &'static str,
    &'static str,
    CacheField,
    fn(CacheKind) -> bool,
);

fn caches_prometheus(text: &mut String) {
    let all: fn(CacheKind) -> bool = |_| true;
    let values: fn(CacheKind) -> bool = |kind| kind == CacheKind::Value;
    let flights: fn(CacheKind) -> bool = |kind| kind == CacheKind::SingleFlight;
    let rows: [CacheRow; 13] = [
        (
            "komika_cache_hits_total",
            "Lookups answered from a fresh cached value without waiting",
            "counter",
            |s| &s.hits,
            all,
        ),
        (
            "komika_cache_misses_total",
            "Lookups that found no fresh value (single-flight: lookups that ran the fill)",
            "counter",
            |s| &s.misses,
            all,
        ),
        (
            "komika_cache_fills_total",
            "Values stored (single-flight: successful leader fills)",
            "counter",
            |s| &s.fills,
            all,
        ),
        (
            "komika_cache_evictions_total",
            "Live entries evicted to keep the cache within its capacity",
            "counter",
            |s| &s.evictions,
            all,
        ),
        (
            "komika_cache_expired_total",
            "Expired or old-generation entries dropped",
            "counter",
            |s| &s.expired,
            all,
        ),
        (
            "komika_cache_invalidated_entries_total",
            "Entries dropped by an explicit invalidation",
            "counter",
            |s| &s.invalidated,
            values,
        ),
        (
            "komika_cache_stale_refills_rejected_total",
            "Refills refused because an invalidation advanced the generation during the read",
            "counter",
            |s| &s.stale_refills_rejected,
            values,
        ),
        (
            "komika_cache_fill_errors_total",
            "Single-flight leader fills that failed (nothing cached; retryable)",
            "counter",
            |s| &s.fill_errors,
            flights,
        ),
        (
            "komika_cache_coalesced_waiters_total",
            "Callers that waited on another caller's in-flight fill and shared its value",
            "counter",
            |s| &s.coalesced,
            flights,
        ),
        (
            "komika_cache_waiter_refusals_total",
            "Same-key waiters refused by the cache-waiter or per-client bound",
            "counter",
            |s| &s.waiter_refusals,
            flights,
        ),
        (
            "komika_cache_waiter_cancellations_total",
            "Same-key waiters dropped (deadline or disconnect) before the fill finished",
            "counter",
            |s| &s.waiter_cancellations,
            flights,
        ),
        (
            "komika_cache_registry_full_total",
            "New keys refused because every slot had an active owner",
            "counter",
            |s| &s.registry_full,
            flights,
        ),
        (
            "komika_cache_entries",
            "Entries currently held",
            "gauge",
            |s| &s.entries,
            all,
        ),
    ];
    for (name, help, kind, field, applies) in rows {
        header(text, name, help, kind);
        for (cache, cache_kind, stats) in CACHES.iter() {
            if applies(*cache_kind) {
                let _ = writeln!(
                    text,
                    "{name}{{cache=\"{cache}\"}} {}",
                    field(stats).load(Relaxed)
                );
            }
        }
    }
}

// ---- media -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaRoute {
    Object,
    Manifest,
    Policy,
    /// Engine chapter page (`/api/v1/manga/{id}/chapter/{c}/page/{p}`).
    Page,
    /// Engine cover thumbnail (`/api/v1/manga/{id}/thumbnail`).
    EngineCover,
    /// Work cover (`/covers/{work}.webp`).
    Cover,
    Avatar,
    CommentMedia,
    ExtIcon,
    /// Self-host image proxy (`/img`).
    Proxy,
}

const MEDIA_ROUTE_COUNT: usize = 10;

impl MediaRoute {
    const ALL: [MediaRoute; MEDIA_ROUTE_COUNT] = [
        MediaRoute::Object,
        MediaRoute::Manifest,
        MediaRoute::Policy,
        MediaRoute::Page,
        MediaRoute::EngineCover,
        MediaRoute::Cover,
        MediaRoute::Avatar,
        MediaRoute::CommentMedia,
        MediaRoute::ExtIcon,
        MediaRoute::Proxy,
    ];

    fn label(self) -> &'static str {
        match self {
            MediaRoute::Object => "object",
            MediaRoute::Manifest => "manifest",
            MediaRoute::Policy => "policy",
            MediaRoute::Page => "page",
            MediaRoute::EngineCover => "engine_cover",
            MediaRoute::Cover => "cover",
            MediaRoute::Avatar => "avatar",
            MediaRoute::CommentMedia => "comment_media",
            MediaRoute::ExtIcon => "ext_icon",
            MediaRoute::Proxy => "proxy",
        }
    }
}

/// Response outcome. Handlers mark the outcomes a status cannot express (a no-store
/// fallback is a 200, a store failure is a 500/503) as a response extension; the
/// rest is derived from the status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaOutcome {
    Ok,
    NoStoreFallback,
    Redirect,
    NotModified,
    BadRequest,
    PolicyRefused,
    NotFound,
    ClientError,
    UpstreamError,
    StoreError,
    Busy,
    Deadline,
    Error,
}

const MEDIA_OUTCOME_COUNT: usize = 13;

impl MediaOutcome {
    const ALL: [MediaOutcome; MEDIA_OUTCOME_COUNT] = [
        MediaOutcome::Ok,
        MediaOutcome::NoStoreFallback,
        MediaOutcome::Redirect,
        MediaOutcome::NotModified,
        MediaOutcome::BadRequest,
        MediaOutcome::PolicyRefused,
        MediaOutcome::NotFound,
        MediaOutcome::ClientError,
        MediaOutcome::UpstreamError,
        MediaOutcome::StoreError,
        MediaOutcome::Busy,
        MediaOutcome::Deadline,
        MediaOutcome::Error,
    ];

    fn label(self) -> &'static str {
        match self {
            MediaOutcome::Ok => "ok",
            MediaOutcome::NoStoreFallback => "no_store_fallback",
            MediaOutcome::Redirect => "redirect",
            MediaOutcome::NotModified => "not_modified",
            MediaOutcome::BadRequest => "bad_request",
            MediaOutcome::PolicyRefused => "policy_refused",
            MediaOutcome::NotFound => "not_found",
            MediaOutcome::ClientError => "client_error",
            MediaOutcome::UpstreamError => "upstream_error",
            MediaOutcome::StoreError => "store_error",
            MediaOutcome::Busy => "busy",
            MediaOutcome::Deadline => "deadline",
            MediaOutcome::Error => "error",
        }
    }

    fn from_status(status: StatusCode) -> Self {
        match status.as_u16() {
            200..=299 => MediaOutcome::Ok,
            304 => MediaOutcome::NotModified,
            300..=399 => MediaOutcome::Redirect,
            400 => MediaOutcome::BadRequest,
            403 => MediaOutcome::PolicyRefused,
            404 => MediaOutcome::NotFound,
            405..=499 | 401 | 402 => MediaOutcome::ClientError,
            502 | 504 => MediaOutcome::UpstreamError,
            503 => MediaOutcome::Busy,
            _ => MediaOutcome::Error,
        }
    }
}

/// Upstream bodies checked before they are served or stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BodySource {
    Page,
    EngineCover,
    Proxy,
}

const BODY_SOURCE_COUNT: usize = 3;

impl BodySource {
    const ALL: [BodySource; BODY_SOURCE_COUNT] =
        [BodySource::Page, BodySource::EngineCover, BodySource::Proxy];

    fn label(self) -> &'static str {
        match self {
            BodySource::Page => "page",
            BodySource::EngineCover => "engine_cover",
            BodySource::Proxy => "proxy",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BodyCheck {
    Ok,
    Empty,
    Unsupported,
    Mismatched,
    Truncated,
    TooLarge,
}

const BODY_CHECK_COUNT: usize = 6;

impl BodyCheck {
    const ALL: [BodyCheck; BODY_CHECK_COUNT] = [
        BodyCheck::Ok,
        BodyCheck::Empty,
        BodyCheck::Unsupported,
        BodyCheck::Mismatched,
        BodyCheck::Truncated,
        BodyCheck::TooLarge,
    ];

    fn label(self) -> &'static str {
        match self {
            BodyCheck::Ok => "ok",
            BodyCheck::Empty => "empty",
            BodyCheck::Unsupported => "unsupported",
            BodyCheck::Mismatched => "mismatched",
            BodyCheck::Truncated => "truncated",
            BodyCheck::TooLarge => "too_large",
        }
    }
}

struct MediaRouteStats {
    responses: [AtomicU64; MEDIA_OUTCOME_COUNT],
    body_bytes: AtomicU64,
}

pub(crate) struct MediaStats {
    routes: [MediaRouteStats; MEDIA_ROUTE_COUNT],
    body_checks: [[AtomicU64; BODY_CHECK_COUNT]; BODY_SOURCE_COUNT],
    delivered_bytes: AtomicU64,
    delivery_aborts: AtomicU64,
    retained_owners: Level,
    retained_bytes: Level,
}

impl MediaStats {
    pub(crate) const fn new() -> Self {
        Self {
            routes: [const {
                MediaRouteStats {
                    responses: [const { AtomicU64::new(0) }; MEDIA_OUTCOME_COUNT],
                    body_bytes: AtomicU64::new(0),
                }
            }; MEDIA_ROUTE_COUNT],
            body_checks: [const { [const { AtomicU64::new(0) }; BODY_CHECK_COUNT] };
                BODY_SOURCE_COUNT],
            delivered_bytes: AtomicU64::new(0),
            delivery_aborts: AtomicU64::new(0),
            retained_owners: Level::new(),
            retained_bytes: Level::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn leak() -> &'static Self {
        Box::leak(Box::new(Self::new()))
    }

    pub(crate) fn response(&self, route: MediaRoute, outcome: MediaOutcome, body_bytes: u64) {
        let route = &self.routes[route as usize];
        route.responses[outcome as usize].fetch_add(1, Relaxed);
        route.body_bytes.fetch_add(body_bytes, Relaxed);
    }

    pub(crate) fn body_check(&self, source: BodySource, result: BodyCheck) {
        self.body_checks[source as usize][result as usize].fetch_add(1, Relaxed);
    }

    pub(crate) fn delivered(&self, bytes: usize) {
        self.delivered_bytes.fetch_add(bytes as u64, Relaxed);
    }

    pub(crate) fn delivery_aborted(&self) {
        self.delivery_aborts.fetch_add(1, Relaxed);
    }

    pub(crate) fn retain(&self, bytes: usize) {
        self.retained_owners.raise(1);
        self.retained_bytes.raise(bytes as u64);
    }

    pub(crate) fn release(&self, bytes: usize) {
        self.retained_owners.lower(1);
        self.retained_bytes.lower(bytes as u64);
    }

    #[cfg(test)]
    pub(crate) fn responses(&self, route: MediaRoute, outcome: MediaOutcome) -> u64 {
        self.routes[route as usize].responses[outcome as usize].load(Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn body_bytes(&self, route: MediaRoute) -> u64 {
        self.routes[route as usize].body_bytes.load(Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn body_checks(&self, source: BodySource, result: BodyCheck) -> u64 {
        self.body_checks[source as usize][result as usize].load(Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn retained(&self) -> (u64, u64) {
        (
            self.retained_owners.current(),
            self.retained_bytes.current(),
        )
    }

    #[cfg(test)]
    pub(crate) fn delivery(&self) -> (u64, u64) {
        (
            self.delivered_bytes.load(Relaxed),
            self.delivery_aborts.load(Relaxed),
        )
    }

    fn prometheus(&self, text: &mut String) {
        header(
            text,
            "komika_media_responses_total",
            "Media route responses by outcome",
            "counter",
        );
        for route in MediaRoute::ALL {
            for outcome in MediaOutcome::ALL {
                let value = self.routes[route as usize].responses[outcome as usize].load(Relaxed);
                let _ = writeln!(
                    text,
                    "komika_media_responses_total{{route=\"{}\",outcome=\"{}\"}} {value}",
                    route.label(),
                    outcome.label()
                );
            }
        }
        header(
            text,
            "komika_media_response_body_bytes_total",
            "Response body bytes as built by the route (Content-Length or exact body size; \
             not wire bytes, HEAD excluded, a client that disconnects early still counts)",
            "counter",
        );
        for route in MediaRoute::ALL {
            let _ = writeln!(
                text,
                "komika_media_response_body_bytes_total{{route=\"{}\"}} {}",
                route.label(),
                self.routes[route as usize].body_bytes.load(Relaxed)
            );
        }
        header(
            text,
            "komika_media_body_checks_total",
            "Upstream image bodies checked before serving or storing, by result",
            "counter",
        );
        for source in BodySource::ALL {
            for result in BodyCheck::ALL {
                let _ = writeln!(
                    text,
                    "komika_media_body_checks_total{{source=\"{}\",result=\"{}\"}} {}",
                    source.label(),
                    result.label(),
                    self.body_checks[source as usize][result as usize].load(Relaxed)
                );
            }
        }
        for (name, help, kind, value) in [
            (
                "komika_media_delivered_bytes_total",
                "Bytes of slot-owning image payloads handed to the HTTP transport \
                 (page, proxy and raw cover fallback bodies; body bytes, not wire bytes)",
                "counter",
                self.delivered_bytes.load(Relaxed),
            ),
            (
                "komika_media_delivery_aborts_total",
                "Slot-owning deliveries aborted because the client stalled or missed the deadline",
                "counter",
                self.delivery_aborts.load(Relaxed),
            ),
            (
                "komika_media_retained_payloads",
                "Fetched image payloads still owning their fetch slot",
                "gauge",
                self.retained_owners.current(),
            ),
            (
                "komika_media_retained_payloads_max",
                "Most fetched image payloads owning a fetch slot at once since process start",
                "gauge",
                self.retained_owners.max(),
            ),
            (
                "komika_media_retained_bytes",
                "Bytes held by fetched image payloads that still own their fetch slot",
                "gauge",
                self.retained_bytes.current(),
            ),
            (
                "komika_media_retained_bytes_max",
                "Most bytes held by slot-owning image payloads at once since process start",
                "gauge",
                self.retained_bytes.max(),
            ),
        ] {
            header(text, name, help, kind);
            let _ = writeln!(text, "{name} {value}");
        }
    }
}

pub(crate) static MEDIA: MediaStats = MediaStats::new();

/// Mark an outcome its status cannot express. Only rare (fallback/error) paths.
pub(crate) fn mark(response: &mut Response, outcome: MediaOutcome) {
    response.extensions_mut().insert(outcome);
}

/// Body size as built: an exact body size, else the declared `Content-Length`
/// (streamed slot-owning payloads set it). Unknown sizes count zero.
fn body_bytes(response: &Response) -> u64 {
    use axum::body::HttpBody as _;
    response.body().size_hint().exact().unwrap_or_else(|| {
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    })
}

pub(crate) fn record_media_response(
    stats: &MediaStats,
    route: MediaRoute,
    response: &Response,
    head: bool,
) {
    let outcome = response
        .extensions()
        .get::<MediaOutcome>()
        .copied()
        .unwrap_or_else(|| MediaOutcome::from_status(response.status()));
    let bytes = if head { 0 } else { body_bytes(response) };
    stats.response(route, outcome, bytes);
}

/// Route layer: count each media response by outcome and body size. Outermost on
/// the route so the image deadline's 503 is seen too. A concrete layer rather than
/// `middleware::from_fn`, so observing allocates nothing per request: no boxed
/// future and no boxed clone of the inner service. Request and response pass
/// through untouched.
#[derive(Clone, Copy)]
pub(crate) struct MediaLayer(pub(crate) MediaRoute);

impl<S> tower::Layer<S> for MediaLayer {
    type Service = MediaService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MediaService {
            inner,
            route: self.0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MediaService<S> {
    inner: S,
    route: MediaRoute,
}

impl<S, B> tower::Service<axum::http::Request<B>> for MediaService<S>
where
    S: tower::Service<
        axum::http::Request<B>,
        Response = Response,
        Error = std::convert::Infallible,
    >,
    S::Future: Unpin,
{
    type Response = Response;
    type Error = std::convert::Infallible;
    type Future = MediaFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: axum::http::Request<B>) -> Self::Future {
        // One route serves engine pages and engine cover thumbnails.
        let route =
            if self.route == MediaRoute::Page && request.uri().path().ends_with("/thumbnail") {
                MediaRoute::EngineCover
            } else {
                self.route
            };
        let head = request.method() == Method::HEAD;
        MediaFuture {
            inner: self.inner.call(request),
            route,
            head,
        }
    }
}

pub(crate) struct MediaFuture<F> {
    inner: F,
    route: MediaRoute,
    head: bool,
}

impl<F, E> std::future::Future for MediaFuture<F>
where
    F: std::future::Future<Output = Result<Response, E>> + Unpin,
{
    type Output = F::Output;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = &mut *self;
        let output = std::task::ready!(std::pin::Pin::new(&mut this.inner).poll(cx));
        if let Ok(response) = &output {
            record_media_response(&MEDIA, this.route, response, this.head);
        }
        std::task::Poll::Ready(output)
    }
}

// ---- bounded queues --------------------------------------------------------------

pub(crate) struct QueueStats {
    accepted: AtomicU64,
    refused: AtomicU64,
    dropped: AtomicU64,
    depth: Level,
}

impl QueueStats {
    pub(crate) const fn new() -> Self {
        Self {
            accepted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            depth: Level::new(),
        }
    }

    pub(crate) fn accepted(&self, depth: usize) {
        self.accepted.fetch_add(1, Relaxed);
        self.depth.observe(depth as u64);
    }

    pub(crate) fn refused(&self) {
        self.refused.fetch_add(1, Relaxed);
    }

    pub(crate) fn dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.accepted.load(Relaxed),
            self.refused.load(Relaxed),
            self.dropped.load(Relaxed),
            self.depth.max(),
        )
    }
}

/// Best-effort deferred cache writes (`Database::defer`).
pub(crate) static QUEUE_DEFERRED_WRITES: QueueStats = QueueStats::new();
/// The buffered chapter-view counter (`views::record`).
pub(crate) static QUEUE_VIEW_BUFFER: QueueStats = QueueStats::new();

static QUEUES: [(&str, &QueueStats); 2] = [
    ("deferred_writes", &QUEUE_DEFERRED_WRITES),
    ("view_buffer", &QUEUE_VIEW_BUFFER),
];

fn queues_prometheus(text: &mut String) {
    type Field = fn(&QueueStats) -> u64;
    let rows: [(&str, &str, &str, Field); 4] = [
        (
            "komika_queue_accepted_total",
            "Items accepted by a bounded request-path queue",
            "counter",
            |q| q.accepted.load(Relaxed),
        ),
        (
            "komika_queue_refused_total",
            "Items refused by a full or closing queue (deferred writes skip, views write directly)",
            "counter",
            |q| q.refused.load(Relaxed),
        ),
        (
            "komika_queue_dropped_total",
            "Queued items discarded past a hard cap (view buffer entries during flush failures)",
            "counter",
            |q| q.dropped.load(Relaxed),
        ),
        (
            "komika_queue_depth_max",
            "Largest committed queue depth after an accepted item since process start \
             (deferred writes: queued in the channel; view buffer: distinct entries)",
            "gauge",
            |q| q.depth.max(),
        ),
    ];
    for (name, help, kind, field) in rows {
        header(text, name, help, kind);
        for (queue, stats) in QUEUES.iter() {
            let _ = writeln!(text, "{name}{{queue=\"{queue}\"}} {}", field(stats));
        }
    }
}

// ---- exposition ------------------------------------------------------------------

fn header(text: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(text, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// Everything above, in Prometheus text format. `deferred_depth` is the current
/// number of deferred writes queued in their channel (the running one excluded).
pub(crate) fn prometheus(deferred_depth: usize) -> String {
    let mut text = String::new();
    ADMISSION.prometheus(&mut text);
    caches_prometheus(&mut text);
    MEDIA.prometheus(&mut text);
    queues_prometheus(&mut text);
    header(
        &mut text,
        "komika_queue_depth",
        "Current depth of a bounded request-path queue (deferred writes: queued, not running)",
        "gauge",
    );
    let _ = writeln!(
        text,
        "komika_queue_depth{{queue=\"deferred_writes\"}} {deferred_depth}"
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_permits_return_to_zero_and_keep_their_high_water() {
        let stats = AdmissionStats::leak();
        let lane = stats.lane(Lane::Local);
        let first = lane.admit();
        let second = lane.admit();
        assert_eq!(lane.snapshot(), (2, 0, 2, 2));
        drop((first, second));
        lane.refuse();
        assert_eq!(lane.snapshot(), (2, 1, 0, 2));
    }

    #[test]
    fn media_outcome_follows_status_unless_marked() {
        let stats = MediaStats::leak();
        let mut response = Response::new(axum::body::Body::from(vec![0u8; 10]));
        record_media_response(stats, MediaRoute::Cover, &response, false);
        mark(&mut response, MediaOutcome::NoStoreFallback);
        record_media_response(stats, MediaRoute::Cover, &response, true);
        *response.status_mut() = StatusCode::BAD_GATEWAY;
        response.extensions_mut().clear();
        record_media_response(stats, MediaRoute::Cover, &response, false);
        assert_eq!(stats.responses(MediaRoute::Cover, MediaOutcome::Ok), 1);
        assert_eq!(
            stats.responses(MediaRoute::Cover, MediaOutcome::NoStoreFallback),
            1
        );
        assert_eq!(
            stats.responses(MediaRoute::Cover, MediaOutcome::UpstreamError),
            1
        );
        assert_eq!(
            stats.body_bytes(MediaRoute::Cover),
            20,
            "HEAD sends no body"
        );
    }

    #[test]
    fn every_metric_has_help_and_type_and_counters_end_in_total() {
        let text = prometheus(3);
        let mut typed = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').unwrap();
                assert!(
                    typed.insert(name.to_string(), kind.to_string()).is_none(),
                    "{name}"
                );
                assert!(
                    text.contains(&format!("# HELP {name} ")),
                    "{name} has no HELP"
                );
                if kind == "counter" {
                    assert!(name.ends_with("_total"), "{name}");
                }
            }
        }
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            assert!(typed.contains_key(name), "{name} has no TYPE");
        }
        assert!(text.contains("komika_queue_depth{queue=\"deferred_writes\"} 3"));
    }
}
