//! The container registry data plane: the catalog, one repository's
//! attributes, its tags newest first, and one manifest.
//!
//! ARM does not sign these calls. The registry mints its own tokens, from a
//! CLI token for the `containerregistry` audience, in two form posts — and
//! those two are the only place in the crate where a token is not simply
//! asked for. Everything after them goes through
//! [`Client::call`](super::transport::Client::call) like every other read, so
//! the 401 and 429 policy is written once.
//!
//! The endpoints are the `/acr/v1/` ones, which carry attributes. The `/v2/`
//! ones are the OCI distribution API and carry names only.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::auth::Audience;
use super::graph::{string, text};
use super::transport::{Client, Request, host_under, percent_encode};
use super::{Manifest, Registry, Repository, Tag};
use crate::timestamp::Timestamp;

/// What a catalog listing is signed for.
const CATALOG_SCOPE: &str = "registry:catalog:*";
/// How many entries a paged listing asks for at once. A short page is how the
/// end of a listing announces itself.
const PAGE: usize = 100;

/// A registry's access token for one scope.
///
/// Two posts, both `application/x-www-form-urlencoded`, neither signed with a
/// bearer: the first trades a CLI token for a refresh token that lives about
/// three hours and is cached per registry, the second trades that for an
/// access token scoped to the one thing about to be read.
///
/// Called from the client's own token cache and nowhere else, so a registry
/// read looks like every other read from above.
///
/// The login server comes from Resource Graph and the exchange hands it a
/// CLI token, so it has to be a registry's own host under `.azurecr.io`
/// before anything is sent.
pub(crate) fn mint(client: &Client, login_server: &str, scope: &str) -> Result<String> {
    if !host_under(login_server, ".azurecr.io") {
        bail!("{login_server:?} is not a registry login server; no token is sent there");
    }
    let refresh = client.registry_refresh_token(login_server, || exchange(client, login_server))?;
    let issued = client.call_unsigned(Request::post_form(
        format!("https://{login_server}/oauth2/token"),
        vec![
            ("grant_type".to_owned(), "refresh_token".to_owned()),
            ("service".to_owned(), login_server.to_owned()),
            ("scope".to_owned(), scope.to_owned()),
            ("refresh_token".to_owned(), refresh),
        ],
    ))?;
    text(&issued["access_token"])
        .with_context(|| format!("{login_server} answered the token call without a token"))
}

/// The first of the two posts: a CLI token in, a refresh token out. Once per
/// registry per run, however many scopes are asked for after it.
fn exchange(client: &Client, login_server: &str) -> Result<String> {
    let exchanged = match post_exchange(client, login_server) {
        // A 401 here is most often the CLI token gone stale in the cache,
        // which only a data-plane 401 would otherwise drop — and a registry
        // that never answered has had none. One fresh token, one retry.
        Err(error) if is_401(&error) => {
            client.forget(&Audience::ContainerRegistry);
            post_exchange(client, login_server)
        }
        first => first,
    };
    let exchanged = exchanged.map_err(|error| explain(login_server, error))?;
    text(&exchanged["refresh_token"])
        .with_context(|| format!("{login_server} answered the exchange without a token"))
}

/// A 401 on the post, as opposed to no login at all, which no re-mint fixes.
fn is_401(error: &anyhow::Error) -> bool {
    super::transport::is_signed_out(error) && !super::transport::is_no_login(error)
}

/// The exchange post itself, with whatever CLI token the cache holds.
fn post_exchange(client: &Client, login_server: &str) -> Result<Value> {
    let cli = client.token(&Audience::ContainerRegistry)?;
    let mut fields = vec![
        ("grant_type".to_owned(), "access_token".to_owned()),
        ("service".to_owned(), login_server.to_owned()),
        ("access_token".to_owned(), cli),
    ];
    // The swagger calls `tenant` optional and the prose calls it required;
    // `az acr` always sends it, so this does too when the CLI will say.
    if let Some(tenant) = client.tenant() {
        fields.push(("tenant".to_owned(), tenant));
    }
    client.call_unsigned(Request::post_form(
        format!("https://{login_server}/oauth2/exchange"),
        fields,
    ))
}

