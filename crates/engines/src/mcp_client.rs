//! A small client for external MCP servers, so their tools can be offered as Craft Anything
//! tools. Two transports implement [`Transport`]: a local server over stdio (newline-delimited
//! JSON-RPC 2.0), run through `craft_anything_platform::process` like every external program and
//! killed when the client is dropped; and a remote server over Streamable HTTP (JSON or
//! server-sent events in reply to each POST).

use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::process::{Process, RunningProcess, StdinMode};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// The protocol version we ask for; servers may answer with an older one they support.
const PROTOCOL: &str = "2025-06-18";
/// Largest single message we accept from a server.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// How to start a local server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// Where a remote server answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    /// `https://…`, or `http://` to this computer (`localhost`, `127.0.0.1`, `[::1]`).
    pub url: String,
    /// Sent with every request (e.g. `Authorization`).
    pub headers: BTreeMap<String, String>,
}

/// How to reach a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Server {
    Command(ServerCommand),
    Url(ServerUrl),
}

/// One tool a server offers (`tools/list`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RemoteTool {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: Value,
}

/// How JSON-RPC messages reach a server and its answers come back. Implement it for another
/// transport (WebSocket, a pipe…) and pass it to [`McpClient::with_transport`].
pub trait Transport: Send {
    /// Send a notification (no answer expected).
    fn notify(&mut self, msg: &Value) -> Result<(), ToolError>;
    /// Send request `id` and wait (until `deadline`, or `cancelled`) for the message answering it.
    fn request(&mut self, msg: &Value, id: u64, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<Value, ToolError>;
    /// Stop a local server and return the end of what it printed, for an error that has nothing
    /// else to show (a server that stopped answering). `None` for a transport with no output.
    fn stop_for_output(&mut self) -> Option<String> {
        None
    }
}

/// A server, initialized and ready for requests.
pub struct McpClient {
    name: String,
    transport: Box<dyn Transport>,
    next_id: u64,
}

impl McpClient {
    /// Reach the server and complete the MCP handshake (within `timeout`).
    pub fn connect(name: &str, server: &Server, timeout: Duration) -> Result<McpClient, ToolError> {
        let transport: Box<dyn Transport> = match server {
            Server::Command(cmd) => Box::new(Stdio::start(name, cmd)?),
            Server::Url(u) => Box::new(Http::new(name, u)?),
        };
        McpClient::with_transport(name, transport, timeout)
    }

    /// Start a local server and complete the handshake.
    pub fn start(name: &str, cmd: &ServerCommand, timeout: Duration) -> Result<McpClient, ToolError> {
        McpClient::connect(name, &Server::Command(cmd.clone()), timeout)
    }

