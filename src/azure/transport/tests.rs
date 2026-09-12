use super::fake::{Answer, client};
use super::*;
use serde_json::json;
use time::macros::datetime;

#[test]
fn a_spent_token_is_minted_once_more_and_the_call_retried() {
    let (client, transport, _) = client([
        Answer::status(401, r#"{"error":{"code":"ExpiredAuthenticationToken"}}"#),
        Answer::json(json!({"ok": true})),
    ]);
    let answer = client
        .call(&Audience::Arm, Request::get("https://example/one"))
        .unwrap();
    assert_eq!(answer["ok"], json!(true));
    let bearers = transport.bearers();
    assert_eq!(bearers.len(), 2);
    assert_ne!(bearers[0], bearers[1], "the retry carries a fresh token");
}

#[test]
fn a_second_refusal_is_a_signed_out_login() {
    let (client, _, _) = client([
        Answer::status(401, r#"{"error":{"message":"expired"}}"#),
        Answer::status(401, r#"{"error":{"message":"expired"}}"#),
    ]);
    let error = client
        .call(&Audience::Arm, Request::get("https://example/one"))
        .unwrap_err();
    assert!(is_signed_out(&error), "{error:#}");
    assert!(format!("{error:#}").contains("expired"), "{error:#}");
}

#[test]
fn a_throttle_waits_the_header_out_and_asks_once_more() {
    let (client, transport, waits) = client([
        Answer::status(429, r#"{"error":{"message":"slow down"}}"#).with_header("Retry-After", "2"),
        Answer::json(json!({"ok": true})),
    ]);
    client
        .call(&Audience::Arm, Request::get("https://example/one"))
        .unwrap();
    assert_eq!(waits.taken(), [Duration::from_secs(2)]);
    assert_eq!(transport.sent().len(), 2);
    assert_eq!(client.last_throttle(), Some(Duration::from_secs(2)));
    assert_eq!(client.last_throttle(), None, "reading it clears it");
}

#[test]
fn a_throttle_that_does_not_lift_is_an_error_rather_than_a_loop() {
    let (client, transport, _) = client([
        Answer::status(429, "{}").with_header("Retry-After", "1"),
        Answer::status(429, "{}").with_header("Retry-After", "1"),
    ]);
    let error = client
        .call(&Audience::Arm, Request::get("https://example/one"))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("still throttling"),
        "{error:#}"
    );
    assert_eq!(transport.sent().len(), 2, "one wait, not a loop");
}

#[test]
fn a_token_is_minted_once_and_then_reused() {
    let (client, _, _) = client([Answer::json(json!({})), Answer::json(json!({}))]);
    client
        .call(&Audience::Arm, Request::get("https://example/one"))
        .unwrap();
    client
        .call(&Audience::Arm, Request::get("https://example/two"))
        .unwrap();
    // Two calls, one mint: the second reads the cache.
    assert_eq!(client.cached.borrow().len(), 1);
}

#[test]
fn retry_after_reads_seconds_a_date_and_nothing_at_all() {
    let now = datetime!(2026-09-11 20:00:00 UTC);
    assert_eq!(retry_after(Some("2"), now), Duration::from_secs(2));
    assert_eq!(retry_after(None, now), DEFAULT_RETRY_AFTER);
    assert_eq!(retry_after(Some("  "), now), DEFAULT_RETRY_AFTER);
    assert_eq!(retry_after(Some("nan"), now), DEFAULT_RETRY_AFTER);
    assert_eq!(retry_after(Some("banana"), now), DEFAULT_RETRY_AFTER);
    assert_eq!(
        retry_after(Some("0"), now),
        MIN_RETRY_AFTER,
        "never nothing"
    );
    assert_eq!(
        retry_after(Some("7200"), now),
        MAX_RETRY_AFTER,
        "never days"
    );
    assert_eq!(
        retry_after(Some("Fri, 11 Sep 2026 20:00:45 GMT"), now),
        Duration::from_secs(45),
        "an HTTP date counts forward from now"
    );
    assert_eq!(
        retry_after(Some("Fri, 11 Sep 2026 19:00:00 GMT"), now),
        MIN_RETRY_AFTER,
        "a date already past is still worth a breath"
    );
}

#[test]
fn resource_graphs_quota_clock_is_read_when_there_is_no_retry_after() {
    let now = datetime!(2026-09-11 20:00:00 UTC);
    let response = |headers: Vec<(&str, &str)>| Response {
        status: 429,
        headers: headers
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect(),
        body: String::new(),
    };
    assert_eq!(
        throttle_wait(&response(vec![("Retry-After", "12")]), now),
        Duration::from_secs(12)
    );
    assert_eq!(
        throttle_wait(
            &response(vec![("x-ms-user-quota-resets-after", "00:00:04")]),
            now
        ),
        Duration::from_secs(4),
        "Resource Graph writes a clock, not a count of seconds"
    );
    assert_eq!(
        throttle_wait(
            &response(vec![("x-ms-user-quota-resets-after", "01:02:03")]),
            now
        ),
        MAX_RETRY_AFTER,
        "a clock past the cap is still capped"
    );
    assert_eq!(hms_seconds("01:02:03"), Some(3723));
    assert_eq!(hms_seconds("02:30"), Some(150), "a two-part clock is mm:ss");
    assert_eq!(
        throttle_wait(&response(vec![]), now),
        Duration::from_secs(30),
        "Key Vault says nothing, so the default applies"
    );
    assert_eq!(hms_seconds("nope"), None);
}

#[test]
fn a_refusal_is_read_in_whichever_shape_the_plane_wrote_it() {
    assert_eq!(
        failure_message(r#"{"error":{"code":"Forbidden","message":"no access policy"}}"#),
        "no access policy",
        "ARM and Key Vault"
    );
    assert_eq!(
        failure_message(
            r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#
        ),
        "authentication required",
        "a container registry"
    );
    assert_eq!(
        failure_message(
            r#"{"error":{"message":"see the correlation id","details":[{"message":"the subscription is disabled"}]}}"#
        ),
        "see the correlation id \u{2014} the subscription is disabled"
    );
    assert_eq!(failure_message("<html>nope</html>"), "<html>nope</html>");
    assert_eq!(failure_message(&"x".repeat(400)).len(), 200);
}

#[test]
fn the_codes_a_refusal_carries_include_the_inner_one() {
    let body = r#"{"error":{"code":"Forbidden","message":"…","innererror":{"code":"ForbiddenByFirewall"}}}"#;
    assert_eq!(failure_codes(body), ["Forbidden", "ForbiddenByFirewall"]);
    assert_eq!(
        failure_codes(r#"{"errors":[{"code":"DENIED"}]}"#),
        ["DENIED"]
    );
    assert!(failure_codes("nope").is_empty());
}

#[test]
fn a_form_body_is_encoded_the_way_a_token_endpoint_wants_it() {
    let encoded = form_encode(&[
        ("grant_type".to_owned(), "access_token".to_owned()),
        ("service".to_owned(), "acr.azurecr.io".to_owned()),
        (
            "scope".to_owned(),
            "repository:team/api:metadata_read".to_owned(),
        ),
    ]);
    assert_eq!(
        encoded,
        "grant_type=access_token&service=acr.azurecr.io&scope=repository%3Ateam%2Fapi%3Ametadata_read"
    );
}

#[test]
fn a_2xx_that_is_not_json_names_the_url() {
    let (client, _, _) = client([Answer::ok("<html>hello</html>")]);
    let error = client
        .call(&Audience::Arm, Request::get("https://example/one"))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("https://example/one"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("other than JSON"),
        "{error:#}"
    );
}

#[test]
fn a_plain_refusal_carries_the_status_and_the_message() {
    let (client, _, _) = client([Answer::status(
        403,
        r#"{"error":{"message":"caller is not authorized"}}"#,
    )]);
    let error = client
        .call(&Audience::Vault, Request::get("https://kv/secrets"))
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("403"), "{message}");
    assert!(message.contains("caller is not authorized"), "{message}");
    assert!(!is_signed_out(&error), "a 403 is a permission, not a login");
}
