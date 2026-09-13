//! The one client every Azure read goes through, and the seam a test replaces
//! the network with.
//!
//! The client owns the policy all three planes share: sign with the cached
//! token, mint a new one and retry once when a plane says the token is spent,
//! wait out a throttle once, and turn every other refusal into the message
//! the service actually wrote. Nothing above this file retries anything.
//!
//! [`Method`] has two variants and neither is a write. That is the read-only
//! promise: a write verb cannot be named here, so no later step can add one
//! by accident.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

use super::auth::{Audience, TokenSource};

/// How long a throttled request waits when a service refuses one without
/// saying how long to leave it.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// The longest wait one header may ask for, so a date read out of a clock
/// that disagrees with ours cannot park the app for days.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The shortest wait worth taking: a header that works out at nothing is
/// still a refusal, and asking again in the same breath is what makes it
/// worse.
const MIN_RETRY_AFTER: Duration = Duration::from_secs(1);
/// The statuses Azure sheds load with.
const THROTTLED: [u16; 2] = [429, 503];
/// A body larger than this is not an answer to any of these calls.
const BODY_LIMIT: u64 = 32 * 1024 * 1024;
/// `Retry-After` in its other form: an IMF-fixdate, always in GMT.
const HTTP_DATE: &[FormatItem<'static>] = format_description!(
    "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
);

/// The two verbs this crate can spell. Both read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    /// Resource Graph's query and the two registry token endpoints. All
    /// three read; a `POST` is simply how they take their arguments.
    Post,
}

/// What a `POST` carries: a document, or the form fields a token endpoint
/// wants.
#[derive(Clone, Debug, Default)]
pub enum Body {
    #[default]
    None,
    Json(Value),
    Form(Vec<(String, String)>),
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    /// Filled in by [`Client::call`]; a request built by hand carries `None`
    /// and is signed on its way out.
    pub bearer: Option<String>,
    pub body: Body,
}

impl Request {
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            bearer: None,
            body: Body::None,
        }
    }

    #[must_use]
    pub fn post_json(url: impl Into<String>, body: Value) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            bearer: None,
            body: Body::Json(body),
        }
    }

    #[must_use]
    pub fn post_form(url: impl Into<String>, fields: Vec<(String, String)>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            bearer: None,
            body: Body::Form(fields),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// How the client reaches the network. One seam, so a test drives every read
/// in the crate with canned answers rather than a socket.
pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> Result<Response>;
}

/// The real thing. Redirects are not followed: a hop would drop the
/// `Authorization` header and trade a status this code can read for a page of
/// markup it cannot.
pub struct Https {
    agent: ureq::Agent,
}

impl Default for Https {
    fn default() -> Self {
        Self::new()
    }
}

impl Https {
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(30)))
                .build()
                .into(),
        }
    }
}

impl Transport for Https {
    fn send(&self, request: Request) -> Result<Response> {
        let bearer = request
            .bearer
            .as_ref()
            .map(|token| format!("Bearer {token}"));
        // `ureq` types its builders by whether a body follows, so the verb
        // and the body are chosen together rather than one after the other.
        let mut response = match (request.method, &request.body) {
            (Method::Get, _) => {
                let mut builder = self.agent.get(&request.url);
                if let Some(bearer) = &bearer {
                    builder = builder.header("Authorization", bearer);
                }
                builder.call()
            }
            (Method::Post, Body::Json(document)) => {
                let mut builder = self.agent.post(&request.url);
                if let Some(bearer) = &bearer {
                    builder = builder.header("Authorization", bearer);
                }
                builder.send_json(document)
            }
            (Method::Post, Body::Form(fields)) => {
                let mut builder = self
                    .agent
                    .post(&request.url)
                    .header("Content-Type", "application/x-www-form-urlencoded");
                if let Some(bearer) = &bearer {
                    builder = builder.header("Authorization", bearer);
                }
                builder.send(form_encode(fields))
            }
            (Method::Post, Body::None) => {
                let mut builder = self.agent.post(&request.url);
                if let Some(bearer) = &bearer {
                    builder = builder.header("Authorization", bearer);
                }
                builder.send_empty()
            }
        }
        .with_context(|| format!("the request to {} failed", request.url))?;

        let status = response.status().as_u16();
        // Read before the body, which takes the response apart.
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .with_context(|| format!("failed to read the answer from {}", request.url))?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

/// The credentials are spent rather than the request being wrong. The worker
/// recognises this one by type: it means stop the refresh and tell the user
/// to sign in, rather than mark one vault bad and carry on.
#[derive(Debug)]
pub struct SignedOut(pub String);

impl fmt::Display for SignedOut {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SignedOut {}

/// There is no login at all: `az` would not mint a token.
///
/// Told apart from [`SignedOut`] because they mean different things to a
/// registry. A registry answering `401` means this login has no role on it,
/// which is worth naming the role for; `az` answering nothing means there is
/// no login to have a role, which is worth naming `az login` for. Reading
/// both as the first one told a user to ask for AcrPull when what they
/// needed was to sign in.
#[derive(Debug)]
pub struct NoLogin(pub String);

impl fmt::Display for NoLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NoLogin {}

#[must_use]
pub fn is_no_login(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<NoLogin>())
}

/// True when this error, or anything it is wrapped in, means the run cannot
/// go on until somebody signs in — either plane's way of saying so.
#[must_use]
pub fn is_signed_out(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<SignedOut>() || cause.is::<NoLogin>())
}

/// What a person is shown for one failure.
///
/// A signed-out login is the one refusal worth rewording: the CLI answers it
/// with five lines of its own stack, and what a person needs is the two words
/// that fix it. The worker's events and the subcommands' errors both go
/// through here, so the status bar and stderr say the same thing.
#[must_use]
pub fn said(error: &anyhow::Error) -> String {
    if is_signed_out(error) {
        return SIGNED_OUT.to_owned();
    }
    format!("{error:#}")
}

/// The two words that fix a signed-out login.
pub const SIGNED_OUT: &str = "not signed in — run `az login`";

/// A plane refused, and said why in its own words.
///
/// Carried as a type rather than a formatted string so a caller can ask what
/// the status and the code were without parsing the message back apart —
/// which is how "a `403` naming `ForbiddenByFirewall`" gets told from "a
/// `403` naming nothing" a few frames later.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub url: String,
    /// `error.code`, its `innererror.code`, and a registry's `errors[0].code`
    /// — whichever of the three the body carried.
    pub codes: Vec<String>,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn has_code(&self, code: &str) -> bool {
        self.codes
            .iter()
            .any(|held| held.eq_ignore_ascii_case(code))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} answered {}: {}",
            self.url, self.status, self.message
        )
    }
}

