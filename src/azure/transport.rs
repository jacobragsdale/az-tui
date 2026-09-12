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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result};
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
pub trait Transport: Send {
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

/// True when this error, or anything it is wrapped in, is a refused login.
#[must_use]
pub fn is_signed_out(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<SignedOut>())
}

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
    cached: RefCell<HashMap<String, String>>,
    /// One refresh token per registry, by login server. The exchange that
    /// mints one costs a round trip and the token it mints is good for every
    /// scope that registry is asked for, for about three hours.
    refresh_tokens: RefCell<HashMap<String, String>>,
    /// The tenant, asked for once and only when something needs it.
    tenant: RefCell<Option<Option<String>>>,
    /// How long the last refusal asked to be left alone, until something
    /// reads it.
    throttled: Cell<Option<Duration>>,
    /// How a wait is taken. A test hands in a closure that records the wait
    /// instead of sleeping through it.
    sleep: Box<dyn Fn(Duration) + Send>,
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
        sleep: Box<dyn Fn(Duration) + Send>,
    ) -> Self {
        Self {
            tokens,
            transport,
            cached: RefCell::new(HashMap::new()),
            refresh_tokens: RefCell::new(HashMap::new()),
            tenant: RefCell::new(None),
            throttled: Cell::new(None),
            sleep,
        }
    }

    /// How long the refusals since this was last asked want to be left alone.
    /// Reading it clears it, so one refusal is reported once.
    pub fn last_throttle(&self) -> Option<Duration> {
        self.throttled.take()
    }

    /// Forgets the token for one audience, so the next call mints afresh.
    ///
    /// A registry's access token is minted from its refresh token, so
    /// forgetting one has to forget the other: otherwise the retry trades a
    /// spent refresh token for another spent access token.
    pub fn forget(&self, audience: &Audience) {
        self.cached.borrow_mut().remove(&audience.cache_key());
        if let Audience::Acr { login_server, .. } = audience {
            self.refresh_tokens.borrow_mut().remove(login_server);
        }
    }

    /// The tenant the login is in, asked for at most once a run.
    pub fn tenant(&self) -> Option<String> {
        let mut held = self.tenant.borrow_mut();
        held.get_or_insert_with(|| self.tokens.tenant()).clone()
    }

    /// The refresh token one registry issued, or the one it issues now.
    pub(crate) fn registry_refresh_token(
        &self,
        login_server: &str,
        mint: impl FnOnce() -> Result<String>,
    ) -> Result<String> {
        let held = self.refresh_tokens.borrow().get(login_server).cloned();
        if let Some(held) = held {
            return Ok(held);
        }
        let minted = mint()?;
        self.refresh_tokens
            .borrow_mut()
            .insert(login_server.to_owned(), minted.clone());
        Ok(minted)
    }

    /// One signed call, and the JSON it answered with.
    ///
    /// A `401` is worth exactly one fresh token: the CLI's tokens expire while
    /// the TUI is open, and re-minting is cheap. A second `401` is a signed-out
    /// login, which no amount of retrying fixes.
    pub fn call(&self, audience: &Audience, mut request: Request) -> Result<Value> {
        request.bearer = Some(self.token(audience)?);
        match self.attempt(&request) {
            Err(error) if is_signed_out(&error) => {
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

    /// This audience's token, minted on first use. Public because the
    /// registry exchange needs a CLI token as a *value* in a form body rather
    /// than as a header.
    pub fn bearer(&self, audience: &Audience) -> Result<String> {
        self.token(audience)
    }

    /// This audience's token, minted on first use.
    fn token(&self, audience: &Audience) -> Result<String> {
        let key = audience.cache_key();
        // Cloned out before the mint below, which may itself reach back into
        // this cache: a borrow held across it would panic at runtime.
        let held = self.cached.borrow().get(&key).cloned();
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
        self.cached.borrow_mut().insert(key, minted.clone());
        Ok(minted)
    }

    /// One request, waiting out a throttle once. Throttling first: it is the
    /// one refusal that is nobody's fault and only worth waiting out.
    fn attempt(&self, request: &Request) -> Result<Value> {
        let response = self.transport.send(request.clone())?;
        let response = if THROTTLED.contains(&response.status) {
            let wait = throttle_wait(&response, OffsetDateTime::now_utc());
            self.note_throttle(wait);
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
        if response.body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&response.body)
            .with_context(|| format!("{url} answered with something other than JSON"))
    }

    /// One read makes several calls; the longest wait any of them asked for
    /// is the one worth reporting.
    fn note_throttle(&self, wait: Duration) {
        if self.throttled.get().is_none_or(|held| wait > held) {
            self.throttled.set(Some(wait));
        }
    }
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

/// A transport over canned answers, keeping every request it was handed. The
/// shape of every test in this crate.
#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// One canned answer: a status, the headers worth carrying, and a body.
    #[derive(Clone, Debug)]
    pub struct Answer {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Answer {
        pub fn ok(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        pub fn json(body: serde_json::Value) -> Self {
            Self::ok(body.to_string())
        }

        pub fn status(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        pub fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }
    }

    /// Called with every request on its way out, before the answer comes
    /// back. A test that has to land something in another thread's queue at
    /// an exact moment does it here rather than by sleeping and hoping.
    type Watcher = Arc<dyn Fn(&Request) + Send + Sync>;

    #[derive(Clone, Default)]
    pub struct FakeTransport {
        answers: Arc<Mutex<VecDeque<Answer>>>,
        sent: Arc<Mutex<Vec<Request>>>,
        watcher: Arc<Mutex<Option<Watcher>>>,
    }

    impl FakeTransport {
        pub fn answering(answers: impl IntoIterator<Item = Answer>) -> Self {
            Self {
                answers: Arc::new(Mutex::new(answers.into_iter().collect())),
                sent: Arc::new(Mutex::new(Vec::new())),
                watcher: Arc::new(Mutex::new(None)),
            }
        }

        /// Runs `watch` on every request as it goes out.
        pub fn watch(&self, watch: impl Fn(&Request) + Send + Sync + 'static) {
            *self.watcher.lock().unwrap() = Some(Arc::new(watch));
        }

        /// Every request it was handed, in order.
        pub fn sent(&self) -> Vec<Request> {
            self.sent.lock().unwrap().clone()
        }

        pub fn urls(&self) -> Vec<String> {
            self.sent().into_iter().map(|request| request.url).collect()
        }

        /// The bearer each request carried, for asserting a call was signed
        /// with the right plane's token.
        pub fn bearers(&self) -> Vec<Option<String>> {
            self.sent()
                .into_iter()
                .map(|request| request.bearer)
                .collect()
        }

        pub fn remaining(&self) -> usize {
            self.answers.lock().unwrap().len()
        }
    }

    impl Transport for FakeTransport {
        fn send(&self, request: Request) -> Result<Response> {
            let watcher = self.watcher.lock().unwrap().clone();
            if let Some(watch) = watcher {
                watch(&request);
            }
            let answer = self.answers.lock().unwrap().pop_front();
            self.sent.lock().unwrap().push(request.clone());
            let answer = answer.with_context(|| {
                format!("the fake transport ran out of answers at {}", request.url)
            })?;
            Ok(Response {
                status: answer.status,
                headers: answer.headers,
                body: answer.body,
            })
        }
    }

    /// A client over canned answers and fixed tokens, and the waits it took.
    pub fn client(answers: impl IntoIterator<Item = Answer>) -> (Client, FakeTransport, Waits) {
        let transport = FakeTransport::answering(answers);
        let waits = Waits::default();
        let recorder = waits.clone();
        let client = Client::with_sleep(
            Box::new(super::super::auth::FixedTokens::new()),
            Box::new(transport.clone()),
            Box::new(move |wait| recorder.0.lock().unwrap().push(wait)),
        );
        (client, transport, waits)
    }

    #[derive(Clone, Default)]
    pub struct Waits(Arc<Mutex<Vec<Duration>>>);

    impl Waits {
        pub fn taken(&self) -> Vec<Duration> {
            self.0.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
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
            Answer::status(429, r#"{"error":{"message":"slow down"}}"#)
                .with_header("Retry-After", "2"),
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
}