    /// Complete the handshake over any transport.
    pub fn with_transport(name: &str, transport: Box<dyn Transport>, timeout: Duration) -> Result<McpClient, ToolError> {
        let mut client = McpClient { name: name.to_string(), transport, next_id: 1 };
        let started = client
            .request(
                "initialize",
                json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "craft-anything", "version": env!("CARGO_PKG_VERSION")}}),
                timeout,
                &|| false,
            )
            .and_then(|_| client.notify("notifications/initialized", json!({})));
        match started {
            Ok(()) => Ok(client),
            Err(e) => Err(client.with_output(e)),
        }
    }

    /// For a server that stopped answering (`TimedOut` without details): stop it and add the end of
    /// what it printed to the error, as `details.output_tail` and in its remediation. The client
    /// can't be used after this; any other error is returned as it is.
    pub fn with_output(&mut self, e: ToolError) -> ToolError {
        if e.code != ErrorCode::TimedOut || e.details.is_some() {
            return e;
        }
        let Some(printed) = self.transport.stop_for_output().filter(|p| !p.trim().is_empty()) else { return e };
        let said = e.remediation.clone().map(|r| format!("{r}\n\n")).unwrap_or_default();
        e.with_remediation(format!("{said}What it printed last:\n{printed}")).with_details(json!({"output_tail": printed}))
    }

    /// Every tool the server offers (following pagination, at most 50 pages).
    pub fn list_tools(&mut self, timeout: Duration) -> Result<Vec<RemoteTool>, ToolError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params, timeout, &|| false)?;
            for t in r.get("tools").and_then(Value::as_array).cloned().unwrap_or_default() {
                if let Ok(t) = serde_json::from_value::<RemoteTool>(t) {
                    tools.push(t);
                }
            }
            cursor = r.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// Call a tool. A result the server marks as an error becomes a `ToolError` with its text.
    pub fn call_tool(&mut self, name: &str, arguments: Value, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value, ToolError> {
        let r = self.request("tools/call", json!({"name": name, "arguments": arguments}), timeout, cancelled)?;
        if r.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            let text = content_text(&r);
            return Err(ToolError::new(
                ErrorCode::ProgramFailed,
                format!("The {} plugin's {name} failed: {}", self.name, if text.is_empty() { "no details" } else { &text }),
            ));
        }
        Ok(r)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), ToolError> {
        self.transport.notify(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value, ToolError> {
        // Nothing is sent, so there is no request for a cancellation notice to name.
        if cancelled() {
            return Err(ToolError::cancelled());
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let answer = match self.transport.request(&msg, id, Instant::now() + timeout, cancelled) {
            Err(e) if e.code == ErrorCode::Cancelled => {
                let _ = self.notify("notifications/cancelled", json!({"requestId": id}));
                return Err(e);
            }
            Err(e) if e.code == ErrorCode::TimedOut => {
                return Err(ToolError::new(ErrorCode::TimedOut, format!("The {} plugin didn't answer {method} in time.", self.name))
                    .with_remediation("Try again; if it keeps happening, check the plugin."));
            }
            r => r?,
        };
        if let Some(err) = answer.get("error") {
            let text = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(ToolError::new(ErrorCode::ProgramFailed, format!("The {} plugin refused {method}: {text}", self.name)));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Whether `msg` is the answer to request `id`.
fn answers(msg: &Value, id: u64) -> bool {
    msg.get("id").and_then(Value::as_u64) == Some(id) && (msg.get("result").is_some() || msg.get("error").is_some())
}

/// A local server over stdio.
struct Stdio {
    name: String,
    process: RunningProcess,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Stdio {
    fn start(name: &str, cmd: &ServerCommand) -> Result<Stdio, ToolError> {
        let (tx, lines) = channel();
        let mut p = Process::new(&cmd.command)
            .args(&cmd.args)
            .stdin(StdinMode::Piped)
            // A server lives as long as the session; it's stopped when the client is dropped.
            .timeout(Duration::from_secs(7 * 24 * 3600))
            .stdout_cap(MAX_LINE)
            .on_stdout_line(move |l| {
                let _ = tx.send(l.to_string());
            })
            .label(format!("the {name} plugin"))
            .remediation(format!("Check the command for the {name} plugin in config.toml ([[plugins.mcp]])."));
        for (k, v) in &cmd.env {
            p = p.env(k, v);
        }
        if let Some(dir) = &cmd.cwd {
            p = p.cwd(dir);
        }
        let mut process = p.spawn()?;
        let stdin = process.take_stdin().ok_or_else(|| ToolError::internal(format!("the {name} plugin has no input pipe")))?;
        Ok(Stdio { name: name.to_string(), process, stdin, lines })
    }

    /// The server is gone: its exit code and the end of what it printed, when the run has them.
    fn exited(&mut self) -> ToolError {
        let fix = "Check its command in config.toml ([[plugins.mcp]]).";
        let Some(Ok(out)) = self.process.wait_timeout(Duration::from_millis(500)) else {
            return ToolError::new(ErrorCode::EngineMissing, format!("The {} plugin exited.", self.name)).with_remediation(fix);
        };
        let code = out.code.map(|c| format!(" with exit code {c}")).unwrap_or_default();
        let printed = craft_anything_platform::process::output_excerpt(&out.end_text());
        let said = if printed.is_empty() { format!("It printed nothing. {fix}") } else { format!("What it printed last:\n{printed}\n\n{fix}") };
        ToolError::new(ErrorCode::EngineMissing, format!("The {} plugin exited{code}.", self.name))
            .with_remediation(said)
            .with_details(json!({"exit_code": out.code, "output_tail": printed}))
    }
}

impl Transport for Stdio {
    fn stop_for_output(&mut self) -> Option<String> {
        self.process.kill_and_wait(Duration::from_secs(2));
        let printed = match self.process.try_result()? {
            Ok(out) => out.end_text(),
            // A stopped run keeps what it had printed in `details.partial`.
            Err(e) => e.details.as_ref()?.pointer("/partial/stderr_tail")?.as_str()?.to_string(),
        };
        Some(craft_anything_platform::process::output_excerpt(&printed))
    }

    fn notify(&mut self, msg: &Value) -> Result<(), ToolError> {
        let mut line = msg.to_string();
        line.push('\n');
        let sent = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| ToolError::new(ErrorCode::EngineMissing, format!("The {} plugin stopped: {e}", self.name)));
        // A write fails when the server has already exited: say what it printed instead.
        // The supervisor may not have published the exit yet, so wait for it a moment.
        if sent.is_err() && self.process.wait_timeout(Duration::from_millis(500)).is_some() {
            return Err(self.exited());
        }
        sent
    }

    fn request(&mut self, msg: &Value, id: u64, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<Value, ToolError> {
        self.notify(msg)?;
        loop {
            if cancelled() {
                return Err(ToolError::cancelled());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ToolError::new(ErrorCode::TimedOut, "no answer in time"));
            }
            let line = match self.lines.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) if self.process.is_finished() => return Err(self.exited()),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err(self.exited()),
            };
            // Servers may log to stdout or send notifications; only our answer matters.
            if let Ok(msg) = serde_json::from_str::<Value>(line.trim())
                && answers(&msg, id)
            {
                return Ok(msg);
            }
        }
    }
}

impl Drop for Stdio {
    fn drop(&mut self) {
        // Confirmed, not just asked: the session may end (and the process exit) right after.
        self.process.kill_and_wait(Duration::from_secs(5));
    }
}

/// A remote server over Streamable HTTP: each message is POSTed; a request's answer comes back as
/// JSON or as server-sent events. The session id the server gives is sent back with each request.
struct Http {
    name: String,
    url: String,
    https: bool,
    headers: BTreeMap<String, String>,
    session: Option<String>,
    initialized: bool,
    #[cfg(test)]
    tls: Option<ureq::tls::TlsConfig>,
}

impl Http {
    fn new(name: &str, u: &ServerUrl) -> Result<Http, ToolError> {
        let url = u.url.trim();
        if !url_allowed(url) {
            return Err(ToolError::invalid(format!(
                "The {name} plugin's url must start with https:// (or http:// to this computer), name a host without user:password@, and use a port from 0 to 65535 if it has one."
            ))
            .with_remediation("Put keys or tokens in the plugin's headers in config.toml ([[plugins.mcp]]), not in the url."));
        }
        let https = url.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("https:"));
        Ok(Http {
            name: name.to_string(),
            url: url.to_string(),
            https,
            headers: u.headers.clone(),
            session: None,
            initialized: false,
            #[cfg(test)]
            tls: None,
        })
    }

    /// POSTs `msg`, giving up at `deadline` across every redirect hop and before resending once
    /// `cancelled` says so.
    fn post(&mut self, msg: &Value, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<ureq::http::Response<ureq::Body>, ToolError> {
        // Redirects are followed by hand below, for https too: each target is checked before the
        // body is sent again, and the configured headers and session go only to the configured
        // server. With no redirects allowed, ureq returns a 3xx answer as it is.
        let mut builder = ureq::Agent::config_builder()
            .https_only(self.https)
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent("CraftAnything")
            .timeout_connect(Some(Duration::from_secs(20)));
        if !self.https {
            // Plain HTTP is allowed only to loopback, so no proxy may carry it off this computer.
            builder = builder.proxy(None);
        }
        #[cfg(test)]
        if let Some(tls) = &self.tls {
            builder = builder.proxy(None).tls_config(tls.clone());
        }
        let agent = builder.build().new_agent();
        let body = msg.to_string();
        let home = origin(&self.url);
        let mut url = self.url.clone();
        let mut hops = 0;
        let resp = loop {
            if hops > 0 && cancelled() {
                return Err(ToolError::cancelled());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ToolError::new(ErrorCode::TimedOut, "no answer in time"));
            }
            let mut req = agent
                .post(&url)
                .config()
                .timeout_global(Some(left))
                .build()
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream");
            if self.initialized {
                req = req.header("MCP-Protocol-Version", PROTOCOL);
            }
            // The configured headers and the session belong to the configured server only.
            if origin(&url) == home {
                if let Some(s) = &self.session {
                    req = req.header("Mcp-Session-Id", s);
                }
                for (k, v) in &self.headers {
                    req = req.header(k, v);
                }
            }
            let resp = req.send(body.as_str()).map_err(|e| {
                if matches!(e, ureq::Error::RequireHttpsOnly(_)) {
                    // This error carries the redirect URI, which can contain tokens or signed parameters.
                    return ToolError::new(ErrorCode::EngineMissing, format!("The {} plugin redirected to plain HTTP and was refused.", self.name))
                        .with_remediation("Keep every redirect for a remote MCP plugin on HTTPS.");
                }
                if matches!(e, ureq::Error::Protocol(_)) {
                    // These carry raw text from the answer, such as a malformed redirect target
                    // with signed parameters. A send failure invalidates the cached session.
                    return ToolError::new(
                        ErrorCode::EngineMissing,
                        format!("The {} plugin sent an HTTP answer that couldn't be read, such as a malformed redirect.", self.name),
                    )
                    .with_remediation("Check its url in config.toml ([[plugins.mcp]]) and the server's redirects.");
                }
                let code = if timed_out(&e) { ErrorCode::TimedOut } else { ErrorCode::EngineMissing };
                ToolError::new(code, format!("Couldn't reach the {} plugin: {e}", self.name))
                    .with_remediation("Check its url in config.toml ([[plugins.mcp]]) and your connection.")
            })?;
            let status = resp.status().as_u16();
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                break resp;
            }
            let Some(next) = resp.headers().get("location").and_then(|v| v.to_str().ok()).and_then(|l| resolve_location(&url, l)) else {
                break resp;
            };
            next_hop_allowed(&self.name, self.https, &next)?;
            // 301, 302 and 303 turn a POST into a GET, which can't carry an MCP message.
            if !matches!(status, 307 | 308) {
                break resp;
            }
            if hops == MAX_REDIRECTS {
                return Err(ToolError::new(ErrorCode::EngineMissing, format!("The {} plugin redirected more than {MAX_REDIRECTS} times.", self.name))
                    .with_remediation("Set its url in config.toml ([[plugins.mcp]]) to the final address."));
            }
            hops += 1;
            url = next;
        };
        let status = resp.status().as_u16();
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            if origin(&url) == home {
                self.session = Some(s.to_string());
            } else if (200..300).contains(&status) {
                // The session would have to be sent to an address that wasn't configured.
                return Err(ToolError::new(
                    ErrorCode::EngineMissing,
                    format!("The {} plugin redirected to another address that started a session, which isn't followed.", self.name),
                )
                .with_remediation("Set its url in config.toml ([[plugins.mcp]]) to the final address."));
            }
        }
        if !(200..300).contains(&status) {
            let hint = if status == 401 || status == 403 {
                "Check the plugin's headers (its key or token) in config.toml."
            } else {
                "Check its url in config.toml ([[plugins.mcp]])."
            };
            return Err(ToolError::new(ErrorCode::EngineMissing, format!("The {} plugin answered {status}.", self.name)).with_remediation(hint));
        }
        Ok(resp)
    }
}