impl std::error::Error for ApiError {}

/// The refusal this error carries, if it is one.
#[must_use]
pub fn api_error(error: &anyhow::Error) -> Option<&ApiError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>())
}

/// The client every read in the crate goes through.
///
// ponytail: one concrete type with two boxed seams rather than
// `Client<T: TokenSource, H: Transport>`. Generics would spell the two
// parameters into every function signature in `graph`, `vault`, `acr`, the
// worker and the screens for no gain — there is one real pair and one fake
// pair, and neither is on a hot path.
pub struct Client {
    tokens: Box<dyn TokenSource>,
    transport: Box<dyn Transport>,
    /// One token per audience, minted on first use and re-minted once when a
    /// plane says it is spent. A CLI token lasts about an hour; a running TUI
    /// outlives that.
    cached: Mutex<HashMap<String, String>>,
    /// One refresh token per registry, by login server. The exchange that
    /// mints one costs a round trip and the token it mints is good for every
    /// scope that registry is asked for, for about three hours.
    refresh_tokens: Mutex<HashMap<String, String>>,
    /// The tenant, asked for once and only when something needs it.
    tenant: Mutex<Option<Option<String>>>,
    /// How a wait is taken. The worker hands in one that says so on screen
    /// first; a test hands in one that records the wait instead of sleeping.
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
}

impl Client {
    #[must_use]
    pub fn new(tokens: Box<dyn TokenSource>, transport: Box<dyn Transport>) -> Self {
        Self::with_sleep(tokens, transport, Box::new(std::thread::sleep))
    }

    #[must_use]
    pub fn with_sleep(
        tokens: Box<dyn TokenSource>,
        transport: Box<dyn Transport>,
        sleep: Box<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        Self {
            tokens,
            transport,
            cached: Mutex::new(HashMap::new()),
            refresh_tokens: Mutex::new(HashMap::new()),
            tenant: Mutex::new(None),
            sleep,
        }
    }

    /// Replaces how a throttle wait is taken, so the worker can say on screen
    /// how long Azure asked for before sleeping through it.
    pub fn set_sleep(&mut self, sleep: Box<dyn Fn(Duration) + Send + Sync>) {
        self.sleep = sleep;
    }

    /// Forgets the token for one audience, so the next call mints afresh.
    ///
    /// A registry's access token is minted from its refresh token, and that
    /// from the CLI's token, so forgetting one has to forget the whole chain:
    /// otherwise the retry trades a spent token for another spent token, and
    /// a TUI left open past the CLI token's hour loses every registry for
    /// good.
    pub fn forget(&self, audience: &Audience) {
        locked(&self.cached).remove(&audience.cache_key());
        if let Audience::Acr { login_server, .. } = audience {
            locked(&self.refresh_tokens).remove(login_server);
            locked(&self.cached).remove(&Audience::ContainerRegistry.cache_key());
        }
    }

