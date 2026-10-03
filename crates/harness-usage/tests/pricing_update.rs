//! 1.5: `harness pricing update` is the only fetch, validates what it gets, and leaves the
//! previous table when anything is wrong.

use harness_usage::pricing::{Pricing, Source, update};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const API: &str = r#"{"openai":{"models":{"gpt-5":{"cost":{"input":1.5,"output":9}},"o3":{"cost":{"input":2,"output":8}}}}}"#;

async fn serve(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn an_update_writes_the_table_and_reports_its_date_and_size() {
    let server = serve(200, API).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pricing.json");
    let summary = update(&format!("{}/api.json", server.uri()), &dest, "2026-10-03")
        .await
        .unwrap();
    assert_eq!(summary.date, "2026-10-03");
    assert_eq!(summary.models, 2);
    let pricing = Pricing::load(&dest, vec![]);
    let (price, source) = pricing.price_of("openai/gpt-5").unwrap();
    assert_eq!(price.input, Some(1.5));
    assert_eq!(source, Source::Downloaded("2026-10-03".into()));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn an_update_that_gets_something_else_keeps_the_old_table() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pricing.json");
    std::fs::write(&dest, "old table").unwrap();
    for (status, body) in [
        (200, "<html>not pricing</html>"),
        (200, "{}"),
        (500, "oops"),
        (404, ""),
    ] {
        let server = serve(status, body).await;
        let err = update(&format!("{}/api.json", server.uri()), &dest, "2026-10-03")
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "old table",
            "{status} {body}"
        );
    }
}

#[tokio::test]
async fn an_update_that_cannot_connect_says_why_and_keeps_the_old_table() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pricing.json");
    std::fs::write(&dest, "old table").unwrap();
    let err = update("http://127.0.0.1:9/api.json", &dest, "2026-10-03")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("models.dev") || err.to_string().contains("127.0.0.1"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old table");
}

#[tokio::test]
async fn the_data_is_fetched_over_https_only() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pricing.json");
    let err = update("http://example.com/api.json", &dest, "2026-10-03")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("https"), "{err}");
    assert!(!dest.exists());
}