impl Transport for Http {
    fn notify(&mut self, msg: &Value) -> Result<(), ToolError> {
        let method = msg.get("method").and_then(Value::as_str);
        // A cancellation is best effort and the caller waits for it, so it gets a short budget.
        let budget = if method == Some("notifications/cancelled") { Duration::from_secs(2) } else { Duration::from_secs(30) };
        self.post(msg, Instant::now() + budget, &|| false)?;
        if method == Some("notifications/initialized") {
            self.initialized = true;
        }
        Ok(())
    }

    fn request(&mut self, msg: &Value, id: u64, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<Value, ToolError> {
        let resp = self.post(msg, deadline, cancelled)?;
        let event_stream = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
        let unreadable =
            |e: &dyn std::fmt::Display| ToolError::new(ErrorCode::GarbledOutput, format!("The {} plugin's answer couldn't be read: {e}", self.name));
        let late = || ToolError::new(ErrorCode::TimedOut, "no answer in time");
        let mut body = resp.into_body();
        if !event_stream {
            let text = body.with_config().limit(MAX_LINE as u64).read_to_string().map_err(|e| if timed_out(&e) { late() } else { unreadable(&e) })?;
            let v: Value = serde_json::from_str(&text).map_err(|e| unreadable(&e))?;
            // A batch is allowed too.
            let found = match v {
                Value::Array(items) => items.into_iter().find(|m| answers(m, id)),
                m => Some(m).filter(|m| answers(m, id)),
            };
            return found.ok_or_else(|| unreadable(&"it didn't answer the request"));
        }
        // Server-sent events: `data:` lines make up each event; the one answering us ends it.
        let reader = std::io::BufReader::new(body.with_config().limit(MAX_LINE as u64).reader());
        let mut data = String::new();
        for line in reader.lines() {
            if cancelled() {
                return Err(ToolError::cancelled());
            }
            let line = line.map_err(|e| if io_timed_out(&e) { late() } else { unreadable(&e) })?;
            if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.strip_prefix(' ').unwrap_or(d));
                data.push('\n');
            } else if line.is_empty() && !data.is_empty() {
                if let Ok(m) = serde_json::from_str::<Value>(data.trim())
                    && answers(&m, id)
                {
                    return Ok(m);
                }
                data.clear();
            }
        }
        match serde_json::from_str::<Value>(data.trim()) {
            Ok(m) if answers(&m, id) => Ok(m),
            _ => Err(unreadable(&"the event stream ended without an answer")),
        }
    }
}

/// Redirects a plugin may take for one message.
const MAX_REDIRECTS: usize = 5;

/// Whether a plugin may be sent its message again at `next`, the target of a redirect: for an
/// https plugin it must be `https:` and pass [`url_allowed`] (a redirect to plain HTTP is
/// refused); for a plain-http plugin it must be `http:` on this computer. The target stays out
/// of every error: it can carry tokens or signed parameters.
fn next_hop_allowed(name: &str, https: bool, next: &str) -> Result<(), ToolError> {
    let Some(scheme) = next.parse::<ureq::http::Uri>().ok().and_then(|u| u.scheme_str().map(str::to_ascii_lowercase)) else {
        return Err(ToolError::new(
            ErrorCode::EngineMissing,
            format!("The {name} plugin sent an HTTP answer that couldn't be read, such as a malformed redirect."),
        )
        .with_remediation("Check its url in config.toml ([[plugins.mcp]]) and the server's redirects."));
    };
    match (https, scheme.as_str()) {
        (true, "https") | (false, "http") if url_allowed(next) => Ok(()),
        (true, "http") => Err(ToolError::new(ErrorCode::EngineMissing, format!("The {name} plugin redirected to plain HTTP and was refused."))
            .with_remediation("Keep every redirect for a remote MCP plugin on HTTPS.")),
        (true, _) => Err(ToolError::new(
            ErrorCode::EngineMissing,
            format!("The {name} plugin redirected to an address that isn't a valid https:// address, and was refused."),
        )
        .with_remediation("Set its url in config.toml ([[plugins.mcp]]) to the final address.")),
        (false, _) => Err(ToolError::new(
            ErrorCode::EngineMissing,
            format!("The {name} plugin redirected to an address other than http:// on this computer, and was refused."),
        )
        .with_remediation("Set its url in config.toml ([[plugins.mcp]]) to the final address, or keep its redirects on this computer.")),
    }
}

/// Resolves a `Location` header against the URL that answered it: absolute, scheme-relative,
/// absolute-path, query-only, fragment-only, or path-relative, with `.` and `..` segments removed (RFC 3986
/// section 5.2.4).
fn resolve_location(base: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty() {
        return None;
    }
    let absolute = loc.split_once(':').is_some_and(|(scheme, _)| {
        scheme.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    });
    if absolute {
        return Some(without_dot_segments(loc));
    }
    let uri = base.parse::<ureq::http::Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if loc.starts_with("//") {
        return Some(without_dot_segments(&format!("{scheme}:{loc}")));
    }
    let origin = format!("{scheme}://{}", uri.authority()?.as_str());
    let path = uri.path();
    let joined = if loc.starts_with('/') {
        format!("{origin}{loc}")
    } else if loc.starts_with('?') {
        // The base path is kept exactly as it was sent.
        return Some(format!("{origin}{path}{loc}"));
    } else if loc.starts_with('#') {
        // A fragment isn't sent, so this is the same request target again.
        return Some(format!("{origin}{}", uri.path_and_query().map_or(path, |p| p.as_str())));
    } else {
        let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
        format!("{origin}{dir}{loc}")
    };
    Some(without_dot_segments(&joined))
}