    /// The tenant the login is in, asked for at most once a run.
    pub fn tenant(&self) -> Option<String> {
        locked(&self.tenant)
            .get_or_insert_with(|| self.tokens.tenant())
            .clone()
    }

    /// The refresh token one registry issued, or the one it issues now.
    pub(crate) fn registry_refresh_token(
        &self,
        login_server: &str,
        mint: impl FnOnce() -> Result<String>,
    ) -> Result<String> {
        let held = locked(&self.refresh_tokens).get(login_server).cloned();
        if let Some(held) = held {
            return Ok(held);
        }
        let minted = mint()?;
        locked(&self.refresh_tokens).insert(login_server.to_owned(), minted.clone());
        Ok(minted)
    }

    /// One signed call, and the JSON it answered with.
    ///
    /// A `401` is worth exactly one fresh token: the CLI's tokens expire while
    /// the TUI is open, and re-minting is cheap. A second `401` is a signed-out
    /// login, which no amount of retrying fixes. The mint is inside the
    /// retried expression: a registry refusing a spent refresh token says so
    /// while the token is being minted, not while the call is being made.
    pub fn call(&self, audience: &Audience, mut request: Request) -> Result<Value> {
        let first = self.token(audience).and_then(|token| {
            request.bearer = Some(token);
            self.attempt(&request)
        });
        match first {
            // A missing login is not retried: a second `az` shell-out would
            // only fail the same way, a second later.
            Err(error) if is_signed_out(&error) && !is_no_login(&error) => {
                self.forget(audience);
                let Ok(minted) = self.token(audience) else {
                    // The mint failed too; the first refusal is the one that
                    // says to sign in.
                    return Err(error);
                };
                request.bearer = Some(minted);
                self.attempt(&request)
            }
            result => result,
        }
    }

    /// One unsigned call: the two registry token endpoints, which are how a
    /// token is got and so carry none. The throttle and error policy is the
    /// same; only the bearer is missing.
    pub fn call_unsigned(&self, request: Request) -> Result<Value> {
        self.attempt(&request)
    }

    /// This audience's token, minted on first use. Crate-visible because the
    /// registry exchange needs a CLI token as a *value* in a form body rather
    /// than as a header.
    pub(crate) fn token(&self, audience: &Audience) -> Result<String> {
        let key = audience.cache_key();
        // Cloned out before the mint below, which may itself reach back into
        // this cache: a lock held across it would deadlock.
        let held = locked(&self.cached).get(&key).cloned();
        if let Some(held) = held {
            return Ok(held);
        }
        // A registry does not take a CLI token: it trades one for its own.
        let minted = match audience {
            Audience::Acr {
                login_server,
                scope,
            } => super::acr::mint(self, login_server, scope)?,
            other => self.tokens.token(other)?,
        };
        locked(&self.cached).insert(key, minted.clone());
        Ok(minted)
    }

    /// One request, waiting out a throttle once. Throttling first: it is the
    /// one refusal that is nobody's fault and only worth waiting out.
    fn attempt(&self, request: &Request) -> Result<Value> {
        let response = self.transport.send(request.clone())?;
        let response = if THROTTLED.contains(&response.status) {
            let wait = throttle_wait(&response, OffsetDateTime::now_utc());
            (self.sleep)(wait);
            self.transport.send(request.clone())?
        } else {
            response
        };
        self.read(request, response)
    }

    fn read(&self, request: &Request, response: Response) -> Result<Value> {
        let status = response.status;
        let url = &request.url;
        if status == 401 {
            return Err(anyhow::Error::new(SignedOut(format!(
                "Azure refused the token for {url}: {}",
                failure_message(&response.body)
            ))));
        }
        if THROTTLED.contains(&status) {
            // The wait has already been taken once; a second refusal is the
            // service saying to come back later, not to ask again now.
            return Err(anyhow::Error::new(ApiError {
                status,
                url: url.clone(),
                codes: failure_codes(&response.body),
                message: format!(
                    "Azure is still throttling this: {}",
                    failure_message(&response.body)
                ),
            }));
        }
        if !(200..300).contains(&status) {
            return Err(anyhow::Error::new(ApiError {
                status,
                url: url.clone(),
                codes: failure_codes(&response.body),
                message: failure_message(&response.body),
            }));
        }
        // No call in the crate legitimately answers with nothing: read as
        // "no rows" it would empty a vault, or both tabs, and the next save
        // would write that emptiness to the cache.
        if response.body.trim().is_empty() {
            bail!("{url} answered with an empty body");
        }
        serde_json::from_str(&response.body)
            .with_context(|| format!("{url} answered with something other than JSON"))
    }
}

