//! A local control panel: live status, settings, and Keychain login.
//!
//! Blocking HTTP on `tiny_http`, because the bot has no async runtime and a loopback
//! panel for a handful of people does not need one.
//!
//! The config file stays the source of truth. This reads it fresh on every request
//! and rewrites it in place on a save, then asks the bot to reload -- so a change made
//! here and a change made in an editor are the same kind of change.

mod auth;
mod edit;

use std::io::Cursor;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, StatusCode};
use tracing::{info, warn};

use crate::config::{Config, Settings};
use crate::state::Shared;

pub use auth::Auth;

/// Mutating requests must carry this. Combined with a `SameSite=Strict` cookie it
/// closes cross-site request forgery: a form posted from another origin cannot set a
/// custom header, and a plain navigation will not send the cookie anyway.
const CSRF_HEADER: &str = "x-terracore-ui";
const SESSION_COOKIE: &str = "tc_session";

/// Refuse a body larger than this before reading it. Every request this server takes
/// is a small JSON object.
const MAX_BODY: usize = 64 * 1024;

struct Panel {
    config_path: PathBuf,
    wallet_path: PathBuf,
    shared: Arc<Shared>,
    auth: Auth,
}

/// A running panel. Production only needs to know it started; the tests need to
/// reach it, which is why the address and the panel itself come back.
pub struct Running {
    /// Where it actually bound, which is not the configured address when that
    /// asked for port 0.
    pub addr: std::net::SocketAddr,
    /// Held so the tests can mint a session and reach the same instance the server
    /// threads are serving. Production has no use for it.
    #[cfg_attr(not(test), allow(dead_code))]
    panel: Arc<Panel>,
}

/// Start the panel on its own threads. Returns once it is listening.
pub fn spawn(config: &Config, shared: Arc<Shared>) -> Result<Running> {
    let bind = config.web.bind.clone();
    let address = bind
        .to_socket_addrs()
        .with_context(|| format!("[web] bind `{bind}` is not an address"))?
        .next()
        .with_context(|| format!("[web] bind `{bind}` resolved to nothing"))?;

    // Saying this once, loudly, is the honest thing: this panel can change what the
    // accounts do, and it authenticates with a cookie over whatever transport it is
    // given.
    if !address.ip().is_loopback() {
        warn!(
            %bind,
            "the control panel is NOT bound to loopback -- put a TLS reverse proxy in \
             front of it, or anyone who can reach this port can try to log in"
        );
    }

    let auth = Auth::new(
        config.hive.nodes.clone(),
        Duration::from_secs(config.hive.timeout_secs),
        Duration::from_secs(config.web.session_hours * 3600),
        config.web.access.clone(),
    )?;

    let panel = Arc::new(Panel {
        config_path: config.path.clone(),
        wallet_path: config.wallet_path(),
        shared,
        auth,
    });

    let server = tiny_http::Server::http(address)
        .map_err(|e| anyhow::anyhow!("could not listen on {bind}: {e}"))?;
    // Read back rather than reused: a bind of `:0` resolves to whatever port the OS
    // handed out, and that is what a caller has to connect to.
    let bound = server
        .server_addr()
        .to_ip()
        .context("the panel bound to something that is not an IP socket")?;
    let server = Arc::new(server);

    // A small fixed pool rather than a thread per request: a login waits on a Hive
    // node for a second or so, and one slow login should not hold up the status poll.
    for _ in 0..4 {
        let server = Arc::clone(&server);
        let panel = Arc::clone(&panel);
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                panel.handle(request);
            }
        });
    }

    info!(bind = %bound, roles = config.web.access.len(), "control panel listening");
    Ok(Running { addr: bound, panel })
}