/// `url` with `.` and `..` removed from its path. Anything without `scheme://` is left as it is.
fn without_dot_segments(url: &str) -> String {
    let Some(after) = url.find("://").map(|i| i + 3) else { return url.to_string() };
    let path_start = url[after..].find(['/', '?', '#']).map_or(url.len(), |i| after + i);
    let path_end = url[path_start..].find(['?', '#']).map_or(url.len(), |i| path_start + i);
    let path = &url[path_start..path_end];
    if path.is_empty() {
        return url.to_string();
    }
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, s) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *s {
            "." | ".." => {
                if *s == ".." {
                    out.pop();
                }
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("{}/{}{}", &url[..path_start], out.join("/"), &url[path_end..])
}

/// The scheme, host and port of `url` (the scheme's default port when none is written), lowercased,
/// so redirect targets can be compared with the configured server.
fn origin(url: &str) -> Option<(String, String, u16)> {
    let uri = url.trim().parse::<ureq::http::Uri>().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let port = match (uri.port_u16(), scheme.as_str()) {
        (Some(p), _) => p,
        (None, "http") => 80,
        (None, "https") => 443,
        (None, _) => return None,
    };
    Some((scheme, uri.host()?.to_ascii_lowercase(), port))
}

/// Whether a ureq error is its timeout, also when a body reader wrapped it in an I/O error.
fn timed_out(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(e) => io_timed_out(e),
        _ => false,
    }
}

fn io_timed_out(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::TimedOut || e.get_ref().and_then(|i| i.downcast_ref::<ureq::Error>()).is_some_and(timed_out)
}

/// `https://`, or `http://` to this computer only (keys must not cross the network in the clear).
pub fn url_allowed(url: &str) -> bool {
    let trimmed = url.trim();
    let Ok(uri) = trimmed.parse::<ureq::http::Uri>() else { return false };
    let Some(scheme) = uri.scheme_str() else { return false };
    let Some(authority) = uri.authority() else { return false };
    let auth = authority.as_str();
    if auth.contains('@') {
        return false;
    }
    let valid_port = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok();
    if scheme.eq_ignore_ascii_case("https") {
        if let Some(after) = auth.strip_prefix('[') {
            after
                .split_once(']')
                .is_some_and(|(ip, tail)| ip.parse::<std::net::Ipv6Addr>().is_ok() && (tail.is_empty() || tail.strip_prefix(':').is_some_and(valid_port)))
        } else {
            let (host, port) = match auth.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (auth, None),
            };
            !host.is_empty() && !host.contains(['[', ']']) && port.is_none_or(valid_port)
        }
    } else if scheme.eq_ignore_ascii_case("http") {
        !auth.contains('+') && craft_anything_platform::standin_server::host_is_loopback(auth, None)
    } else {
        false
    }
}