/// A refusal on the exchange itself is not a spent token: it is a login with
/// no role on this registry, which no retry fixes.
fn explain(login_server: &str, error: anyhow::Error) -> anyhow::Error {
    // No login at all is not a missing role: it goes through untouched, and
    // the worker reduces it to the two words that fix it.
    if super::transport::is_no_login(&error) {
        return error;
    }
    let refused = super::transport::api_error(&error)
        .is_some_and(|refusal| refusal.status == 401 || refusal.status == 403)
        || super::transport::is_signed_out(&error);
    if refused {
        return anyhow::anyhow!(
            "{login_server}: no permission (needs AcrPull, or Container Registry Repository Reader on an ABAC registry)"
        );
    }
    error
}

/// Every repository in one registry, by name. A catalog is names and nothing
/// else; the counts arrive from [`attributes`].
pub fn repositories(client: &Client, registry: &Registry) -> Result<Vec<String>> {
    let audience = Audience::Acr {
        login_server: registry.login_server.clone(),
        scope: CATALOG_SCOPE.to_owned(),
    };
    let mut names: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    loop {
        let mut url = format!("https://{}/acr/v1/_catalog?n={PAGE}", registry.login_server);
        if let Some(last) = &last {
            url.push_str("&last=");
            percent_encode(last, &mut url);
        }
        let page = client
            .call(&audience, Request::get(&url))
            .map_err(|error| explain(&registry.login_server, error))?;
        let listed: Vec<String> = page["repositories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect();
        let full = listed.len() >= PAGE;
        let previous = last.clone();
        last = listed.last().cloned();
        names.extend(listed);
        // A page that did not move the cursor on would be asked for for ever,
        // whatever it claimed to be full of.
        if !full || last.is_none() || last == previous {
            return Ok(names);
        }
    }
}

/// One repository's counts and stamps, which the catalog does not carry. One
/// call per repository, which is why the worker runs these as a low-priority
/// fill after the names are already on screen.
pub fn attributes(client: &Client, registry: &Registry, name: &str) -> Result<Repository> {
    let answer = client
        .call(
            &metadata_audience(registry, name),
            Request::get(format!(
                "https://{}/acr/v1/{}",
                registry.login_server,
                path(name)
            )),
        )
        .map_err(|error| explain(&registry.login_server, error))?;
    Ok(Repository {
        registry: registry.name.clone(),
        name: text(&answer["imageName"]).unwrap_or_else(|| name.to_owned()),
        tag_count: count(&answer["tagCount"]),
        manifest_count: count(&answer["manifestCount"]),
        created: stamp(&answer["createdTime"]),
        updated: stamp(&answer["lastUpdateTime"]),
    })
}

/// One repository's tags, in the order the registry gives them, which
/// `orderby=timedesc` makes newest first.
pub fn tags(client: &Client, registry: &Registry, repo: &str) -> Result<Vec<Tag>> {
    let audience = metadata_audience(registry, repo);
    let mut tags: Vec<Tag> = Vec::new();
    let mut last: Option<String> = None;
    loop {
        let page = client
            .call(
                &audience,
                Request::get(tags_url(registry, repo, last.as_deref())),
            )
            .map_err(|error| explain(&registry.login_server, error))?;
        let listed: Vec<Tag> = page["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(tag)
            .collect();
        let full = listed.len() >= PAGE;
        let previous = last.clone();
        last = listed.last().map(|held| held.name.clone());
        tags.extend(listed);
        if !full || last.is_none() || last == previous {
            return Ok(tags);
        }
    }
}

/// What one tag points at. The singular call nests everything under
/// `manifest`, unlike the listing whose items sit in `manifests[]` directly.
pub fn manifest(
    client: &Client,
    registry: &Registry,
    repo: &str,
    digest: &str,
) -> Result<Manifest> {
    let answer = client
        .call(
            &metadata_audience(registry, repo),
            Request::get(format!(
                "https://{}/acr/v1/{}/_manifests/{}",
                registry.login_server,
                path(repo),
                digest
            )),
        )
        .map_err(|error| explain(&registry.login_server, error))?;
    let held = if answer["manifest"].is_object() {
        &answer["manifest"]
    } else {
        &answer
    };
    Ok(Manifest {
        digest: text(&held["digest"]).unwrap_or_else(|| digest.to_owned()),
        size: count(&held["imageSize"]),
        // A multi-arch index names neither; the details pane prints `index`.
        architecture: text(&held["architecture"]),
        os: text(&held["os"]),
        created: stamp(&held["createdTime"]),
        tags: held["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect(),
    })
}

/// What one repository's own calls are signed for. `metadata_read` is the
/// action the `/acr/v1/` endpoints want; `pull` is for `/v2/` blobs.
fn metadata_audience(registry: &Registry, repo: &str) -> Audience {
    Audience::Acr {
        login_server: registry.login_server.clone(),
        scope: format!("repository:{repo}:metadata_read"),
    }
}

fn tag(entry: &Value) -> Option<Tag> {
    Some(Tag {
        name: text(&entry["name"])?,
        digest: string(&entry["digest"]),
        created: stamp(&entry["createdTime"]),
        updated: stamp(&entry["lastUpdateTime"]),
    })
}

fn tags_url(registry: &Registry, repo: &str, last: Option<&str>) -> String {
    let mut url = format!(
        "https://{}/acr/v1/{}/_tags?n={PAGE}&orderby=timedesc",
        registry.login_server,
        path(repo)
    );
    if let Some(last) = last {
        url.push_str("&last=");
        percent_encode(last, &mut url);
    }
    url
}

/// A repository name in a path. Names carry `/` — `team/api` — which has to
/// survive as itself, so only the characters that are not path-safe are
/// escaped.
fn path(repo: &str) -> String {
    let mut encoded = String::new();
    for segment in repo.split('/') {
        if !encoded.is_empty() {
            encoded.push('/');
        }
        percent_encode(segment, &mut encoded);
    }
    encoded
}

/// The pull reference `y` copies on a tag: what `docker pull` takes.
#[must_use]
pub fn pull_reference(login_server: &str, repo: &str, tag: &str) -> String {
    format!("{login_server}/{repo}:{tag}")
}

/// The digest reference `Y` copies: the one that names a build rather than a
/// moving label.
#[must_use]
pub fn digest_reference(login_server: &str, repo: &str, digest: &str) -> String {
    format!("{login_server}/{repo}@{digest}")
}

/// A digest short enough for a cell: the algorithm and eight hex characters.
/// The details pane prints the whole thing. Counted in characters: the
/// string is whatever the registry sent, and a byte slice through a
/// multi-byte one would panic in the middle of a frame.
#[must_use]
pub fn short_digest(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algorithm, hex)) if hex.chars().count() > 8 => {
            format!("{algorithm}:{}", hex.chars().take(8).collect::<String>())
        }
        _ => digest.to_owned(),
    }
}