impl Panel {
    fn handle(&self, mut request: Request) {
        let method = request.method().clone();
        // Strip the query string; no route here reads one except the log tail, which
        // takes its limit from the path-free parsing below.
        let url = request.url().to_string();
        let (path, query) = match url.split_once('?') {
            Some((path, query)) => (path.to_string(), query.to_string()),
            None => (url, String::new()),
        };

        let response = match self.route(&method, &path, &query, &mut request) {
            Ok(response) => response,
            // Any error that escapes a handler is reported as such rather than
            // dropping the connection, so the UI can show the reason.
            Err(e) => json_response(StatusCode(500), &json!({ "error": format!("{e:#}") })),
        };

        if let Err(e) = request.respond(response) {
            // The client hung up. Normal, and not worth more than a debug line.
            tracing::debug!(error = %e, "could not send a response");
        }
    }

    fn route(
        &self,
        method: &Method,
        path: &str,
        query: &str,
        request: &mut Request,
    ) -> Result<Response<Cursor<Vec<u8>>>> {
        match (method, path) {
            (Method::Get, "/") | (Method::Get, "/index.html") => Ok(html(include_str!("ui.html"))),

            // --- unauthenticated ------------------------------------------
            (Method::Post, "/api/challenge") => {
                if let Some(refused) = self.require_ui_header(request) {
                    return Ok(refused);
                }
                #[derive(Deserialize)]
                struct Body {
                    account: String,
                }
                let body: Body = read_json(request)?;
                match self.auth.challenge(&body.account) {
                    Ok(message) => Ok(ok(&json!({ "message": message }))),
                    Err(e) => Ok(error(StatusCode(403), &e)),
                }
            }

            (Method::Post, "/api/login") => {
                if let Some(refused) = self.require_ui_header(request) {
                    return Ok(refused);
                }
                #[derive(Deserialize)]
                struct Body {
                    message: String,
                    signature: String,
                }
                let body: Body = read_json(request)?;
                match self.auth.login(&body.message, &body.signature) {
                    Ok((token, session)) => {
                        info!(account = %session.account, role = session.role.as_str(), "login");
                        let mut response = ok(&json!({
                            "account": session.account,
                            "role": session.role.as_str(),
                            "expires_at": session.expires_at,
                        }));
                        response.add_header(session_cookie(&token, false));
                        Ok(response)
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "login refused");
                        Ok(error(StatusCode(401), &e))
                    }
                }
            }

            (Method::Post, "/api/logout") => {
                if let Some(refused) = self.require_ui_header(request) {
                    return Ok(refused);
                }
                if let Some(token) = cookie(request, SESSION_COOKIE) {
                    self.auth.logout(&token);
                }
                let mut response = ok(&json!({ "ok": true }));
                response.add_header(session_cookie("", true));
                Ok(response)
            }

            // --- authenticated --------------------------------------------
            (Method::Get, "/api/me") => {
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                Ok(ok(&json!({
                    "account": session.account,
                    "role": session.role.as_str(),
                    "expires_at": session.expires_at,
                })))
            }

            (Method::Get, "/api/status") => {
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                let board = self.shared.status().clone();
                let accounts: serde_json::Map<String, Value> = board
                    .accounts
                    .iter()
                    .filter(|(name, _)| session.role.may_read(&session.account, name))
                    .map(|(name, status)| (name.clone(), serde_json::to_value(status).unwrap_or(Value::Null)))
                    .collect();
                let control = self.shared.control().clone();
                Ok(ok(&json!({
                    "cycle": board.cycle,
                    "cycle_started": board.cycle_started,
                    "cycle_finished": board.cycle_finished,
                    "running": board.running,
                    "dry_run": board.dry_run,
                    "blacklist_size": board.blacklist_size,
                    "paused": control.paused,
                    "accounts": accounts,
                    "now": crate::state::epoch_secs(),
                })))
            }

            (Method::Get, "/api/config") => {
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                let text = std::fs::read_to_string(&self.config_path)
                    .context("reading the config")?;
                let config = Config::from_str(&text)?;
                let defaults = Config::default_settings(&text)?;

                let accounts: Vec<Value> = config
                    .accounts
                    .iter()
                    .filter(|a| session.role.may_read(&session.account, &a.name))
                    .map(|a| {
                        json!({
                            "name": a.name,
                            "enabled": a.enabled,
                            "writable": session.role.may_write(&session.account, &a.name),
                            "settings": a.settings,
                        })
                    })
                    .collect();

                Ok(ok(&json!({
                    "defaults": defaults,
                    "accounts": accounts,
                    "general": {
                        "cycle_interval_secs": config.general.cycle_interval_secs,
                        "dry_run": config.general.dry_run,
                    },
                })))
            }

            (Method::Get, "/api/keys") => {
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                // Metadata only, and never a secret: which accounts hold which roles
                // is stored in the clear precisely so this question is cheap.
                let held = crate::keys::list(&self.wallet_path).unwrap_or_default();
                let filtered: serde_json::Map<String, Value> = held
                    .into_iter()
                    .filter(|(name, _)| session.role.may_read(&session.account, name))
                    .map(|(name, roles)| (name, json!(roles)))
                    .collect();
                Ok(ok(&json!({ "keys": filtered })))
            }

            (Method::Get, "/api/log") => {
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                // The log is cross-account by nature, so it is admin-only rather than
                // filtered into something misleading.
                if !session.role.is_admin() {
                    return Ok(forbidden("reading the log needs the admin role"));
                }
                let limit = query_value(query, "limit")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(200)
                    .clamp(1, 2000);
                let lines = self.shared.log().tail(limit);
                Ok(ok(&json!({ "lines": lines })))
            }

            (Method::Post, "/api/control") => {
                if let Some(refused) = self.require_ui_header(request) {
                    return Ok(refused);
                }
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                if !session.role.is_admin() {
                    return Ok(forbidden("controlling the bot needs the admin role"));
                }
                #[derive(Deserialize)]
                struct Body {
                    action: String,
                }
                let body: Body = read_json(request)?;
                let mut control = self.shared.control();
                match body.action.as_str() {
                    "pause" => control.paused = true,
                    "resume" => control.paused = false,
                    "run_now" => control.run_now = true,
                    other => {
                        return Ok(error(
                            StatusCode(400),
                            &anyhow::anyhow!("unknown action `{other}`"),
                        ))
                    }
                }
                info!(by = %session.account, action = %body.action, "control");
                Ok(ok(&json!({ "ok": true, "paused": control.paused })))
            }

            (Method::Put, path) if path.starts_with("/api/accounts/") => {
                if let Some(refused) = self.require_ui_header(request) {
                    return Ok(refused);
                }
                let Some(session) = self.session(request) else {
                    return Ok(unauthorized());
                };
                // `strip_prefix`, not `trim_start_matches`: the latter strips the
                // pattern repeatedly, so `/api/accounts//api/accounts/alice` would
                // have resolved to `alice`.
                let name = path.strip_prefix("/api/accounts/").unwrap_or_default().to_string();
                // Nothing downstream decodes percent-escapes, so the name has to be
                // one that never needs them. Hive account names never do.
                if !is_hive_account_name(&name) {
                    return Ok(error(
                        StatusCode(400),
                        &anyhow::anyhow!("`{name}` is not a Hive account name"),
                    ));
                }
                if !session.role.may_write(&session.account, &name) {
                    return Ok(forbidden(&format!(
                        "the {} role cannot change @{name}",
                        session.role.as_str()
                    )));
                }

                #[derive(Deserialize)]
                struct Body {
                    enabled: bool,
                    settings: Settings,
                }
                let body: Body = read_json(request)?;

                match self.save_account(&name, &body.settings, body.enabled) {
                    Ok(()) => {
                        info!(by = %session.account, account = %name, "settings saved");
                        Ok(ok(&json!({ "ok": true })))
                    }
                    Err(e) => Ok(error(StatusCode(400), &e)),
                }
            }

            _ => Ok(json_response(
                StatusCode(404),
                &json!({ "error": "no such endpoint" }),
            )),
        }
    }

    /// Rewrite the config and ask the bot to pick it up.
    ///
    /// Written to a temporary file and renamed, so a crash or a full disk leaves the
    /// old config intact rather than a half-written one -- this file is the only
    /// record of what every account is meant to do.
    fn save_account(&self, name: &str, settings: &Settings, enabled: bool) -> Result<()> {
        let text = std::fs::read_to_string(&self.config_path).context("reading the config")?;
        let updated = edit::apply(&text, name, settings, enabled)?;

        // Parsing what is about to be written is the check that matters: a config the
        // bot cannot load would stop it at the next reload.
        Config::from_str(&updated).context("the edited config would not load")?;

        write_atomically(&self.config_path, &updated)?;
        self.shared.control().reload = true;
        Ok(())
    }

    fn session(&self, request: &Request) -> Option<auth::Session> {
        let token = cookie(request, SESSION_COOKIE)?;
        self.auth.session(&token)
    }

    /// Present on every request the UI makes, absent on every request another site
    /// could make on a victim's behalf.
    fn require_ui_header(&self, request: &Request) -> Option<Response<Cursor<Vec<u8>>>> {
        let present = request.headers().iter().any(|h| h.field.equiv(CSRF_HEADER));
        if present {
            None
        } else {
            Some(forbidden(&format!(
                "requests that change something must carry the {CSRF_HEADER} header"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T> {
    let length = request.body_length().unwrap_or(0);
    if length > MAX_BODY {
        anyhow::bail!("request body is too large");
    }
    // Capped regardless of what Content-Length claimed: the length is the client's
    // word for it, and this is the only place the server allocates on their say-so.
    let mut body = String::with_capacity(length.min(MAX_BODY));
    let reader = request.as_reader();
    std::io::Read::read_to_string(&mut std::io::Read::take(reader, MAX_BODY as u64), &mut body)
        .context("reading the request body")?;
    serde_json::from_str(&body).context("the request body is not the JSON this endpoint expects")
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn cookie(request: &Request, name: &str) -> Option<String> {
    let header = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))?
        .value
        .as_str()
        .to_string();
    cookie_value(&header, name)
}

/// Pull one cookie out of a `Cookie:` header.
///
/// Separate from [`cookie`] so a test can call the parser rather than restate it.
/// The previous test held its own copy of these three lines, which meant it would
/// have passed however the real one behaved.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// Hive account names: 3-16 characters of lowercase letters, digits, `.` and `-`.
fn is_hive_account_name(name: &str) -> bool {
    (3..=16).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

fn session_cookie(token: &str, clear: bool) -> Header {
    // HttpOnly so no script can read it, SameSite=Strict so no other origin can cause
    // it to be sent. `Secure` is deliberately not set: the default bind is plain HTTP
    // on loopback, where `Secure` would stop the cookie working at all. Behind a TLS
    // proxy, have the proxy add it.
    let value = if clear {
        format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
    } else {
        format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/")
    };
    Header::from_bytes(&b"Set-Cookie"[..], value.as_bytes())
        .expect("a well-formed Set-Cookie header")
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("a well-formed header")
}

fn json_response(status: StatusCode, value: &Value) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn ok(value: &Value) -> Response<Cursor<Vec<u8>>> {
    json_response(StatusCode(200), value)
}

fn error(status: StatusCode, e: &anyhow::Error) -> Response<Cursor<Vec<u8>>> {
    json_response(status, &json!({ "error": format!("{e:#}") }))
}

fn unauthorized() -> Response<Cursor<Vec<u8>>> {
    json_response(StatusCode(401), &json!({ "error": "not logged in" }))
}

fn forbidden(reason: &str) -> Response<Cursor<Vec<u8>>> {
    json_response(StatusCode(403), &json!({ "error": reason }))
}

fn html(body: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.as_bytes().to_vec())
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        // Everything is inline and same-origin; the one thing the page must be able
        // to reach is the Keychain extension, which injects into the page itself.
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             connect-src 'self'; img-src data:; form-action 'none'; frame-ancestors 'none'",
        ))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebRole;

    #[test]
    fn a_cookie_header_yields_the_named_value() {
        let header = "other=1; tc_session=abc123; another=2";
        assert_eq!(cookie_value(header, SESSION_COOKIE).as_deref(), Some("abc123"));
        assert_eq!(cookie_value(header, "nothing-here"), None);
        assert_eq!(cookie_value("", SESSION_COOKIE), None);
        // A prefix of the name is not the name.
        assert_eq!(cookie_value("tc_sessionx=abc", SESSION_COOKIE), None);
    }

    #[test]
    fn only_something_shaped_like_a_hive_account_reaches_the_config_writer() {
        assert!(is_hive_account_name("alice"));
        assert!(is_hive_account_name("a-b.c1"));
        assert!(!is_hive_account_name(""));
        assert!(!is_hive_account_name("ab"));
        assert!(!is_hive_account_name("../../etc/passwd"));
        assert!(!is_hive_account_name("Alice"));
        assert!(!is_hive_account_name("alice%2e"));
        assert!(!is_hive_account_name(&"a".repeat(17)));
    }

    #[test]
    fn a_query_value_is_read_by_name() {
        assert_eq!(query_value("limit=50&x=1", "limit").as_deref(), Some("50"));
        assert_eq!(query_value("x=1", "limit"), None);
        assert_eq!(query_value("", "limit"), None);
    }

    #[test]
    fn the_roles_scope_what_each_session_may_touch() {
        assert!(WebRole::Admin.may_write("admin", "someone-else"));
        assert!(WebRole::Operator.may_write("bob", "bob"));
        assert!(!WebRole::Operator.may_write("bob", "alice"));
        assert!(!WebRole::Operator.may_read("bob", "alice"));
        assert!(WebRole::Viewer.may_read("carol", "alice"));
        assert!(!WebRole::Viewer.may_write("carol", "carol"));
        assert!(!WebRole::Viewer.is_admin());
        assert!(!WebRole::Operator.is_admin());
    }
}

#[cfg(test)]
mod http_tests {
    //! The routes, over real HTTP, with real sessions.
    //!
    //! Sessions are minted directly rather than through Keychain -- signing as
    //! somebody else's account is exactly what the handshake exists to prevent, so it
    //! cannot be done here. The handshake itself is covered in `auth`, including a
    //! live check against the chain.

    use super::*;
    use crate::config::WebRole;

    struct Fixture {
        addr: std::net::SocketAddr,
        panel: Arc<Panel>,
        shared: Arc<Shared>,
        config_path: PathBuf,
    }

    const CONFIG: &str = r#"
[hive]
nodes = ["https://api.hive.blog"]

[web]
enabled = true
bind = "127.0.0.1:0"

[web.access]
adminuser = "admin"
bob = "operator"
carol = "viewer"

[defaults.attack]
delay_secs = 20

[accounts.alice]
[accounts.bob]
"#;

    fn start(name: &str) -> Fixture {
        let config_path = std::env::temp_dir().join(format!("tc-bot-test-{name}-{}.toml", std::process::id()));
        std::fs::write(&config_path, CONFIG).unwrap();
        let config = Config::load(&config_path).unwrap();
        let shared = Shared::new(50);
        let running = spawn(&config, Arc::clone(&shared)).unwrap();
        Fixture {
            addr: running.addr,
            panel: running.panel,
            shared,
            config_path,
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.config_path);
        }
    }

    impl Fixture {
        fn session(&self, account: &str, role: WebRole) -> String {
            self.panel.auth.mint_session(account, role)
        }

        /// Returns (status, body).
        fn request(
            &self,
            method: &str,
            path: &str,
            token: Option<&str>,
            body: Option<&str>,
        ) -> (u16, String) {
            self.request_raw(method, path, token, body, true)
        }

        /// As above, but able to leave off the header the UI always sends -- which is
        /// what a cross-site request would look like.
        fn request_raw(
            &self,
            method: &str,
            path: &str,
            token: Option<&str>,
            body: Option<&str>,
            ui_header: bool,
        ) -> (u16, String) {
            let url = format!("http://{}{path}", self.addr);
            let mut request = match method {
                "GET" => ureq::get(&url),
                "PUT" => ureq::put(&url),
                _ => ureq::post(&url),
            };
            if ui_header {
                request = request.set(CSRF_HEADER, "1");
            }
            if let Some(token) = token {
                request = request.set("Cookie", &format!("{SESSION_COOKIE}={token}"));
            }
            let result = match body {
                Some(body) => request.set("Content-Type", "application/json").send_string(body),
                None => request.call(),
            };
            match result {
                Ok(response) => (response.status(), response.into_string().unwrap_or_default()),
                Err(ureq::Error::Status(code, response)) => {
                    (code, response.into_string().unwrap_or_default())
                }
                Err(e) => panic!("transport error: {e}"),
            }
        }
    }

    #[test]
    fn an_admin_sees_every_account_and_an_operator_sees_only_its_own() {
        let f = start("scope");
        f.shared.status().accounts.insert("alice".into(), Default::default());
        f.shared.status().accounts.insert("bob".into(), Default::default());

        let admin = f.session("adminuser", WebRole::Admin);
        let (status, body) = f.request("GET", "/api/status", Some(&admin), None);
        assert_eq!(status, 200);
        assert!(body.contains("alice") && body.contains("bob"), "{body}");

        let operator = f.session("bob", WebRole::Operator);
        let (status, body) = f.request("GET", "/api/status", Some(&operator), None);
        assert_eq!(status, 200);
        assert!(body.contains("bob"), "{body}");
        assert!(!body.contains("alice"), "an operator must not see another account: {body}");
    }

    #[test]
    fn an_operator_cannot_write_another_account_but_can_write_its_own() {
        let f = start("write");
        let operator = f.session("bob", WebRole::Operator);

        let settings = serde_json::to_string(&json!({
            "enabled": true,
            "settings": crate::config::Settings::default(),
        }))
        .unwrap();

        let before = std::fs::read_to_string(&f.config_path).unwrap();
        let (status, body) = f.request("PUT", "/api/accounts/alice", Some(&operator), Some(&settings));
        assert_eq!(status, 403, "{body}");
        // A refusal must leave the file untouched, not merely refuse to answer.
        assert_eq!(std::fs::read_to_string(&f.config_path).unwrap(), before);
        assert!(!f.shared.control().reload);

        let (status, body) = f.request("PUT", "/api/accounts/bob", Some(&operator), Some(&settings));
        assert_eq!(status, 200, "{body}");
    }

    #[test]
    fn a_save_rewrites_the_config_and_asks_the_bot_to_reload() {
        let f = start("save");
        let admin = f.session("adminuser", WebRole::Admin);

        let mut settings = crate::config::Settings::default();
        settings.attack.delay_secs = 77;
        settings.upgrade.enabled = true;
        let body = serde_json::to_string(&json!({ "enabled": false, "settings": settings })).unwrap();

        let (status, response) = f.request("PUT", "/api/accounts/alice", Some(&admin), Some(&body));
        assert_eq!(status, 200, "{response}");

        let written = std::fs::read_to_string(&f.config_path).unwrap();
        assert!(written.contains("delay_secs = 77"), "{written}");
        assert!(written.contains("enabled = false"), "{written}");
        // Still a config the bot can load, and still the other account's business.
        let reloaded = Config::from_str(&written).unwrap();
        let alice = reloaded.accounts.iter().find(|a| a.name == "alice").unwrap();
        assert_eq!(alice.settings.attack.delay_secs, 77);
        assert!(!alice.enabled);
        assert!(reloaded.accounts.iter().find(|a| a.name == "bob").unwrap().enabled);

        assert!(f.shared.control().reload, "the bot must be told to re-read the file");
    }

    #[test]
    fn a_viewer_can_read_but_changes_nothing_and_sees_no_log() {
        let f = start("viewer");
        let viewer = f.session("carol", WebRole::Viewer);

        assert_eq!(f.request("GET", "/api/config", Some(&viewer), None).0, 200);
        assert_eq!(f.request("GET", "/api/log", Some(&viewer), None).0, 403);
        assert_eq!(
            f.request("POST", "/api/control", Some(&viewer), Some(r#"{"action":"pause"}"#)).0,
            403
        );

        let body = serde_json::to_string(&json!({
            "enabled": true, "settings": crate::config::Settings::default(),
        }))
        .unwrap();
        assert_eq!(f.request("PUT", "/api/accounts/alice", Some(&viewer), Some(&body)).0, 403);
        assert!(!f.shared.control().paused);
    }

    #[test]
    fn an_admin_controls_the_bot() {
        let f = start("control");
        let admin = f.session("adminuser", WebRole::Admin);

        assert_eq!(f.request("POST", "/api/control", Some(&admin), r#"{"action":"pause"}"#.into()).0, 200);
        assert!(f.shared.control().paused);
        assert_eq!(f.request("POST", "/api/control", Some(&admin), r#"{"action":"resume"}"#.into()).0, 200);
        assert!(!f.shared.control().paused);
        assert_eq!(f.request("POST", "/api/control", Some(&admin), r#"{"action":"run_now"}"#.into()).0, 200);
        assert!(f.shared.control().run_now);

        let (status, body) = f.request("POST", "/api/control", Some(&admin), r#"{"action":"explode"}"#.into());
        assert_eq!(status, 400, "{body}");
    }

    #[test]
    fn a_request_without_the_ui_header_changes_nothing() {
        // What a form posted from another origin looks like: it may carry the cookie
        // in a browser that ignores SameSite, but it cannot set a custom header.
        let f = start("csrf");
        let admin = f.session("adminuser", WebRole::Admin);

        let (status, body) =
            f.request_raw("POST", "/api/control", Some(&admin), Some(r#"{"action":"pause"}"#), false);
        assert_eq!(status, 403, "{body}");
        assert!(!f.shared.control().paused, "a header-less request must not act");

        let settings = serde_json::to_string(&json!({
            "enabled": false, "settings": crate::config::Settings::default(),
        }))
        .unwrap();
        let before = std::fs::read_to_string(&f.config_path).unwrap();
        let (status, _) =
            f.request_raw("PUT", "/api/accounts/alice", Some(&admin), Some(&settings), false);
        assert_eq!(status, 403);
        assert_eq!(std::fs::read_to_string(&f.config_path).unwrap(), before);

        // The very same request with the header does act, so the assertions above are
        // about the header and not about something else refusing.
        let (status, body) =
            f.request_raw("POST", "/api/control", Some(&admin), Some(r#"{"action":"pause"}"#), true);
        assert_eq!(status, 200, "{body}");
        assert!(f.shared.control().paused);
    }

    #[test]
    fn a_bad_account_name_never_reaches_the_config_writer() {
        let f = start("names");
        let admin = f.session("adminuser", WebRole::Admin);
        let settings = serde_json::to_string(&json!({
            "enabled": true, "settings": crate::config::Settings::default(),
        }))
        .unwrap();

        let before = std::fs::read_to_string(&f.config_path).unwrap();
        for name in ["..", "%2e%2e", "Alice", "ab", "a-very-long-account-name", "al/ice"] {
            let (status, body) =
                f.request("PUT", &format!("/api/accounts/{name}"), Some(&admin), Some(&settings));
            // Some of these never reach the handler at all -- an HTTP client
            // normalises `..` out of a path -- so what is asserted is that the request
            // is refused and nothing is written, not which code says so.
            assert!(
                (400..500).contains(&status),
                "{name} should be refused, got {status} {body}"
            );
            assert_eq!(
                std::fs::read_to_string(&f.config_path).unwrap(),
                before,
                "{name} must not have written anything"
            );
        }

        // A name that is fine still works, so the loop above refuses these names
        // rather than refusing everything.
        let (status, body) = f.request("PUT", "/api/accounts/alice", Some(&admin), Some(&settings));
        assert_eq!(status, 200, "{body}");
    }

    #[test]
    fn an_expired_or_forged_token_is_no_session_at_all() {
        let f = start("token");
        assert_eq!(f.request("GET", "/api/status", Some("deadbeef"), None).0, 401);
        assert_eq!(f.request("GET", "/api/status", None, None).0, 401);

        let admin = f.session("adminuser", WebRole::Admin);
        assert_eq!(f.request("GET", "/api/status", Some(&admin), None).0, 200);
        f.panel.auth.logout(&admin);
        assert_eq!(f.request("GET", "/api/status", Some(&admin), None).0, 401);
    }
}