/// The text parts of a tool result (`content: [{type: "text", text}]`), joined.
pub fn content_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_read_their_text_and_tools_parse_loosely() {
        let r = json!({"content": [{"type": "text", "text": "one"}, {"type": "image", "data": "…"}, {"type": "text", "text": "two"}]});
        assert_eq!(content_text(&r), "one\ntwo");
        assert_eq!(content_text(&json!({})), "");
        let t: RemoteTool = serde_json::from_value(json!({"name": "echo", "inputSchema": {"type": "object"}, "extra": 1})).unwrap();
        assert_eq!(t.name, "echo");
        assert!(t.annotations.is_null());
    }

    #[test]
    fn a_server_that_stops_answering_keeps_what_it_printed() {
        #[cfg(windows)]
        let (command, args) = (
            "powershell",
            vec!["-NoProfile".to_string(), "-Command".to_string(), "[Console]::Error.WriteLine('still loading 3 plugins'); Start-Sleep 60".to_string()],
        );
        #[cfg(not(windows))]
        let (command, args) = ("sh", vec!["-c".to_string(), "echo still loading 3 plugins >&2; sleep 60".to_string()]);
        let cmd = ServerCommand { command: command.into(), args, env: BTreeMap::new(), cwd: None };
        let started = Instant::now();
        let err = McpClient::start("demo", &cmd, Duration::from_secs(5)).err().unwrap();
        assert_eq!(err.code, ErrorCode::TimedOut, "{err:?}");
        assert!(err.details.as_ref().and_then(|d| d["output_tail"].as_str()).is_some_and(|t| t.contains("still loading 3 plugins")), "{err:?}");
        assert!(err.remediation.unwrap_or_default().contains("What it printed last:\nstill loading 3 plugins"));
        assert!(started.elapsed() < Duration::from_secs(20), "the server is stopped, not waited for");
    }

    #[test]
    fn a_missing_server_is_a_clear_error() {
        let cmd = ServerCommand { command: "craft-anything-no-such-server".into(), args: vec![], env: BTreeMap::new(), cwd: None };
        let err = McpClient::start("demo", &cmd, Duration::from_secs(5)).err().unwrap();
        assert!(err.remediation.unwrap_or_default().contains("plugins.mcp"));
    }

    #[test]
    fn only_https_or_this_computer() {
        assert!(url_allowed("https://mcp.example.com/mcp"));
        assert!(url_allowed("http://localhost:8080/mcp") && url_allowed("http://127.0.0.1/mcp") && url_allowed("http://[::1]:9000/mcp"));
        assert!(!url_allowed("http://example.com/mcp") && !url_allowed("http://localhost.evil.com/") && !url_allowed("http://evil.com@x/"));
        assert!(!url_allowed("ftp://x") && !url_allowed("https://"));
    }

    /// A tiny Streamable HTTP server on loopback: JSON for initialize (with a session id), 202 for
    /// notifications, an event stream for tools/list, JSON for tools/call. It checks the headers.
    fn http_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
        http_server_with(4, false)
    }

    /// `http_server` answering `accepts` connections. With `slash_redirect`, a POST to `/mcp`
    /// gets a 307 to `/mcp/` (as many local frameworks send) and is recorded as `307 <method>`.
    fn http_server_with(accepts: usize, slash_redirect: bool) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            // A request that never comes ends the trace instead of hanging the test.
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            'conns: for _ in 0..accepts {
                let mut s = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                        Err(_) => break 'conns,
                    }
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = match s.read(&mut chunk) {
                        Ok(n) if n > 0 => n,
                        _ => break 'conns,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((h, b)) = text.split_once("\r\n\r\n") {
                        let len: usize =
                            h.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                        if b.len() >= len {
                            break (h.to_string(), b.to_string());
                        }
                    }
                };
                let msg: Value = serde_json::from_str(&body).unwrap();
                let method = msg["method"].as_str().unwrap_or_default().to_string();
                let lower = head.to_ascii_lowercase();
                let auth = lower.contains("authorization: bearer t");
                if slash_redirect && lower.starts_with("post /mcp ") {
                    seen.push(format!("307 {method} auth={auth}"));
                    s.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /mcp/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                seen.push(format!("{method} session={} auth={auth}", lower.contains("mcp-session-id: s-1")));
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let json_reply = |result: Value, extra: &str| {
                    let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}", body.len())
                };
                let reply = match method.as_str() {
                    "initialize" => json_reply(json!({"protocolVersion": PROTOCOL, "capabilities": {}}), "Mcp-Session-Id: s-1\r\n"),
                    "notifications/initialized" => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".to_string(),
                    "tools/list" => {
                        // A notification first, then the answer, as an event stream.
                        let ev = json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}});
                        let body = format!("event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\ndata: {ev}\n\n");
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                    }
                    _ => json_reply(json!({"content": [{"type": "text", "text": "hi"}]}), ""),
                };
                s.write_all(reply.as_bytes()).unwrap();
            }
            seen
        });
        (url, handle)
    }

    #[test]
    fn a_remote_server_answers_over_http() {
        let (url, server) = http_server();
        let headers = BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]);
        let mut c = McpClient::connect("remote", &Server::Url(ServerUrl { url, headers }), Duration::from_secs(10)).unwrap();
        let tools = c.list_tools(Duration::from_secs(10)).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let r = c.call_tool("echo", json!({}), Duration::from_secs(10), &|| false).unwrap();
        assert_eq!(content_text(&r), "hi");
        let seen = server.join().unwrap();
        assert_eq!(seen[0], "initialize session=false auth=true");
        assert!(seen[1..].iter().all(|s| s.ends_with("session=true auth=true")), "the session id goes back with every later message: {seen:?}");
        let err =
            McpClient::connect("remote", &Server::Url(ServerUrl { url: "http://example.com/mcp".into(), headers: BTreeMap::new() }), Duration::from_secs(1))
                .err()
                .unwrap();
        assert!(err.message.contains("https://"));
    }

    /// Malformed or ambiguous authorities must not slip past `url_allowed`, while uppercase
    /// schemes, valid ports, and surrounding whitespace remain accepted.
    #[test]
    fn url_allowed_rejects_authority_confusion_and_empty_hosts() {
        for good in [
            "HTTPS://MCP.EXAMPLE.COM:8443/mcp?v=1",
            "  https://mcp.example.com  ",
            "https://[::1]/mcp",
            "https://[::1]:8443/mcp",
            "HTTP://LOCALHOST:8080/mcp",
            "http://127.0.0.1",
            "http://127.0.0.1:0/mcp",
            "http://[::1]:9000/mcp",
        ] {
            assert!(url_allowed(good), "expected allowed: {good:?}");
        }
        for bad in [
            "http://[::1].evil.example.com/mcp",
            "http://[::1/mcp",
            "http://[::1]:bad/mcp",
            "http://[::1]:+9000/mcp",
            "http://[::1]:80:90/mcp",
            r"http://evil.example.com\@127.0.0.1/mcp",
            "http://user@127.0.0.1/mcp",
            "http://localhost:80:evil/mcp",
            "http://127.0.0.1:notaport/mcp",
            "http://127.0.0.1:+8080/mcp",
            "http://127.0.0.1:/mcp",
            "http:///mcp",
            "https:///mcp",
            "https://?q=1",
            "https://#frag",
            "https://user@mcp.example.com/mcp",
            "https://:443/mcp",
            "https://mcp.example.com:/mcp",
            "https://mcp.example.com:+443/mcp",
            "https://mcp.example.com:notaport/mcp",
            "https://mcp.example.com:99999/mcp",
            "https://evil[1]/mcp",
            "https://mcp.example.com[not-ip]:443/mcp",
            "https://[not-ip]/mcp",
            "https://[::1].evil.example.com/mcp",
            "https://[::1]:bad/mcp",
            "https://[::1]:+8443/mcp",
            "https://[::1/mcp",
            r"https://mcp.example.com\evil/mcp",
            "https://mcp example.com/mcp",
            "/relative/mcp",
        ] {
            assert!(!url_allowed(bad), "expected rejected: {bad:?}");
        }
    }

    /// Surrounding whitespace accepted by `url_allowed` is trimmed before opening the HTTP client.
    #[test]
    fn trimmed_loopback_url_connects() {
        let (url, server) = http_server();
        let headers = BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]);
        let mut c = McpClient::connect("remote", &Server::Url(ServerUrl { url: format!("  {url}\n"), headers }), Duration::from_secs(10)).unwrap();
        assert_eq!(c.list_tools(Duration::from_secs(10)).unwrap().len(), 1);
        let _ = c.call_tool("echo", json!({}), Duration::from_secs(10), &|| false).unwrap();
        let _ = server.join().unwrap();
    }

    /// Drain an incoming HTTP/1.1 request (headers plus `Content-Length` body, bounded in bytes
    /// and time) before replying so closing the socket never races unread `POST` bytes with a RST.
    /// Returns what was read.
    fn drain_http_request(stream: &mut impl std::io::Read) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while buf.len() < 32 * 1024 && Instant::now() < deadline {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]);
                        let len: usize =
                            head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())).unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A plain HTTP plugin on this computer that 307s to `/mcp/` on the same listener is followed,
    /// and the body and headers are sent again.
    #[test]
    fn loopback_http_follows_a_trailing_slash_redirect() {
        let (url, server) = http_server_with(4, true);
        let headers = BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]);
        McpClient::connect("local", &Server::Url(ServerUrl { url, headers }), Duration::from_secs(10)).unwrap();
        assert_eq!(
            server.join().unwrap(),
            [
                "307 initialize auth=true",
                "initialize session=false auth=true",
                "307 notifications/initialized auth=true",
                "notifications/initialized session=true auth=true"
            ]
        );
    }

    /// A plain HTTP plugin on this computer that redirects off it is refused before the headers
    /// and body go anywhere, and the error says why without echoing the target.
    #[test]
    fn loopback_http_refuses_a_redirect_off_this_computer() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let redirect_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        redirect_listener.set_nonblocking(true).unwrap();
        let redirect_url = format!("http://{}/mcp", redirect_listener.local_addr().unwrap());
        let stop_redirect = stop.clone();
        let redirect_worker = std::thread::spawn(move || {
            while !stop_redirect.load(Ordering::SeqCst) {
                if let Ok((mut s, _)) = redirect_listener.accept() {
                    s.set_nonblocking(false).unwrap();
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                    drain_http_request(&mut s);
                    let reply =
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://192.0.2.1/steal?sig=private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = s.write_all(reply.as_bytes());
                    let _ = s.flush();
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let secret_headers = BTreeMap::from([("X-Api-Key".to_string(), "sk-test".to_string())]);
        let res = McpClient::connect("demo", &Server::Url(ServerUrl { url: redirect_url, headers: secret_headers }), Duration::from_secs(5));
        stop.store(true, Ordering::SeqCst);
        redirect_worker.join().unwrap();

        let err = res.err().unwrap();
        assert!(err.message.contains("other than http:// on this computer"), "{err:?}");
        assert!(!format!("{err:?}").contains("192.0.2.1") && !format!("{err:?}").contains("private"), "redirect target leaked: {err:?}");
    }

    /// A loopback server that answers each POST, after `delay`, with a 307 to `location`, and
    /// returns how many requests it saw (at most `accepts`).
    fn redirect_server(location: String, delay: Duration, accepts: usize) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut seen = 0;
            while seen < accepts && Instant::now() < deadline {
                let Ok((mut s, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                s.set_nonblocking(false).unwrap();
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                drain_http_request(&mut s);
                seen += 1;
                std::thread::sleep(delay);
                let reply = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = s.write_all(reply.as_bytes());
            }
            seen
        });
        (url, handle)
    }

    fn http_to(url: &str) -> Http {
        let headers = BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]);
        Http::new("local", &ServerUrl { url: url.to_string(), headers }).unwrap()
    }

    fn soon(d: Duration) -> Instant {
        Instant::now() + d
    }

    /// A redirect to another port on this computer is followed, but the configured headers and
    /// session stay with the configured server. A session the other address starts can't be sent
    /// back to it, so that handshake is refused; an answer without a session is used.
    #[test]
    fn loopback_redirect_to_another_origin_drops_configured_headers() {
        let (target, server) = http_server_with(2, false);
        let (url, redirector) = redirect_server(target, Duration::ZERO, 2);
        let mut http = http_to(&url);
        http.session = Some("s-1".into());
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let err = http.post(&init, soon(Duration::from_secs(10)), &|| false).err().unwrap();
        assert!(err.message.contains("started a session"), "{err:?}");
        assert!(err.remediation.as_deref().is_some_and(|r| r.contains("final address")), "{err:?}");
        assert!(!format!("{err:?}").contains("127.0.0.1"), "redirect target leaked: {err:?}");
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
        http.post(&list, soon(Duration::from_secs(10)), &|| false).unwrap();
        assert_eq!(redirector.join().unwrap(), 2);
        assert_eq!(server.join().unwrap(), ["initialize session=false auth=false", "tools/list session=false auth=false"]);
    }

    /// Ports are compared by value: `http://127.0.0.1/` and `http://127.0.0.1:80/` are one origin.
    #[test]
    fn origins_compare_the_effective_port() {
        assert_eq!(origin("http://127.0.0.1/mcp"), origin("http://127.0.0.1:80/mcp/"));
        assert_eq!(origin("HTTP://LOCALHOST:8080/a"), origin("http://localhost:8080/b"));
        assert_ne!(origin("http://127.0.0.1/mcp"), origin("http://127.0.0.1:8080/mcp"));
        assert_ne!(origin("http://127.0.0.1:443/mcp"), origin("https://127.0.0.1/mcp"));
    }

    /// A deadline under a second isn't stretched: a redirect that arrives after it isn't followed.
    #[test]
    fn a_short_deadline_is_not_extended_for_a_redirect() {
        let (url, redirector) = redirect_server("/mcp/".into(), Duration::from_millis(300), 2);
        let msg = json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {}});
        let err = http_to(&url).post(&msg, soon(Duration::from_millis(200)), &|| false).err().unwrap();
        assert_eq!(err.code, ErrorCode::TimedOut, "{err:?}");
        assert_eq!(redirector.join().unwrap(), 1, "the redirect target must not be sent the call after the deadline");
    }

    /// A loopback server that reads one request, sends `head` and nothing more, until the test drops the sender.
    fn silent_server(head: &'static str) -> (String, std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (stop, stopped) = channel::<()>();
        let handle = std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else { return };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            drain_http_request(&mut s);
            let _ = s.write_all(head.as_bytes());
            let _ = s.flush();
            let _ = stopped.recv_timeout(Duration::from_secs(40));
        });
        (url, stop, handle)
    }

    /// A call cancelled after the server got it is followed by a notice naming that request. The
    /// notice is best effort: a server that takes it and never answers doesn't keep the caller
    /// waiting.
    #[test]
    fn a_cancelled_call_names_its_request_and_does_not_wait_long_for_the_notice() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let got_call = Arc::new(AtomicBool::new(false));
        let flag = got_call.clone();
        let (stop, stopped) = channel::<()>();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut bodies, mut held) = (Vec::new(), Vec::new());
            while bodies.len() < 2 && Instant::now() < deadline {
                let Ok((mut s, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                s.set_nonblocking(false).unwrap();
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let request = drain_http_request(&mut s);
                bodies.push(serde_json::from_str::<Value>(request.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap_or(Value::Null));
                if bodies.len() == 1 {
                    // The call has arrived: cancel it while its answer is still coming.
                    flag.store(true, Ordering::SeqCst);
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000\r\n\r\n: working\n");
                    let _ = s.flush();
                }
                held.push(s);
            }
            let _ = stopped.recv_timeout(Duration::from_secs(40));
            bodies
        });
        let mut client = McpClient { name: "local".into(), transport: Box::new(http_to(&url)), next_id: 1 };
        let started = Instant::now();
        let err = client.call_tool("echo", json!({}), Duration::from_secs(10), &|| got_call.load(Ordering::SeqCst)).err().unwrap();
        let took = started.elapsed();
        drop(stop);
        let bodies = server.join().unwrap();
        assert_eq!(err.code, ErrorCode::Cancelled, "{err:?}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert_eq!((bodies[0]["method"].as_str(), bodies[0]["id"].as_u64()), (Some("tools/call"), Some(1)), "{bodies:?}");
        assert_eq!(bodies[1]["method"].as_str(), Some("notifications/cancelled"), "{bodies:?}");
        assert_eq!(bodies[1]["params"]["requestId"].as_u64(), Some(1), "{bodies:?}");
    }

    /// A call cancelled before it is sent sends nothing, so no notice names a request the server
    /// never got.
    #[test]
    fn a_call_cancelled_before_sending_sends_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let mut client = McpClient { name: "local".into(), transport: Box::new(http_to(&url)), next_id: 1 };
        let err = client.call_tool("echo", json!({}), Duration::from_secs(10), &|| true).err().unwrap();
        assert_eq!(err.code, ErrorCode::Cancelled, "{err:?}");
        assert!(listener.accept().is_err(), "nothing should have connected");
    }

    /// An answer whose body arrives after the deadline is a timeout, for JSON and event streams.
    #[test]
    fn a_late_answer_body_is_a_timeout() {
        for head in [
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100\r\n\r\n",
        ] {
            let (url, stop, server) = silent_server(head);
            let msg = json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list", "params": {}});
            let started = Instant::now();
            let err = http_to(&url).request(&msg, 5, soon(Duration::from_millis(500)), &|| false).err().unwrap();
            let took = started.elapsed();
            drop(stop);
            server.join().unwrap();
            assert_eq!(err.code, ErrorCode::TimedOut, "{head}: {err:?}");
            assert!(took < Duration::from_secs(3), "{head}: took {took:?}");
        }
    }

    /// Cancelling while the first server is answering stops the message from being sent again.
    #[test]
    fn a_cancelled_message_is_not_resent_after_a_redirect() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (url, redirector) = redirect_server(format!("http://{}/mcp", target.local_addr().unwrap()), Duration::ZERO, 1);
        let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {}});
        let err = http_to(&url).post(&msg, soon(Duration::from_secs(10)), &|| true).err().unwrap();
        assert_eq!(err.code, ErrorCode::Cancelled, "{err:?}");
        assert_eq!(redirector.join().unwrap(), 1);
        target.set_nonblocking(true).unwrap();
        assert!(target.accept().is_err(), "the redirect target must not be contacted after cancelling");
    }

    /// One timeout covers every hop: a slow redirect chain fails in time instead of restarting
    /// the clock at each hop.
    #[test]
    fn one_timeout_covers_every_redirect_hop() {
        // Each hop takes 900 ms: with one 1 s budget the second hop is cut off near 1 s; with a
        // fresh budget per hop it would finish at 1.8 s.
        let (url, redirector) = redirect_server("/mcp/".into(), Duration::from_millis(900), 2);
        let msg = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}});
        let started = Instant::now();
        let err = http_to(&url).post(&msg, soon(Duration::from_secs(1)), &|| false).err().unwrap();
        assert_eq!(err.code, ErrorCode::TimedOut, "{err:?}");
        assert!(started.elapsed() < Duration::from_millis(1500), "took {:?}", started.elapsed());
        assert_eq!(redirector.join().unwrap(), 2);
    }

    #[test]
    fn redirect_locations_resolve_against_the_answering_url() {
        let base = "http://127.0.0.1:8080/api/mcp?x=1";
        assert_eq!(resolve_location(base, "../mcp/").as_deref(), Some("http://127.0.0.1:8080/mcp/"));
        assert_eq!(resolve_location(base, "./v2/../mcp?next=a/../b").as_deref(), Some("http://127.0.0.1:8080/api/mcp?next=a/../b"));
        assert_eq!(resolve_location(base, "http://127.0.0.1:9000/a/./b/../c").as_deref(), Some("http://127.0.0.1:9000/a/c"));
        assert_eq!(resolve_location(base, "/..").as_deref(), Some("http://127.0.0.1:8080/"));
        assert_eq!(resolve_location(base, "/mcp/").as_deref(), Some("http://127.0.0.1:8080/mcp/"));
        assert_eq!(resolve_location(base, "mcp/").as_deref(), Some("http://127.0.0.1:8080/api/mcp/"));
        assert_eq!(resolve_location(base, "?y=2").as_deref(), Some("http://127.0.0.1:8080/api/mcp?y=2"));
        // A fragment-only redirect keeps the path and query it answered.
        assert_eq!(resolve_location(base, "#status").as_deref(), Some("http://127.0.0.1:8080/api/mcp?x=1"));
        // A query-only redirect keeps the path it answered, dot segments and all.
        assert_eq!(resolve_location("http://127.0.0.1:8080/api/../mcp?x=1", "?y=2").as_deref(), Some("http://127.0.0.1:8080/api/../mcp?y=2"));
        assert_eq!(resolve_location(base, "//evil.example/mcp").as_deref(), Some("http://evil.example/mcp"));
        assert_eq!(resolve_location(base, "HTTPS://evil.example/").as_deref(), Some("HTTPS://evil.example/"));
        assert_eq!(resolve_location(base, "  "), None);
    }

    const PRIVATE_SIG: &str = "signed-mcp-query-must-stay-private";

    /// An HTTPS MCP plugin (including uppercase `HTTPS://`) refuses a redirect to plain HTTP
    /// before opening the HTTP connection and omits the redirect target URI from the error.
    #[test]
    fn https_downgrade_redirect_is_refused_without_contacting_http() {
        let (err, https_seen, http_seen) = https_redirect_to(|port| format!("http://127.0.0.1:{port}/mcp?sig={PRIVATE_SIG}"));
        assert_eq!(https_seen, 1);
        assert_eq!(http_seen, 0, "downgrade redirect must never contact plain HTTP");
        assert!(err.message.contains("plain HTTP"), "{err:?}");
        assert!(!format!("{err:?}").contains(PRIVATE_SIG), "redirect target query leaked: {err:?}");
    }

    /// A redirect target too malformed to parse stays out of the error as well.
    #[test]
    fn a_malformed_https_redirect_target_is_not_shown() {
        let (err, https_seen, http_seen) = https_redirect_to(|port| format!("http://127.0.0.1:{port}/m cp?sig={PRIVATE_SIG}"));
        assert_eq!((https_seen, http_seen), (1, 0));
        assert_eq!(err.code, ErrorCode::EngineMissing, "a send failure must discard the cached client: {err:?}");
        assert!(err.message.contains("malformed redirect"), "{err:?}");
        assert!(!format!("{err:?}").contains(PRIVATE_SIG), "redirect target query leaked: {err:?}");
    }

    /// Connects an HTTPS plugin whose server answers with a 302 to `location(plain_http_port)`,
    /// and returns the error and how many requests the HTTPS and plain HTTP servers saw.
    fn https_redirect_to(location: impl Fn(u16) -> String) -> (ToolError, usize, usize) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (server_cfg, client_tls) = localhost_tls();
        let http_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        http_listener.set_nonblocking(true).unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        let http_seen = Arc::new(AtomicUsize::new(0));
        let http_seen_clone = http_seen.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_http = stop.clone();
        let http_worker = std::thread::spawn(move || {
            while !stop_http.load(Ordering::SeqCst) {
                if let Ok((mut s, _)) = http_listener.accept() {
                    s.set_nonblocking(false).unwrap();
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    drain_http_request(&mut s);
                    http_seen_clone.fetch_add(1, Ordering::SeqCst);
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });

        let https_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        https_listener.set_nonblocking(true).unwrap();
        let https_port = https_listener.local_addr().unwrap().port();
        let https_seen = Arc::new(AtomicUsize::new(0));
        let https_seen_clone = https_seen.clone();
        let stop_https = stop.clone();
        let target = location(http_port);
        let https_worker = std::thread::spawn(move || {
            while !stop_https.load(Ordering::SeqCst) {
                if let Ok((socket, _)) = https_listener.accept() {
                    socket.set_nonblocking(false).unwrap();
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = socket.set_write_timeout(Some(Duration::from_secs(3)));
                    let conn = rustls::ServerConnection::new(server_cfg.clone()).unwrap();
                    let mut tls_stream = rustls::StreamOwned::new(conn, socket);
                    drain_http_request(&mut tls_stream);
                    https_seen_clone.fetch_add(1, Ordering::SeqCst);
                    let reply = format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = tls_stream.write_all(reply.as_bytes());
                    let _ = tls_stream.flush();
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let mut transport = Http::new(
            "secure",
            &ServerUrl { url: format!("HTTPS://localhost:{https_port}/mcp"), headers: BTreeMap::from([("X-Api-Key".to_string(), "sk-test".to_string())]) },
        )
        .unwrap();
        transport.tls = Some(client_tls);
        let res = McpClient::with_transport("secure", Box::new(transport), Duration::from_secs(5));

        stop.store(true, Ordering::SeqCst);
        https_worker.join().unwrap();
        http_worker.join().unwrap();

        (res.err().unwrap(), https_seen.load(Ordering::SeqCst), http_seen.load(Ordering::SeqCst))
    }

    /// A self-signed certificate for `localhost`: the server's side, and a client that trusts it.
    fn localhost_tls() -> (std::sync::Arc<rustls::ServerConfig>, ureq::tls::TlsConfig) {
        use std::sync::Arc;
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = key.cert.der().clone();
        let private = rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der());
        let server_cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], private.into())
                .unwrap(),
        );
        let client_tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::new_with_certs(&[ureq::tls::Certificate::from_der(cert.as_ref()).to_owned()]))
            .build();
        (server_cfg, client_tls)
    }

    /// A TLS server for `localhost` on a loopback port. It reads each whole request, answers it
    /// with `reply(path)`, and returns every request it read (head and body, lowercased). It
    /// stops after `accepts` requests, once `stop` is set, or after ten seconds.
    fn tls_server(
        cfg: std::sync::Arc<rustls::ServerConfig>,
        accepts: usize,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reply: impl Fn(&str) -> String + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut seen = Vec::new();
            while seen.len() < accepts && !stop.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
                let Ok((socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // An accepted socket keeps the listener's nonblocking mode on Windows.
                socket.set_nonblocking(false).unwrap();
                let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
                let _ = socket.set_write_timeout(Some(Duration::from_secs(3)));
                let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg.clone()).unwrap(), socket);
                let request = drain_http_request(&mut tls).to_ascii_lowercase();
                let path = request.split(' ').nth(1).unwrap_or_default().to_string();
                seen.push(request);
                let _ = tls.write_all(reply(&path).as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
            seen
        });
        (port, handle)
    }

    fn answer_ok() -> String {
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string()
    }

    fn answer_moved(status: &str, location: &str) -> String {
        format!("HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    /// An https plugin at `port` with a key in its headers and a session already started.
    fn https_to(port: u16, tls: ureq::tls::TlsConfig) -> Http {
        let headers = BTreeMap::from([("X-Api-Key".to_string(), "sk-test".to_string())]);
        let mut http = Http::new("secure", &ServerUrl { url: format!("https://localhost:{port}/mcp"), headers }).unwrap();
        http.tls = Some(tls);
        http.session = Some("s-1".into());
        http
    }

    fn list_message() -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {}})
    }

    fn stop_flag() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
    }

    /// An https plugin redirected with a 307 to another https address sends it the message again,
    /// but not its configured headers or its session.
    #[test]
    fn https_redirect_to_another_origin_drops_configured_headers() {
        let (cfg, tls) = localhost_tls();
        let stop = stop_flag();
        let (other, other_seen) = tls_server(cfg.clone(), 1, stop.clone(), |_| answer_ok());
        let target = format!("https://localhost:{other}/mcp");
        let (home, home_seen) = tls_server(cfg, 1, stop.clone(), move |_| answer_moved("307 Temporary Redirect", &target));
        let res = https_to(home, tls).post(&list_message(), soon(Duration::from_secs(10)), &|| false);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let (home_seen, other_seen) = (home_seen.join().unwrap(), other_seen.join().unwrap());
        assert!(res.is_ok(), "{:?}", res.err());
        assert_eq!((home_seen.len(), other_seen.len()), (1, 1), "{home_seen:?} {other_seen:?}");
        assert!(home_seen[0].contains("x-api-key: sk-test") && home_seen[0].contains("mcp-session-id: s-1"), "{home_seen:?}");
        assert!(!other_seen[0].contains("x-api-key") && !other_seen[0].contains("mcp-session-id"), "{other_seen:?}");
        assert!(other_seen[0].contains("\"method\":\"tools/list\""), "the message must be sent again: {other_seen:?}");
    }

    /// A 307 to another path on the configured https address keeps the headers and the session.
    #[test]
    fn https_redirect_on_the_same_origin_keeps_configured_headers() {
        let (cfg, tls) = localhost_tls();
        let stop = stop_flag();
        let (home, seen) =
            tls_server(cfg, 2, stop.clone(), |path| if path == "/mcp" { answer_moved("307 Temporary Redirect", "/mcp/v2") } else { answer_ok() });
        let res = https_to(home, tls).post(&list_message(), soon(Duration::from_secs(10)), &|| false);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let seen = seen.join().unwrap();
        assert!(res.is_ok(), "{:?}", res.err());
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[1].starts_with("post /mcp/v2 "), "{seen:?}");
        assert!(seen.iter().all(|r| r.contains("x-api-key: sk-test") && r.contains("mcp-session-id: s-1")), "{seen:?}");
    }

    /// A 307 from an https plugin to plain HTTP is refused before anything is sent there, and the
    /// error doesn't show the target. So is a 307 to an https address `url_allowed` refuses.
    #[test]
    fn https_redirects_to_refused_targets_are_not_followed_or_shown() {
        let plain = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        plain.set_nonblocking(true).unwrap();
        let plain_port = plain.local_addr().unwrap().port();
        for (target, said) in [
            (format!("http://127.0.0.1:{plain_port}/mcp?sig={PRIVATE_SIG}"), "plain HTTP"),
            (format!("https://user@localhost:{plain_port}/mcp?sig={PRIVATE_SIG}"), "isn't a valid https:// address"),
        ] {
            let (cfg, tls) = localhost_tls();
            let stop = stop_flag();
            let (home, seen) = tls_server(cfg, 1, stop.clone(), move |_| answer_moved("307 Temporary Redirect", &target));
            let err = https_to(home, tls).post(&list_message(), soon(Duration::from_secs(10)), &|| false).err().unwrap();
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(seen.join().unwrap().len(), 1);
            assert!(err.message.contains(said), "{err:?}");
            assert!(!format!("{err:?}").contains(PRIVATE_SIG), "redirect target query leaked: {err:?}");
        }
        assert!(plain.accept().is_err(), "a refused target must not be contacted");
    }

    /// An https plugin follows at most `MAX_REDIRECTS` redirects for one message.
    #[test]
    fn https_redirects_stop_after_the_limit() {
        let (cfg, tls) = localhost_tls();
        let stop = stop_flag();
        let (home, seen) = tls_server(cfg, MAX_REDIRECTS + 2, stop.clone(), |path| answer_moved("307 Temporary Redirect", &format!("{path}x")));
        let err = https_to(home, tls).post(&list_message(), soon(Duration::from_secs(10)), &|| false).err().unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(err.message.contains(&format!("more than {MAX_REDIRECTS} times")), "{err:?}");
        assert_eq!(seen.join().unwrap().len(), MAX_REDIRECTS + 1);
    }

    /// A 302 would turn the POST into a GET, so an https plugin returns it as it is.
    #[test]
    fn an_https_302_is_returned_not_followed() {
        let (cfg, tls) = localhost_tls();
        let stop = stop_flag();
        let (other, other_seen) = tls_server(cfg.clone(), 1, stop.clone(), |_| answer_ok());
        let target = format!("https://localhost:{other}/mcp");
        let (home, home_seen) = tls_server(cfg, 1, stop.clone(), move |_| answer_moved("302 Found", &target));
        let err = https_to(home, tls).post(&list_message(), soon(Duration::from_secs(10)), &|| false).err().unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(err.message.contains("answered 302"), "{err:?}");
        assert_eq!((home_seen.join().unwrap().len(), other_seen.join().unwrap().len()), (1, 0));
    }
}