/// Bytes as `docker images` prints them: SI units, one decimal.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn stamp(value: &Value) -> Option<Timestamp> {
    Timestamp::parse(&text(value)?)
}

/// A count, whichever way it was written: a registry sends these as numbers,
/// but a string of digits is the same answer.
fn count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::transport::Body;
    use crate::azure::transport::fake::{Answer, client as fake_client};
    use crate::timestamp::ts;
    use serde_json::json;

    fn registry() -> Registry {
        Registry {
            id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ContainerRegistry/registries/acrprod".into(),
            name: "acrprod".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            login_server: "acrprod.azurecr.io".into(),
        }
    }

    fn exchanged() -> Answer {
        Answer::json(json!({ "refresh_token": "refresh-1" }))
    }

    fn issued(name: &str) -> Answer {
        Answer::json(json!({ "access_token": name }))
    }

    fn form(request: &Request) -> Vec<(String, String)> {
        match &request.body {
            Body::Form(fields) => fields.clone(),
            other => panic!("expected a form body, got {other:?}"),
        }
    }

    fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(held, _)| held == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn the_exchange_runs_once_per_registry_however_many_scopes_are_asked_for() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("catalog-token"),
            Answer::json(json!({ "repositories": ["api"] })),
            // The second read is a different scope, so a second access token
            // — but no second exchange.
            issued("repo-token"),
            Answer::json(json!({ "imageName": "api", "tagCount": 3 })),
        ]);
        repositories(&client, &registry()).unwrap();
        attributes(&client, &registry(), "api").unwrap();

        let urls = transport.urls();
        assert_eq!(
            urls.iter()
                .filter(|url| url.ends_with("/oauth2/exchange"))
                .count(),
            1,
            "one exchange, whatever the scopes: {urls:?}"
        );
        assert_eq!(
            urls.iter()
                .filter(|url| url.ends_with("/oauth2/token"))
                .count(),
            2,
            "one access token per scope"
        );
        assert_eq!(
            transport.bearers()[2].as_deref(),
            Some("catalog-token"),
            "the catalog is signed with the registry's own token, not ARM's"
        );
        assert_eq!(transport.bearers()[4].as_deref(), Some("repo-token"));
    }

    #[test]
    fn no_token_is_exchanged_with_a_host_that_is_not_a_registry() {
        let mut elsewhere = registry();
        elsewhere.login_server = "evil.example".into();
        let (client, transport, _) = fake_client([exchanged(), issued("catalog-token")]);
        let error = format!("{:#}", repositories(&client, &elsewhere).unwrap_err());
        assert!(error.contains("not a registry login server"), "{error}");
        assert!(transport.sent().is_empty(), "nothing went out");
    }

    #[test]
    fn the_two_posts_carry_the_fields_each_endpoint_wants() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("catalog-token"),
            Answer::json(json!({ "repositories": [] })),
        ]);
        repositories(&client, &registry()).unwrap();
        let sent = transport.sent();

        let exchange = form(&sent[0]);
        assert_eq!(field(&exchange, "grant_type"), Some("access_token"));
        assert_eq!(field(&exchange, "service"), Some("acrprod.azurecr.io"));
        assert!(
            field(&exchange, "access_token").is_some_and(|token| token.starts_with("registry-")),
            "the CLI token is minted for the containerregistry audience: {exchange:?}"
        );
        assert!(sent[0].bearer.is_none(), "a token call carries no bearer");

        let token = form(&sent[1]);
        assert_eq!(field(&token, "grant_type"), Some("refresh_token"));
        assert_eq!(field(&token, "scope"), Some("registry:catalog:*"));
        assert_eq!(field(&token, "refresh_token"), Some("refresh-1"));
    }

    #[test]
    fn a_refused_data_plane_call_redoes_both_posts_once_and_then_gives_up() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("stale"),
            Answer::status(
                401,
                r#"{"errors":[{"code":"UNAUTHORIZED","message":"expired"}]}"#,
            ),
            // The retry mints both again.
            exchanged(),
            issued("fresh"),
            Answer::json(json!({ "repositories": ["api"] })),
        ]);
        let names = repositories(&client, &registry()).unwrap();
        assert_eq!(names, ["api"]);
        let urls = transport.urls();
        assert_eq!(
            urls.iter()
                .filter(|url| url.ends_with("/oauth2/exchange"))
                .count(),
            2,
            "the spent refresh token is dropped with the access token"
        );
        assert_eq!(transport.bearers()[5].as_deref(), Some("fresh"));
    }

    #[test]
    fn a_spent_chain_is_rebuilt_from_a_fresh_cli_token() {
        use crate::azure::auth::FixedTokens;
        use crate::azure::transport::Client;
        use crate::azure::transport::fake::FakeTransport;

        // The data plane refuses: the access token, the refresh token *and*
        // the CLI token they were traded from are all re-minted, or a TUI
        // open past the CLI token's hour would trade a stale one for ever.
        let tokens = FixedTokens::new();
        let transport = FakeTransport::answering([
            exchanged(),
            issued("stale"),
            Answer::status(401, r#"{"errors":[{"message":"expired"}]}"#),
            exchanged(),
            issued("fresh"),
            Answer::json(json!({ "repositories": ["api"] })),
        ]);
        let client = Client::new(Box::new(tokens.clone()), Box::new(transport.clone()));
        repositories(&client, &registry()).unwrap();
        let sent = transport.sent();
        assert_ne!(
            field(&form(&sent[0]), "access_token"),
            field(&form(&sent[3]), "access_token"),
            "the second exchange carries a fresh CLI token"
        );
        assert_eq!(tokens.count(), 2);

        // The refresh token itself is refused — what happens after three
        // hours — and the next read still gets through.
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("a"),
            Answer::json(json!({ "repositories": ["api"] })),
            Answer::status(401, r#"{"errors":[{"message":"refresh token expired"}]}"#),
            exchanged(),
            issued("b"),
            Answer::json(json!({ "imageName": "api" })),
        ]);
        repositories(&client, &registry()).unwrap();
        assert!(
            attributes(&client, &registry(), "api").is_ok(),
            "{:?}",
            transport.urls()
        );
    }

    #[test]
    fn a_second_refusal_is_reported_rather_than_retried_for_ever() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("stale"),
            Answer::status(401, r#"{"errors":[{"message":"expired"}]}"#),
            exchanged(),
            issued("also-stale"),
            Answer::status(401, r#"{"errors":[{"message":"expired"}]}"#),
        ]);
        let error = repositories(&client, &registry()).unwrap_err();
        assert!(
            format!("{error:#}").contains("acrprod.azurecr.io: no permission"),
            "{error:#}"
        );
        assert_eq!(transport.sent().len(), 6, "two rounds, not a loop");
    }

    #[test]
    fn a_refusal_on_the_exchange_itself_names_the_role_that_is_missing() {
        let refused = || {
            Answer::status(
                401,
                r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#,
            )
        };
        let (client, transport, _) = fake_client([refused(), refused()]);
        let error = format!("{:#}", repositories(&client, &registry()).unwrap_err());
        assert!(
            error.contains("acrprod.azurecr.io: no permission"),
            "{error}"
        );
        assert!(error.contains("AcrPull"), "{error}");
        assert_eq!(
            transport.sent().len(),
            2,
            "one fresh CLI token, then no more"
        );
    }

    #[test]
    fn a_stale_cli_token_on_the_exchange_is_minted_again_once() {
        // The registry was unreachable at every refresh for an hour, so no
        // data-plane 401 ever dropped the CLI token: the exchange is the
        // first call to see it has expired.
        let (client, transport, _) = fake_client([
            Answer::status(401, r#"{"errors":[{"message":"expired"}]}"#),
            exchanged(),
            issued("fresh"),
            Answer::json(json!({ "repositories": ["api"] })),
        ]);
        let names = repositories(&client, &registry()).unwrap();
        assert_eq!(names, ["api"]);
        let sent = transport.sent();
        assert_eq!(sent.len(), 4);
        let tokens: Vec<_> = sent[..2]
            .iter()
            .map(|request| field(&form(request), "access_token").unwrap().to_owned())
            .collect();
        assert_ne!(tokens[0], tokens[1], "the retry carried a fresh CLI token");
    }

    #[test]
    fn no_login_at_all_is_not_reported_as_a_missing_role() {
        use crate::azure::auth::{Audience, FixedTokens};
        use crate::azure::transport::{Client, NoLogin, is_no_login};

        let tokens = FixedTokens::new();
        tokens
            .answers
            .lock()
            .unwrap()
            .push_back(Err(anyhow::Error::new(NoLogin(
                "could not get a token for registry".to_owned(),
            ))));
        let transport = crate::azure::transport::fake::FakeTransport::answering([]);
        let client = Client::new(Box::new(tokens), Box::new(transport));

        let error = repositories(&client, &registry()).unwrap_err();
        assert!(
            is_no_login(&error),
            "a missing login must reach the worker as one: {error:#}"
        );
        assert!(
            !format!("{error:#}").contains("AcrPull"),
            "telling someone to ask for a role they cannot use is worse than useless: {error:#}"
        );
        let _ = Audience::ContainerRegistry.resource();
    }

    #[test]
    fn the_catalog_pages_with_last_and_a_short_page_ends_it() {
        let full: Vec<String> = (0..PAGE).map(|n| format!("repo-{n:03}")).collect();
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("catalog-token"),
            Answer::json(json!({ "repositories": full })),
            Answer::json(json!({ "repositories": ["repo-last"] })),
        ]);
        let names = repositories(&client, &registry()).unwrap();
        assert_eq!(names.len(), PAGE + 1);
        assert_eq!(names.last().unwrap(), "repo-last");
        let urls = transport.urls();
        assert_eq!(urls[2], "https://acrprod.azurecr.io/acr/v1/_catalog?n=100");
        assert_eq!(
            urls[3],
            "https://acrprod.azurecr.io/acr/v1/_catalog?n=100&last=repo-099"
        );
    }

    #[test]
    fn a_page_that_does_not_move_the_cursor_ends_the_listing() {
        let full: Vec<String> = (0..PAGE).map(|_| "same".to_owned()).collect();
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("t"),
            Answer::json(json!({ "repositories": full.clone() })),
            Answer::json(json!({ "repositories": full })),
        ]);
        repositories(&client, &registry()).unwrap();
        assert_eq!(transport.sent().len(), 4, "it stops rather than looping");
    }

    #[test]
    fn tags_keep_the_order_the_registry_gave_them() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("repo-token"),
            Answer::json(json!({
                "registry": "acrprod.azurecr.io",
                "imageName": "payments-api",
                "tags": [
                    { "name": "1.42.0", "digest": "sha256:ab12ef0199", "createdTime": "2026-09-11T18:00:00Z", "lastUpdateTime": "2026-09-11T18:00:00Z", "signed": false },
                    { "name": "1.41.3", "digest": "sha256:9f01aa4288", "createdTime": "2026-09-08T18:00:00Z", "lastUpdateTime": "2026-09-08T18:00:00Z" },
                ],
            })),
        ]);
        let read = tags(&client, &registry(), "payments-api").unwrap();
        assert_eq!(
            read.iter().map(|tag| tag.name.as_str()).collect::<Vec<_>>(),
            ["1.42.0", "1.41.3"],
            "timedesc is the registry's job, not ours"
        );
        assert_eq!(read[0].created, Some(ts("2026-09-11T18:00:00Z")));
        assert!(
            transport.urls()[2].ends_with("/acr/v1/payments-api/_tags?n=100&orderby=timedesc"),
            "{:?}",
            transport.urls()
        );
    }

    #[test]
    fn a_manifest_is_read_from_under_its_own_key_and_an_index_names_no_architecture() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("repo-token"),
            Answer::json(json!({
                "registry": "acrprod.azurecr.io",
                "imageName": "payments-api",
                "manifest": {
                    "digest": "sha256:ab12ef0199",
                    "imageSize": 84_200_000_u64,
                    "createdTime": "2026-09-11T18:00:00Z",
                    "lastUpdateTime": "2026-09-11T18:00:00Z",
                    "architecture": "amd64",
                    "os": "linux",
                    "configMediaType": "application/vnd.docker.container.image.v1+json",
                    "tags": ["1.42.0", "1.42"],
                },
            })),
        ]);
        let read = manifest(&client, &registry(), "payments-api", "sha256:ab12ef0199").unwrap();
        assert_eq!(read.size, Some(84_200_000));
        assert_eq!(read.architecture.as_deref(), Some("amd64"));
        assert_eq!(read.os.as_deref(), Some("linux"));
        assert_eq!(read.tags, ["1.42.0", "1.42"]);
        assert!(transport.urls()[2].ends_with("/_manifests/sha256:ab12ef0199"));

        let (client, _, _) = fake_client([
            exchanged(),
            issued("t"),
            // A manifest list carries neither; the fields are optional.
            Answer::json(
                json!({ "manifest": { "digest": "sha256:ff", "createdTime": "2026-09-11T18:00:00Z" } }),
            ),
        ]);
        let index = manifest(&client, &registry(), "payments-api", "sha256:ff").unwrap();
        assert_eq!(index.architecture, None);
        assert_eq!(index.os, None);
        assert_eq!(index.size, None);
        assert!(index.tags.is_empty());
    }

    #[test]
    fn attributes_fill_in_what_the_catalog_left_out() {
        let (client, _, _) = fake_client([
            exchanged(),
            issued("repo-token"),
            Answer::json(json!({
                "registry": "acrprod.azurecr.io",
                "imageName": "payments-api",
                "createdTime": "2025-09-11T18:00:00Z",
                "lastUpdateTime": "2026-09-11T18:00:00Z",
                "manifestCount": 51,
                "tagCount": "48",
                "changeableAttributes": { "deleteEnabled": true, "listEnabled": true },
            })),
        ]);
        let repository = attributes(&client, &registry(), "payments-api").unwrap();
        assert_eq!(repository.registry, "acrprod");
        assert_eq!(repository.name, "payments-api");
        assert_eq!(repository.manifest_count, Some(51));
        assert_eq!(
            repository.tag_count,
            Some(48),
            "a count written as digits in a string is the same count"
        );
        assert_eq!(repository.updated, Some(ts("2026-09-11T18:00:00Z")));
    }

    #[test]
    fn a_repository_with_a_slash_keeps_it_in_the_path_and_the_scope() {
        let (client, transport, _) = fake_client([
            exchanged(),
            issued("t"),
            Answer::json(json!({ "tags": [] })),
        ]);
        tags(&client, &registry(), "team/api").unwrap();
        assert_eq!(
            field(&form(&transport.sent()[1]), "scope"),
            Some("repository:team/api:metadata_read"),
            "the scope names the repository as it is written"
        );
        assert!(
            transport.urls()[2].contains("/acr/v1/team/api/_tags?"),
            "{:?}",
            transport.urls()
        );
    }

    #[test]
    fn the_references_the_copy_keys_build() {
        assert_eq!(
            pull_reference("acrprod.azurecr.io", "payments-api", "1.42.0"),
            "acrprod.azurecr.io/payments-api:1.42.0"
        );
        assert_eq!(
            digest_reference("acrprod.azurecr.io", "payments-api", "sha256:ab12ef01"),
            "acrprod.azurecr.io/payments-api@sha256:ab12ef01"
        );
        assert_eq!(short_digest("sha256:ab12ef0199aabb"), "sha256:ab12ef01");
        assert_eq!(
            short_digest("sha256:ab12"),
            "sha256:ab12",
            "nothing to trim"
        );
        assert_eq!(short_digest("nonsense"), "nonsense");
        assert_eq!(
            short_digest("sha256:aéééééé"),
            "sha256:aéééééé",
            "a registry's string is cut by character, never mid-byte"
        );
        assert_eq!(short_digest("sha256:éééééééééé"), "sha256:éééééééé");
    }

    #[test]
    fn a_size_reads_the_way_docker_images_prints_it() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 kB");
        assert_eq!(human_size(84_200_000), "84.2 MB");
        assert_eq!(human_size(3_000_000_000), "3.0 GB");
    }
}
