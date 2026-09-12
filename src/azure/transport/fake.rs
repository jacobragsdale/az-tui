//! A transport over canned answers, keeping every request it was handed.
//!
//! The shape of every test in this crate: no test touches the network or
//! runs `az`, and the recorded requests are how a test asserts which URL was
//! asked and which plane's token signed it.

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
        let answer = answer
            .with_context(|| format!("the fake transport ran out of answers at {}", request.url))?;
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
