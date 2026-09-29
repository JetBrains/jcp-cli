//! Fake HTTP server for CLI tests, on `tiny_http`.
//!
//! It has a route table with canned responses and records each request.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    thread,
};
use tiny_http::{Header, Response, Server};

/// One request that the server received
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// Path and query
    pub url: String,
    pub authorization: Option<String>,
    pub body: String,
}

impl Recorded {
    pub fn path(&self) -> &str {
        self.url.split('?').next().unwrap_or_default()
    }

    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("request body is not JSON")
    }
}

/// A canned response
#[derive(Debug, Clone)]
pub struct Reply {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Reply {
    pub fn json(value: serde_json::Value) -> Self {
        Self::bytes(200, "application/json", value.to_string().into_bytes())
    }

    pub fn text(status: u16, text: &str) -> Self {
        Self::bytes(status, "text/plain", text.as_bytes().to_vec())
    }

    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
        }
    }

    pub fn no_content() -> Self {
        Self::bytes(204, "text/plain", Vec::new())
    }
}

struct Route {
    method: String,
    /// A path, or a path with a query for an exact match
    target: String,
    /// The replies in sequence. The last reply repeats.
    replies: VecDeque<Reply>,
}

#[derive(Default)]
struct State {
    routes: Vec<Route>,
    requests: Vec<Recorded>,
}

pub struct FakeHttp {
    base_url: String,
    server: Arc<Server>,
    state: Arc<Mutex<State>>,
}

impl FakeHttp {
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("cannot start the fake server"));
        let port = server
            .server_addr()
            .to_ip()
            .expect("not an IP address")
            .port();
        let state = Arc::new(Mutex::new(State::default()));
        let (thread_server, thread_state) = (server.clone(), state.clone());
        thread::spawn(move || {
            for mut request in thread_server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let recorded = Recorded {
                    method: request.method().as_str().to_string(),
                    url: request.url().to_string(),
                    authorization: request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .map(|h| h.value.as_str().to_string()),
                    body,
                };
                let reply = thread_state.lock().unwrap().reply(&recorded);
                thread_state.lock().unwrap().requests.push(recorded);
                let header =
                    Header::from_bytes("Content-Type", reply.content_type.as_bytes()).unwrap();
                let response = Response::from_data(reply.body)
                    .with_status_code(reply.status)
                    .with_header(header);
                let _ = request.respond(response);
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            server,
            state,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Adds a route with one reply.
    pub fn route(&self, method: &str, target: &str, reply: Reply) -> &Self {
        self.route_seq(method, target, vec![reply])
    }

    /// Adds a route with replies in sequence. The last reply repeats.
    pub fn route_seq(&self, method: &str, target: &str, replies: Vec<Reply>) -> &Self {
        self.state.lock().unwrap().routes.push(Route {
            method: method.into(),
            target: target.into(),
            replies: replies.into(),
        });
        self
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    /// The requests with this method and path (without the query)
    pub fn requests_to(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path() == path)
            .collect()
    }
}

impl State {
    /// Finds the reply. A route with a query matches before a route with the path only.
    fn reply(&mut self, request: &Recorded) -> Reply {
        let path = request.path().to_string();
        let index = self
            .routes
            .iter()
            .position(|r| r.method == request.method && r.target == request.url)
            .or_else(|| {
                self.routes
                    .iter()
                    .position(|r| r.method == request.method && r.target == path)
            });
        match index {
            Some(i) => {
                let replies = &mut self.routes[i].replies;
                if replies.len() > 1 {
                    replies.pop_front().unwrap()
                } else {
                    replies.front().cloned().unwrap_or(Reply::no_content())
                }
            }
            None => Reply::text(
                404,
                &format!("no route for {} {}", request.method, request.url),
            ),
        }
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.server.unblock();
    }
}
