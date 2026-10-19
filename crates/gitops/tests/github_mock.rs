//! GitHub providers against a local wiremock server (no network).

use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use kiln_gitops::*;
use serde::Deserialize;
use serde_json::json;
use wiremock::matchers::{bearer_token, body_partial_json, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const KEY: &[u8] = include_bytes!("fixtures/test_app_key.pem");
const PUB: &[u8] = include_bytes!("fixtures/test_app_key.pub.pem");
const APP_ID: u64 = 12345;
const REPO: &str = "https://github.com/acme/web.git";

#[derive(Deserialize)]
struct Claims {
    iss: String,
    iat: i64,
    exp: i64,
}

fn decode(jwt: &str) -> Claims {
    let mut v = Validation::new(Algorithm::RS256);
    v.set_issuer(&[APP_ID.to_string()]);
    jsonwebtoken::decode::<Claims>(jwt, &DecodingKey::from_rsa_pem(PUB).unwrap(), &v)
        .unwrap()
        .claims
}

/// Matches requests whose bearer token is a valid App JWT for `APP_ID`.
struct AppJwt;
impl Match for AppJwt {
    fn matches(&self, req: &Request) -> bool {
        let Some(h) = req
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let Some(jwt) = h.strip_prefix("Bearer ") else {
            return false;
        };
        let mut v = Validation::new(Algorithm::RS256);
        v.set_issuer(&[APP_ID.to_string()]);
        jsonwebtoken::decode::<Claims>(jwt, &DecodingKey::from_rsa_pem(PUB).unwrap(), &v).is_ok()
    }
}

fn app(server: &MockServer) -> GitHubApp {
    GitHubApp::new(APP_ID, KEY)
        .unwrap()
        .with_api_base(&server.uri())
}

async fn mount_installation(server: &MockServer, times: u64) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/installation"))
        .and(AppJwt)
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": 77, "account": { "login": "acme" } })),
        )
        .expect(times)
        .mount(server)
        .await;
}

async fn mount_token(server: &MockServer, token: &str, ttl: Duration, times: u64) {
    Mock::given(method("POST"))
        .and(path("/app/installations/77/access_tokens"))
        .and(AppJwt)
        .and(body_partial_json(json!({ "repositories": ["web"] })))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({ "token": token, "expires_at": (Utc::now() + ttl).to_rfc3339() }),
        ))
        .expect(times)
        .mount(server)
        .await;
}

#[test]
fn app_jwt_is_rs256_with_skewed_iat() {
    let app = GitHubApp::new(APP_ID, KEY).unwrap();
    let c = decode(&app.app_jwt().unwrap());
    assert_eq!(c.iss, "12345");
    assert_eq!(c.exp - c.iat, 600);
    assert!(c.iat <= Utc::now().timestamp() - 59);
    assert!(GitHubApp::new(APP_ID, b"not a pem").is_err());
    assert!(GitHubApp::new(0, KEY).is_err());
    assert!(!format!("{app:?}").contains("PRIVATE"));
}

#[tokio::test]
async fn installation_token_is_cached() {
    let server = MockServer::start().await;
    mount_installation(&server, 1).await;
    mount_token(&server, "ghs_cached", Duration::hours(1), 1).await;
    let app = app(&server);
    assert_eq!(
        app.git_auth(REPO).await.unwrap(),
        GitAuth::token("ghs_cached")
    );
    assert_eq!(
        app.git_auth("git@github.com:acme/web.git").await.unwrap(),
        GitAuth::token("ghs_cached")
    );
    assert_eq!(app.installation_token(REPO).await.unwrap(), "ghs_cached");
    // `expect(1)` on both mocks is verified when the server drops.
}

#[tokio::test]
async fn token_refreshed_inside_expiry_margin() {
    let server = MockServer::start().await;
    mount_installation(&server, 1).await;
    // Expires in 2 minutes: inside the 5 minute margin, so never served from cache.
    mount_token(&server, "ghs_short", Duration::minutes(2), 2).await;
    let app = app(&server);
    app.installation_token(REPO).await.unwrap();
    app.installation_token(REPO).await.unwrap();
}

