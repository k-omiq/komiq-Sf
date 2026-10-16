//! Public, viewer-independent origins for exact media objects.

use std::collections::HashSet;

#[derive(async_graphql::Enum, Copy, Clone, Eq, PartialEq, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaRouteKind {
    Owner,
    Regional,
}

#[derive(async_graphql::SimpleObject, Clone, Debug, PartialEq)]
pub struct MediaRoute {
    pub id: String,
    /// An origin (`scheme://host[:port]`, no path), or "" for the server's own
    /// origin (the same origin as its API).
    pub base: String,
    pub kind: MediaRouteKind,
    pub region: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MediaRoutes(pub(crate) Vec<MediaRoute>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteConfig {
    id: String,
    kind: MediaRouteKind,
    #[serde(default)]
    base: String,
    region: Option<String>,
}

impl MediaRoutes {
    pub fn owner_only() -> Self {
        Self(vec![MediaRoute {
            id: "owner".into(),
            base: String::new(),
            kind: MediaRouteKind::Owner,
            region: None,
        }])
    }

    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return Ok(Self::owner_only());
        };
        let configs: Vec<RouteConfig> = serde_json::from_str(raw)
            .map_err(|error| format!("KOMIKA_MEDIA_ROUTES: invalid JSON route array: {error}"))?;
        if !(1..=16).contains(&configs.len()) {
            return Err("KOMIKA_MEDIA_ROUTES must contain 1 to 16 entries".into());
        }
        let mut ids = HashSet::new();
        let mut owners = 0;
        let mut routes = Vec::with_capacity(configs.len());
        for config in configs {
            if !(1..=64).contains(&config.id.len())
                || !config
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(
                    "KOMIKA_MEDIA_ROUTES id must be 1 to 64 characters from [A-Za-z0-9_-]".into(),
                );
            }
            if !ids.insert(config.id.clone()) {
                return Err(format!(
                    "KOMIKA_MEDIA_ROUTES has duplicate id {:?}",
                    config.id
                ));
            }
            if config.kind == MediaRouteKind::Owner {
                owners += 1;
            } else if config.base.is_empty() {
                return Err("KOMIKA_MEDIA_ROUTES regional entries need a non-empty base".into());
            }
            if let Some(region) = &config.region {
                if !(1..=64).contains(&region.chars().count())
                    || region.chars().any(char::is_control)
                {
                    return Err("KOMIKA_MEDIA_ROUTES region must be 1 to 64 characters without control characters".into());
                }
            }
            let base = if config.base.is_empty() {
                String::new()
            } else {
                let invalid_base = || {
                    "KOMIKA_MEDIA_ROUTES base must be an absolute http(s) origin with a host and no credentials, path, query or fragment".to_string()
                };
                let url = reqwest::Url::parse(&config.base).map_err(|_| invalid_base())?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.has_host()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || !matches!(url.path(), "" | "/")
                {
                    return Err(invalid_base());
                }
                url.origin().ascii_serialization()
            };
            routes.push(MediaRoute {
                id: config.id,
                base,
                kind: config.kind,
                region: config.region,
            });
        }
        if owners != 1 {
            return Err("KOMIKA_MEDIA_ROUTES must contain exactly one owner".into());
        }
        Ok(Self(routes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_graphql::{EmptySubscription, Request, Schema};
    use serde_json::json;

    fn assert_invalid(raw: &str, message: &str) {
        let error = MediaRoutes::parse(Some(raw)).unwrap_err();
        assert!(error.contains("KOMIKA_MEDIA_ROUTES"), "{raw}: {error}");
        assert!(error.contains(message), "{raw}: {error}");
    }

    #[test]
    fn defaults_to_same_origin_owner() {
        let expected = vec![MediaRoute {
            id: "owner".into(),
            base: String::new(),
            kind: MediaRouteKind::Owner,
            region: None,
        }];
        assert_eq!(MediaRoutes::owner_only().0, expected);
        for raw in [None, Some(""), Some("  ")] {
            assert_eq!(MediaRoutes::parse(raw).unwrap().0, expected);
        }
    }

    #[test]
    fn normalizes_configured_origins() {
        let routes = MediaRoutes::parse(Some(
            r#"[
            {"id":"owner","kind":"owner","region":null},
            {"id":"sg","kind":"regional","base":"https://Media-SG.Example.com/","region":"SG"},
            {"id":"us","kind":"regional","base":"HTTPS://Media-US.Example.com:443"},
            {"id":"eu","kind":"regional","base":"https://Media-EU.Example.com:8443/"}
        ]"#,
        ))
        .unwrap()
        .0;
        assert_eq!(routes[0], MediaRoutes::owner_only().0[0]);
        assert_eq!(routes[1].base, "https://media-sg.example.com");
        assert_eq!(routes[1].kind, MediaRouteKind::Regional);
        assert_eq!(routes[1].region.as_deref(), Some("SG"));
        assert_eq!(routes[2].base, "https://media-us.example.com");
        assert_eq!(routes[2].region, None);
        assert_eq!(routes[3].base, "https://media-eu.example.com:8443");
    }

    #[test]
    fn owner_can_have_an_explicit_http_origin() {
        let routes = MediaRoutes::parse(Some(
            r#"[{"id":"primary","kind":"owner","base":"http://LOCALHOST:80/"}]"#,
        ))
        .unwrap();
        assert_eq!(routes.0[0].base, "http://localhost");
    }

    #[test]
    fn accepts_maximum_entry_id_and_unicode_region_lengths() {
        let mut configs = vec![json!({"id":"owner","kind":"owner"})];
        for index in 1..16 {
            configs.push(json!({
                "id": format!("{index:0>62}_-"),
                "kind": "regional",
                "base": "https://media.example.com",
                "region": "界".repeat(64),
            }));
        }
        assert_eq!(
            MediaRoutes::parse(Some(&configs_to_json(&configs)))
                .unwrap()
                .0
                .len(),
            16
        );
    }

    fn configs_to_json(configs: &[serde_json::Value]) -> String {
        serde_json::to_string(configs).unwrap()
    }

    #[test]
    fn rejects_invalid_json_or_non_arrays() {
        for raw in ["not JSON", "{}", "null", "42", r#""routes""#] {
            assert_invalid(raw, "JSON route array");
        }
    }

    #[test]
    fn rejects_empty_array() {
        assert_invalid("[]", "1 to 16 entries");
    }

    #[test]
    fn rejects_seventeen_entries() {
        let mut configs = vec![json!({"id":"owner","kind":"owner"})];
        for index in 1..17 {
            configs.push(json!({"id":format!("r{index}"),"kind":"regional","base":"https://media.example.com"}));
        }
        assert_invalid(&configs_to_json(&configs), "1 to 16 entries");
    }

    #[test]
    fn rejects_missing_owner() {
        assert_invalid(
            r#"[{"id":"sg","kind":"regional","base":"https://media.example.com"}]"#,
            "exactly one owner",
        );
    }

    #[test]
    fn rejects_two_owners() {
        assert_invalid(
            r#"[{"id":"a","kind":"owner"},{"id":"b","kind":"owner"}]"#,
            "exactly one owner",
        );
    }

    #[test]
    fn rejects_duplicate_ids() {
        assert_invalid(
            r#"[{"id":"owner","kind":"owner"},{"id":"owner","kind":"regional","base":"https://media.example.com"}]"#,
            "duplicate id",
        );
    }

    #[test]
    fn rejects_invalid_ids() {
        for id in [
            "".to_string(),
            "a".repeat(65),
            "bad.id".into(),
            "bad id".into(),
            "界".into(),
        ] {
            let raw = json!([{"id":id,"kind":"owner"}]).to_string();
            assert_invalid(&raw, "id must be 1 to 64 characters");
        }
    }

    #[test]
    fn rejects_regional_without_a_base() {
        for base in [None, Some("")] {
            let mut regional = json!({"id":"sg","kind":"regional"});
            if let Some(base) = base {
                regional["base"] = json!(base);
            }
            let raw = json!([{"id":"owner","kind":"owner"}, regional]).to_string();
            assert_invalid(&raw, "regional entries need a non-empty base");
        }
    }

    #[test]
    fn rejects_invalid_bases() {
        for base in [
            "https://media.example.com/path",
            "https://media.example.com/?query=1",
            "https://media.example.com/?",
            "https://media.example.com/#fragment",
            "https://media.example.com/#",
            "https://user:password@media.example.com/",
            "https://user@media.example.com/",
            "https://:password@media.example.com/",
            "ftp://media.example.com/",
            "/relative",
            "https://",
            " ",
        ] {
            let raw = json!([{"id":"owner","kind":"owner","base":base}]).to_string();
            assert_invalid(&raw, "base must be an absolute http(s) origin");
        }
    }

    #[test]
    fn rejects_invalid_regions() {
        for region in [
            String::new(),
            "a".repeat(65),
            "界".repeat(65),
            "SG\n".into(),
            "SG\u{7f}".into(),
        ] {
            let raw = json!([{"id":"owner","kind":"owner","region":region}]).to_string();
            assert_invalid(
                &raw,
                "region must be 1 to 64 characters without control characters",
            );
        }
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_field_types() {
        for raw in [
            r#"[{"id":"owner","kind":"owner","unexpected":true}]"#,
            r#"[{"id":"owner","kind":"OWNER"}]"#,
            r#"[{"id":"owner","kind":"other"}]"#,
            r#"[{"id":"owner","kind":"owner","base":null}]"#,
            r#"[{"id":"owner","kind":"owner","region":42}]"#,
            r#"[{"kind":"owner"}]"#,
            r#"[{"id":"owner"}]"#,
        ] {
            assert_invalid(raw, "JSON route array");
        }
    }

    const QUERY: &str = "{ mediaRoutes { id base kind region } }";

    async fn assert_response(schema: &crate::graphql::ApiSchema, expected: serde_json::Value) {
        // No AppState or database is attached; even a token must not require one.
        for token in [None, Some("unused-token".to_string())] {
            let request = Request::new(QUERY).data(crate::graphql::RequestAuth(token));
            let response = schema.execute(request).await;
            assert!(response.errors.is_empty(), "{:?}", response.errors);
            assert!(response.cache_control.public);
            assert_ne!(
                response.cache_control.max_age, -1,
                "mediaRoutes must remain cacheable"
            );
            assert_eq!(response.data.into_json().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn schema_defaults_without_media_routes_data() {
        let schema = Schema::build(
            crate::graphql::QueryRoot,
            crate::graphql::MutationRoot,
            EmptySubscription,
        )
        .finish();
        assert_response(
            &schema,
            json!({"mediaRoutes":[
                {"id":"owner","base":"","kind":"OWNER","region":null}
            ]}),
        )
        .await;
    }

    #[tokio::test]
    async fn schema_returns_configured_media_routes() {
        let routes = MediaRoutes::parse(Some(
            r#"[
            {"id":"primary","kind":"owner","base":"https://Owner.Example.com/"},
            {"id":"sg","kind":"regional","base":"https://Media-SG.Example.com:443/","region":"SG"}
        ]"#,
        ))
        .unwrap();
        let schema = Schema::build(
            crate::graphql::QueryRoot,
            crate::graphql::MutationRoot,
            EmptySubscription,
        )
        .data(routes)
        .finish();
        assert_response(
            &schema,
            json!({"mediaRoutes":[
                {"id":"primary","base":"https://owner.example.com","kind":"OWNER","region":null},
                {"id":"sg","base":"https://media-sg.example.com","kind":"REGIONAL","region":"SG"}
            ]}),
        )
        .await;
    }
}