/// One of the client's caches, poisoned or not. A thread that panicked while
/// holding one left a map that is still a map; the other threads carry on.
fn locked<T>(cache: &Mutex<T>) -> MutexGuard<'_, T> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a plane says when it refuses. ARM and Key Vault write the reason
/// under `error.message`, a registry under `errors[0].message`, and anything
/// else is worth the front of its body rather than nothing.
#[must_use]
pub fn failure_message(text: &str) -> String {
    let parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    // ARM puts the reason a person can act on under `error.details` and a
    // stub about correlation ids in `error.message`; both are said.
    let details: Vec<&str> = parsed["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|detail| detail["message"].as_str())
        .collect();
    for candidate in [
        &parsed["error"]["message"],
        &parsed["errors"][0]["message"],
        &parsed["message"],
    ] {
        if let Some(said) = candidate.as_str() {
            return if details.is_empty() {
                said.to_owned()
            } else {
                format!("{said} \u{2014} {}", details.join("; "))
            };
        }
    }
    text.trim().chars().take(200).collect()
}

/// The code a plane refused with, when it wrote one: `error.code`, or a
/// registry's `errors[0].code`. The inner code is where Key Vault says
/// *why* a `403` happened, so it is read too.
#[must_use]
pub fn failure_codes(text: &str) -> Vec<String> {
    let parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    [
        &parsed["error"]["code"],
        &parsed["error"]["innererror"]["code"],
        &parsed["errors"][0]["code"],
    ]
    .into_iter()
    .filter_map(|value| value.as_str())
    .map(str::to_owned)
    .collect()
}

/// How long to leave a throttled answer alone.
///
/// ARM and a container registry say so in `Retry-After`. Resource Graph does
/// not: it says when the quota window resets, in `x-ms-user-quota-resets-after`
/// and as `hh:mm:ss`. Key Vault says nothing at all and the default applies.
#[must_use]
pub fn throttle_wait(response: &Response, now: OffsetDateTime) -> Duration {
    if let Some(header) = response.header("Retry-After") {
        return retry_after(Some(header), now);
    }
    if let Some(clock) = response
        .header("x-ms-user-quota-resets-after")
        .and_then(hms_seconds)
    {
        return retry_after(Some(&clock.to_string()), now);
    }
    DEFAULT_RETRY_AFTER
}

/// `hh:mm:ss` as whole seconds. Resource Graph's quota header is a duration
/// written as a clock, which no other Azure header does.
fn hms_seconds(raw: &str) -> Option<u64> {
    let parts: Vec<u64> = raw
        .trim()
        .split(':')
        .map(|part| part.trim().parse::<u64>().ok())
        .collect::<Option<_>>()?;
    match parts[..] {
        [hours, minutes, seconds] => Some(hours * 3600 + minutes * 60 + seconds),
        [minutes, seconds] => Some(minutes * 60 + seconds),
        _ => None,
    }
}

/// The wait a throttled answer asks for: `Retry-After` as whole seconds, or
/// as a date to count forward to. Never less than [`MIN_RETRY_AFTER`], never
/// more than [`MAX_RETRY_AFTER`], and [`DEFAULT_RETRY_AFTER`] when the header
/// is absent or is something this cannot read.
#[must_use]
pub fn retry_after(header: Option<&str>, now: OffsetDateTime) -> Duration {
    let Some(raw) = header.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return DEFAULT_RETRY_AFTER;
    };
    // `nan` parses as a number and would take the clamp — and the `Duration`
    // — down with it, so only a finite number counts.
    let seconds = match raw.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => seconds,
        _ => match PrimitiveDateTime::parse(raw, HTTP_DATE) {
            Ok(when) => (when.assume_utc() - now).as_seconds_f64(),
            Err(_) => return DEFAULT_RETRY_AFTER,
        },
    };
    Duration::from_secs_f64(
        seconds.clamp(MIN_RETRY_AFTER.as_secs_f64(), MAX_RETRY_AFTER.as_secs_f64()),
    )
}

/// `application/x-www-form-urlencoded`, which is also a query string.
///
// ponytail: percent-encoding by hand rather than the `url` crate; it is the
// unreserved set from RFC 3986 and a space, and nothing here encodes anything
// more exotic than a token.
#[must_use]
pub fn form_encode(pairs: &[(String, String)]) -> String {
    let mut encoded = String::new();
    for (key, value) in pairs {
        if !encoded.is_empty() {
            encoded.push('&');
        }
        percent_encode(key, &mut encoded);
        encoded.push('=');
        percent_encode(value, &mut encoded);
    }
    encoded
}

/// One path or query segment, with everything outside the unreserved set
/// escaped. A repository name carries `/`, which has to survive as `%2F` in a
/// scope string and as itself in a path — so callers pick.
pub fn percent_encode(raw: &str, out: &mut String) {
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