#[tokio::test]
async fn installation_found_by_scanning_when_repo_lookup_404s() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/installation"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/app/installations"))
        .and(query_param("page", "1"))
        .and(AppJwt)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 5, "account": { "login": "other" } },
            { "id": 77, "account": { "login": "ACME" } }
        ])))
        .mount(&server)
        .await;
    mount_token(&server, "ghs_scan", Duration::hours(1), 1).await;
    let app = app(&server);
    assert_eq!(app.installation_id("acme", "web").await.unwrap(), 77);
    assert_eq!(app.installation_token(REPO).await.unwrap(), "ghs_scan");
}

#[tokio::test]
async fn app_opens_pr_and_reads_state() {
    let server = MockServer::start().await;
    mount_installation(&server, 1).await;
    mount_token(&server, "ghs_pr", Duration::hours(1), 1).await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/web/pulls"))
        .and(bearer_token("ghs_pr"))
        .and(body_partial_json(json!({ "head": "kiln/fp-1-x", "base": "main", "title": "FP-1: X", "draft": false })))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({ "number": 42, "html_url": "https://github.com/acme/web/pull/42", "state": "open" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls/42"))
        .and(bearer_token("ghs_pr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 42, "html_url": "https://github.com/acme/web/pull/42", "state": "closed", "merged": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls/43"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 43, "html_url": "https://github.com/acme/web/pull/43", "state": "closed", "merged": false
        })))
        .mount(&server)
        .await;

    let app = app(&server);
    let provider: &dyn PrProvider = &app;
    assert_eq!(provider.id(), "github_app");
    let pr = provider
        .open_pr(&NewPr {
            repo_url: REPO.into(),
            base: "main".into(),
            head: "kiln/fp-1-x".into(),
            title: "FP-1: X".into(),
            body: "body".into(),
            draft: false,
        })
        .await
        .unwrap();
    assert_eq!(
        pr,
        PrRef {
            number: 42,
            url: "https://github.com/acme/web/pull/42".into()
        }
    );
    assert_eq!(provider.pr_state(REPO, 42).await.unwrap(), PrState::Merged);
    assert_eq!(provider.pr_state(REPO, 43).await.unwrap(), PrState::Closed);
}

#[tokio::test]
async fn token_provider_reuses_existing_pr_on_422() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/web/pulls"))
        .and(bearer_token("ghp_dev"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{ "message": "A pull request already exists for acme:kiln/fp-2-y." }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls"))
        .and(query_param("head", "acme:kiln/fp-2-y"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 7, "html_url": "https://github.com/acme/web/pull/7", "state": "open" }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "html_url": "https://github.com/acme/web/pull/7", "state": "open", "merged": false
        })))
        .mount(&server)
        .await;

    let gh = GitHubToken::new("ghp_dev").with_api_base(&server.uri());
    let pr = gh
        .open_pr(&NewPr {
            repo_url: "git@github.com:acme/web.git".into(),
            base: "main".into(),
            head: "kiln/fp-2-y".into(),
            title: "t".into(),
            body: "b".into(),
            draft: true,
        })
        .await
        .unwrap();
    assert_eq!(pr.number, 7);
    assert_eq!(gh.pr_state(REPO, 7).await.unwrap(), PrState::Open);
    assert_eq!(gh.git_auth(REPO).await.unwrap(), GitAuth::token("ghp_dev"));
    assert!(matches!(
        gh.pr_state(REPO, 8).await,
        Err(GitError::NotFound(_))
    ));
}

#[tokio::test]
async fn api_errors_and_bad_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/web/pulls"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({ "message": "Resource not accessible" })),
        )
        .mount(&server)
        .await;
    let gh = GitHubToken::new("ghp_dev").with_api_base(&server.uri());
    let pr = NewPr {
        repo_url: REPO.into(),
        base: "main".into(),
        head: "kiln/x".into(),
        title: "t".into(),
        body: "b".into(),
        draft: false,
    };
    match gh.open_pr(&pr).await {
        Err(GitError::Http {
            status: 403,
            message,
            ..
        }) => assert_eq!(message, "Resource not accessible"),
        other => panic!("unexpected {other:?}"),
    }
    let mut local = pr.clone();
    local.repo_url = "/tmp/repo.git".into();
    assert!(matches!(
        gh.open_pr(&local).await,
        Err(GitError::InvalidUrl(_))
    ));
}
