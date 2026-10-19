//! Turnstile on the sensitive mutations (`crate::turnstile`): off changes nothing, on
//! refuses a missing or failed token with a code the apps show, and a passing token goes
//! to `siteverify` with the client address.
use super::tests::{exec, first_error, setup_with_state};
use super::*;
use crate::turnstile::test_support::{
    checker, DUMMY_TOKEN, FAILING_SECRET, PASSING_SECRET, PASSING_SITE_KEY,
};

const BOB: &str = "ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c";

/// Each sensitive mutation, with `{token}` where its token goes and the bearer it needs.
/// The password changes come last: they end bob's other sessions.
fn sensitive(token: &str) -> Vec<(&'static str, String, Option<&'static str>)> {
    let t = |text: &str| text.replace("{token}", token);
    vec![
        (
            "register",
            t(
                r#"mutation { register(input:{username:"newbie", email:"n@e.com", password:"password123"{token}}) { token } }"#,
            ),
            None,
        ),
        (
            "login",
            t(r#"mutation { login(username:"bob", password:"password123"{token}) { token } }"#),
            None,
        ),
        (
            "postComment",
            t(
                r#"mutation { postComment(input:{targetType:"series", targetId:"w_missing", body:"hi", hasSpoiler:false{token}}) { id } }"#,
            ),
            Some("bobtok"),
        ),
        (
            "postReview",
            t(
                r#"mutation { postReview(input:{seriesId:"w_missing", score:8, body:"", hasSpoiler:false{token}}) { id } }"#,
            ),
            Some("bobtok"),
        ),
        (
            "submitReport",
            t(
                r#"mutation { submitReport(input:{kind:OTHER, detail:"Something is wrong here."{token}}) { id } }"#,
            ),
            None,
        ),
        (
            "changePassword",
            t(
                r#"mutation { changePassword(currentPassword:"password123", newPassword:"password456"{token}) { id } }"#,
            ),
            Some("bobtok"),
        ),
        (
            "resetUserPassword",
            t(&format!(
                r#"mutation {{ resetUserPassword(userId:"{BOB}", password:"temporary123"{{token}}) }}"#
            )),
            Some("admintok"),
        ),
    ]
}

fn code(response: &async_graphql::Response) -> Option<String> {
    let error = response.errors.first()?;
    match error.extensions.as_ref()?.get("code")? {
        async_graphql::Value::String(code) => Some(code.clone()),
        _ => None,
    }
}

async fn site_key(schema: &ApiSchema) -> async_graphql::Value {
    let response = exec(schema, "{ instance { turnstileSiteKey } }", None, "1.1.1.1").await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let async_graphql::Value::Object(data) = response.data else {
        panic!("no data");
    };
    let async_graphql::Value::Object(instance) = data["instance"].clone() else {
        panic!("no instance");
    };
    instance["turnstileSiteKey"].clone()
}

#[tokio::test]
async fn off_needs_no_token_and_shows_no_site_key() {
    let (schema, _, _, _dir) = setup_with_state(100).await;
    assert_eq!(site_key(&schema).await, async_graphql::Value::Null);
    let response = exec(
        &schema,
        r#"mutation { login(username:"bob", password:"password123") { token } }"#,
        None,
        "1.1.1.1",
    )
    .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    // A token sent to a server with Turnstile off is accepted and ignored.
    let response = exec(
        &schema,
        &format!(r#"mutation {{ register(input:{{username:"newbie", email:"n@e.com", password:"password123", turnstileToken:"{DUMMY_TOKEN}"}}) {{ token }} }}"#),
        None,
        "1.1.1.1",
    )
    .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
}

#[tokio::test]
async fn on_refuses_every_sensitive_mutation_without_a_token() {
    let (_, pool, state, _dir) = setup_with_state(100).await;
    let (mock, check) = checker(PASSING_SECRET, std::time::Duration::from_secs(2)).await;
    let schema = schema_builder(state, false).data(check).finish();
    assert_eq!(
        site_key(&schema).await,
        async_graphql::Value::String(PASSING_SITE_KEY.into())
    );
    for (name, text, bearer) in sensitive("") {
        let response = exec(&schema, &text, bearer, "1.1.1.1").await;
        assert_eq!(
            code(&response).as_deref(),
            Some("TURNSTILE_REQUIRED"),
            "{name}: {:?}",
            response.errors
        );
        assert_eq!(
            first_error(&response),
            "Complete the security check, then try again.",
            "{name}"
        );
    }
    // Nothing was asked of Cloudflare, and nothing was written.
    assert!(mock.requests.lock().unwrap().is_empty());
    let users: i64 =
        crate::db_types::query_scalar("SELECT COUNT(*) FROM users WHERE username = 'newbie'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(users, 0);
    let reports: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM report")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reports, 0);
}

#[tokio::test]
async fn on_refuses_every_sensitive_mutation_with_a_failed_token() {
    let (_, _, state, _dir) = setup_with_state(100).await;
    let (mock, check) = checker(FAILING_SECRET, std::time::Duration::from_secs(2)).await;
    let schema = schema_builder(state, false).data(check).finish();
    let token = format!(r#", turnstileToken:"{DUMMY_TOKEN}""#);
    let all = sensitive(&token);
    for (name, text, bearer) in &all {
        let response = exec(&schema, text, *bearer, "1.1.1.1").await;
        assert_eq!(
            code(&response).as_deref(),
            Some("TURNSTILE_FAILED"),
            "{name}: {:?}",
            response.errors
        );
    }
    assert_eq!(mock.requests.lock().unwrap().len(), all.len());
}

#[tokio::test]
async fn on_accepts_a_passing_token_and_sends_the_client_address() {
    let (_, pool, state, _dir) = setup_with_state(100).await;
    let (mock, check) = checker(PASSING_SECRET, std::time::Duration::from_secs(2)).await;
    let schema = schema_builder(state, false).data(check).finish();
    let token = format!(r#", turnstileToken:"{DUMMY_TOKEN}""#);
    for (name, text, bearer) in sensitive(&token) {
        let response = exec(&schema, &text, bearer, "203.0.113.9").await;
        if matches!(name, "postComment" | "postReview") {
            // Their targets do not exist: past the check, the usual error.
            assert!(!response.errors.is_empty(), "{name}");
            assert!(
                !code(&response).unwrap_or_default().starts_with("TURNSTILE"),
                "{name}: {:?}",
                response.errors
            );
        } else {
            assert!(response.errors.is_empty(), "{name}: {:?}", response.errors);
        }
    }
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 7);
    for request in &requests {
        assert_eq!(request["response"], DUMMY_TOKEN);
        assert_eq!(request["remoteip"], "203.0.113.9");
    }
    let reports: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM report")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reports, 1);
}
