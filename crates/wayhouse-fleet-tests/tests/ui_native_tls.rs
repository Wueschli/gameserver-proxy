//! `wayhouse-ui --tls-cert/--tls-key`: the UI serves HTTPS itself and marks its
//! session cookie `Secure`.

use std::time::Duration;

use anyhow::{ensure, Context, Result};
use wayhouse_fleet_tests::tls_front::{TEST_CA, TEST_LEAF, TEST_LEAF_KEY};
use wayhouse_fleet_tests::{build_fleet_bins, free_port, spawn_ui_with, wait_until};

#[tokio::test]
async fn login_over_native_tls_sets_a_secure_cookie_that_works() -> Result<()> {
    build_fleet_bins()?;
    let port = free_port()?;
    let _ui = spawn_ui_with(
        port,
        &[
            "--tls-cert",
            TEST_LEAF,
            "--tls-key",
            TEST_LEAF_KEY,
            "--ui-password",
            "secret",
        ]
        .map(String::from),
    )?;
    let ca = reqwest::Certificate::from_pem(&std::fs::read(TEST_CA)?)?;
    let client = wayhouse_http::builder()
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(10))
        .build()?;
    let base = format!("https://localhost:{port}");
    wait_until(
        || {
            let (client, url) = (client.clone(), format!("{base}/healthz"));
            async move { Ok(client.get(url).send().await.is_ok()) }
        },
        Duration::from_secs(30),
        "the UI to answer over HTTPS",
    )
    .await?;

    let anonymous = client.get(format!("{base}/ui/session")).send().await?;
    ensure!(
        anonymous.status() == reqwest::StatusCode::UNAUTHORIZED,
        "/ui/session without a cookie: {}",
        anonymous.status()
    );

    let login = client
        .post(format!("{base}/ui/login"))
        .json(&serde_json::json!({ "password": "secret" }))
        .send()
        .await?
        .error_for_status()?;
    let set_cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .context("login set no cookie")?
        .to_str()?
        .to_string();
    ensure!(
        set_cookie.split(';').any(|a| a.trim() == "Secure"),
        "session cookie not Secure: {set_cookie}"
    );

    let cookie = set_cookie.split(';').next().unwrap_or_default();
    let session = client
        .get(format!("{base}/ui/session"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await?;
    ensure!(
        session.status().is_success(),
        "the cookie did not unlock /ui/session: {}",
        session.status()
    );
    Ok(())
}
