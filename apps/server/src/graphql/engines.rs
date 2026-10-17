use super::{gql_read_err, mutation_write_error, require_admin, state};
use async_graphql::{Context, Enum, Error, Result, SimpleObject};

use crate::engine_route::{self, CounterSnapshot, Decision, Engines, Route, VersionCheck};

#[derive(Clone, Copy, Enum, Eq, PartialEq)]
pub(super) enum EngineVersionCheck {
    Match,
    OwnerUnknown,
    NotInstalled,
    Differs,
}

impl From<VersionCheck> for EngineVersionCheck {
    fn from(check: VersionCheck) -> Self {
        match check {
            VersionCheck::Match => Self::Match,
            VersionCheck::OwnerUnknown => Self::OwnerUnknown,
            VersionCheck::NotInstalled => Self::NotInstalled,
            VersionCheck::Differs { .. } => Self::Differs,
        }
    }
}

#[derive(SimpleObject)]
pub(super) struct EngineCounters {
    lookups: String,
    source_requests: String,
    page_fetches: String,
    page_bytes: String,
    cover_fetches: String,
    failures: String,
    fallbacks: String,
}

impl From<CounterSnapshot> for EngineCounters {
    fn from(c: CounterSnapshot) -> Self {
        // Counters can exceed GraphQL Int and JavaScript's exact integer range.
        Self {
            lookups: c.lookups.to_string(),
            source_requests: c.source_requests.to_string(),
            page_fetches: c.page_fetches.to_string(),
            page_bytes: c.page_bytes.to_string(),
            cover_fetches: c.cover_fetches.to_string(),
            failures: c.failures.to_string(),
            fallbacks: c.fallbacks.to_string(),
        }
    }
}

#[derive(SimpleObject)]
pub(super) struct RoutedSourceStatus {
    source_id: String,
    strict: bool,
    version_check: EngineVersionCheck,
    owner_version: Option<String>,
    regional_version: Option<String>,
    active: bool,
}

#[derive(SimpleObject)]
pub(super) struct EngineStatus {
    id: String,
    reachable: bool,
    checked_at: Option<String>,
    error: Option<String>,
    counters: EngineCounters,
    routed_sources: Vec<RoutedSourceStatus>,
}

#[derive(SimpleObject)]
pub(super) struct SourceEngineRoute {
    source_id: String,
    engine_id: String,
    strict: bool,
    active: bool,
    version_check: EngineVersionCheck,
}

fn routed_source(engines: &Engines, source: &str, route: &Route) -> RoutedSourceStatus {
    let health = engines.engine(&route.engine_id).and_then(|e| e.health());
    let regional = health.as_ref().and_then(|h| h.installed.get(source));
    RoutedSourceStatus {
        source_id: source.to_string(),
        strict: route.strict,
        version_check: engine_route::compare_versions(route.owner_extension.as_ref(), regional)
            .into(),
        owner_version: route
            .owner_extension
            .as_ref()
            .and_then(|e| e.version_code)
            .map(|v| v.to_string()),
        regional_version: regional.and_then(|e| e.version_code).map(|v| v.to_string()),
        active: matches!(engines.decide(source), Decision::Regional { .. }),
    }
}

fn source_route(engines: &Engines, source: &str) -> Option<SourceEngineRoute> {
    let route = engines.route(source)?;
    let status = routed_source(engines, source, &route);
    Some(SourceEngineRoute {
        source_id: status.source_id,
        engine_id: route.engine_id,
        strict: status.strict,
        active: status.active,
        version_check: status.version_check,
    })
}

pub(super) async fn engines(ctx: &Context<'_>) -> Result<Vec<EngineStatus>> {
    require_admin(ctx).await?;
    let st = state(ctx);
    let reachable = crate::engine_generation::commit_ready(&st.pool).is_ok()
        && st
            .suwayomi
            .fence
            .as_ref()
            .is_none_or(|f| f.generation().is_some());
    let mut out = vec![EngineStatus {
        id: engine_route::OWNER_ENGINE.into(),
        reachable,
        checked_at: None,
        error: (!reachable).then(|| "Owner engine is unavailable".into()),
        counters: CounterSnapshot::default().into(),
        routed_sources: Vec::new(),
    }];
    let engines = &st.suwayomi.regional;
    let routes = engines.routes();
    for engine in engines.engines() {
        let health = engine.health();
        let mut routed_sources: Vec<_> = routes
            .iter()
            .filter(|(_, r)| r.engine_id == engine.id)
            .map(|(source, route)| routed_source(engines, source, route))
            .collect();
        routed_sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        out.push(EngineStatus {
            id: engine.id.clone(),
            reachable: health.as_ref().is_some_and(|h| h.reachable),
            checked_at: health.as_ref().map(|h| h.checked_at.to_rfc3339()),
            error: health.and_then(|h| h.error),
            counters: engine.counters.snapshot().into(),
            routed_sources,
        });
    }
    Ok(out)
}

pub(super) async fn source_engine_route(
    ctx: &Context<'_>,
    source_id: String,
) -> Result<Option<SourceEngineRoute>> {
    require_admin(ctx).await?;
    Ok(source_route(&state(ctx).suwayomi.regional, &source_id))
}

pub(super) async fn set_source_engine(
    ctx: &Context<'_>,
    source_id: String,
    engine_id: Option<String>,
    strict: bool,
) -> Result<Option<SourceEngineRoute>> {
    let admin = require_admin(ctx).await?;
    if source_id.trim().is_empty() || source_id.chars().count() > 64 {
        return Err(Error::new("sourceId must contain 1-64 characters"));
    }
    let st = state(ctx);
    let engines = &st.suwayomi.regional;
    let old = engines.route(&source_id);
    let engine_id = engine_id
        .as_deref()
        .filter(|id| *id != engine_route::OWNER_ENGINE);
    if let Some(id) = engine_id {
        if let Some(reason) = engine_route::route_refusal(engines, &st.pool, &source_id, id)
            .await
            .map_err(gql_read_err)?
        {
            return Err(Error::new(reason));
        }
    }
    engine_route::store_route(
        &st.pool,
        &source_id,
        engine_id.map(|id| (id, strict)),
        Some(&admin.username),
    )
    .await
    .map_err(mutation_write_error)?;
    engines
        .reload_routes(&st.pool)
        .await
        .map_err(gql_read_err)?;
    for id in old
        .as_ref()
        .map(|r| r.engine_id.as_str())
        .into_iter()
        .chain(engine_id)
    {
        if let Some(engine) = engines.engine(id) {
            engine.clear();
        }
    }
    Ok(source_route(engines, &source_id))
}
