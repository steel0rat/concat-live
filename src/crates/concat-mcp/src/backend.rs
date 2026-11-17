// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Where a tool's request goes: the window's server, or an API of this
//! process's own.
//!
//! Both take a [`Request`] and give back its result as JSON, so a tool
//! cannot tell them apart and the meaning of every request stays the
//! API's. When the window opts into live mode, the same requests edit
//! its existing GUI session through the same [`Window`] backend.
//!
//! - [`Window`]: a JSON-RPC connection to the server the Automation page
//!   runs. Found through the listing the window writes
//!   (`concat_server::discovery`) or named by `CONCAT_API_ADDRESS` and
//!   `CONCAT_API_TOKEN`. Goes through the window's register of open
//!   projects and its one export slot. Without live mode the project on
//!   screen is refused; with it the window remains that project's writer.
//! - [`Headless`]: an [`Api`] in this process, writing under the user's
//!   home (or `CONCAT_MCP_ROOTS`). Nothing to start first, and nothing
//!   shared with a window: the project lock refuses a folder the window
//!   has open, so the window's server is preferred when it is there.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use concat_api::{Api, ApiError, AppDirs, Request, Response, Stopper};
use concat_server::client::Client;
use concat_server::discovery;
use serde_json::Value;

/// How long a connection to the window's server may take to open.
const PATIENCE: Duration = Duration::from_secs(3);

/// What a tool's request is handed to.
pub trait Backend: Send {
    /// Runs one request; its result as the JSON the API replies with.
    fn call(&mut self, request: Request) -> Result<Value, ApiError>;

    /// What this backend is, for the agent and the log: "window" or
    /// "headless", and where.
    fn describe(&self) -> Value;

    /// The session is over: stop what is running, and wait for it.
    fn finish(&mut self) {}
}

/// Which backend `concat --mcp` uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// The window's server when it can be reached, this process otherwise.
    #[default]
    Auto,
    /// The window's server, or an error saying how to turn it on.
    Window,
    /// This process, whatever else is running.
    Headless,
}

impl Mode {
    /// The mode a word names: "auto", "window" or "headless".
    pub fn parse(word: &str) -> Option<Mode> {
        match word.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Mode::Auto),
            "window" => Some(Mode::Window),
            "headless" => Some(Mode::Headless),
            _ => None,
        }
    }
}

/// The backend `mode` asks for, with the reason for whatever was not
/// chosen written to standard error, where an MCP client shows a server's
/// log.
pub fn open(mode: Mode) -> Result<Box<dyn Backend>, String> {
    match mode {
        Mode::Headless => Ok(Box::new(Headless::start()?)),
        Mode::Window => Ok(Box::new(Window::find()?)),
        Mode::Auto => match Window::find() {
            Ok(window) => Ok(Box::new(window)),
            Err(why) => {
                eprintln!("concat mcp: working headless: {why}");
                Ok(Box::new(Headless::start()?))
            }
        },
    }
}

/// Where the window's server is, and its token: the environment's if it
/// names them, the window's listing otherwise.
fn locate() -> Result<(SocketAddr, String), String> {
    let address = std::env::var("CONCAT_API_ADDRESS")
        .ok()
        .filter(|text| !text.is_empty());
    let token = std::env::var("CONCAT_API_TOKEN")
        .ok()
        .filter(|text| !text.is_empty());
    if let (Some(address), Some(token)) = (&address, &token) {
        let address = address.parse().map_err(|_| {
            format!("CONCAT_API_ADDRESS {address:?} is not an address like 127.0.0.1:7420")
        })?;
        return Ok((address, token.clone()));
    }
    if address.is_some() != token.is_some() {
        return Err("CONCAT_API_ADDRESS and CONCAT_API_TOKEN go together".to_owned());
    }
    let dirs = AppDirs::locate()?;
    let path = discovery::path_in(&dirs.config);
    match discovery::read(&path)? {
        Some(listing) => Ok((listing.address, listing.token)),
        None => Err(
            "the Concat window is not serving the API: open Concat, then Settings > Automation, and turn on Scripts and apps"
                .to_owned(),
        ),
    }
}

/// The window's server. A dropped connection may retry a read once, but
/// never a mutation whose reply could have been lost after it was applied.
pub struct Window {
    client: Client,
}

impl Window {
    /// Connects to the window's server.
    pub fn find() -> Result<Window, String> {
        let (address, token) = locate()?;
        let client = Client::connect(address, &token, PATIENCE)
            .map_err(|error| format!("{error} (if the window has closed, its listing is stale)"))?;
        Ok(Window { client })
    }

    /// Connects to the server at `address` with `token`.
    pub fn connect(address: SocketAddr, token: &str) -> Result<Window, String> {
        Ok(Window {
            client: Client::connect(address, token, PATIENCE)?,
        })
    }

    fn call_with_reconnect(
        &mut self,
        request: Request,
        reconnect: impl FnOnce() -> Result<Window, String>,
    ) -> Result<Value, ApiError> {
        match self.client.call(&request) {
            Err(error)
                if error
                    .message
                    .starts_with("the connection to the Concat server") =>
            {
                if !can_retry(&request) {
                    return Err(ApiError::failed(format!(
                        "{}; the request's outcome is unknown. Check the project or job state before retrying this request",
                        error.message
                    )));
                }
                let again = reconnect().map_err(|why| {
                    ApiError::failed(format!(
                        "{} - and reconnecting failed: {why}",
                        error.message
                    ))
                })?;
                *self = again;
                self.client.call(&request)
            }
            outcome => outcome,
        }
    }
}

impl Backend for Window {
    fn call(&mut self, request: Request) -> Result<Value, ApiError> {
        self.call_with_reconnect(request, Window::find)
    }

    fn describe(&self) -> Value {
        serde_json::json!({ "mode": "window", "address": self.client.address().to_string() })
    }
}

fn can_retry(request: &Request) -> bool {
    matches!(
        request,
        Request::Version
            | Request::ProjectList
            | Request::ProjectGet { .. }
            | Request::ProjectDocument { .. }
            | Request::CatalogueList { .. }
            | Request::MediaProbe { .. }
            | Request::TemplateList
            | Request::ExportStatus { .. }
            | Request::PreviewFrame { output: None, .. }
    )
}

/// An API in this process.
pub struct Headless {
    api: Api,
    stopper: Stopper,
}

impl Headless {
    /// An API over this machine's app directories, writing under the
    /// roots `CONCAT_MCP_ROOTS` names (the platform's path-list syntax),
    /// or the user's home.
    pub fn start() -> Result<Headless, String> {
        Ok(Headless::with_api(Api::new(Arc::new(|_| {}))?))
    }

    /// An API made by the caller - a test's, over a scratch folder.
    pub fn with_api(mut api: Api) -> Headless {
        let roots: Vec<PathBuf> = std::env::var_os("CONCAT_MCP_ROOTS")
            .map(|roots| std::env::split_paths(&roots).collect())
            .filter(|roots: &Vec<PathBuf>| !roots.is_empty())
            .unwrap_or_else(concat_server::home_root);
        api.add_capability("mcp");
        if api.roots().is_empty() {
            api.restrict_writes_to(roots);
        }
        let stopper = api.stopper();
        Headless { api, stopper }
    }
}

impl Backend for Headless {
    fn call(&mut self, request: Request) -> Result<Value, ApiError> {
        match self.api.dispatch(request) {
            Response::Result(reply) => serde_json::to_value(&reply)
                .map_err(|error| ApiError::failed(format!("the reply did not encode: {error}"))),
            Response::Error(error) => Err(error),
        }
    }

    fn describe(&self) -> Value {
        let roots: Vec<String> = self
            .api
            .roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        serde_json::json!({ "mode": "headless", "writesUnder": roots })
    }

    fn finish(&mut self) {
        self.stopper.cancel_all();
        self.api.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_server::{Config, Server};
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;

    fn result(server: &Server, request: Request) -> Value {
        match server.hub().call(request) {
            Response::Result(reply) => serde_json::to_value(reply).expect("a JSON reply"),
            Response::Error(error) => panic!("the API refused: {error}"),
        }
    }

    fn server() -> (Server, tempfile::TempDir, String, usize) {
        let scratch = tempfile::tempdir().expect("scratch");
        let dirs = AppDirs {
            config: scratch.path().join("config"),
            data: scratch.path().join("data"),
        };
        let server = Server::start(
            Config {
                json: Some("127.0.0.1:0".parse().expect("an address")),
                roots: vec![scratch.path().to_path_buf()],
                ..Config::default()
            },
            move |events| Ok(Api::with_dirs(dirs, events)),
        )
        .expect("a real server");
        let created = result(
            &server,
            Request::ProjectCreate {
                location: scratch.path().to_string_lossy().into_owned(),
                name: "Lost reply".to_owned(),
                video: None,
            },
        );
        let tracks = created["project"]["timelines"][0]["tracks"]
            .as_array()
            .expect("tracks")
            .len();
        let path = scratch
            .path()
            .join("Lost reply")
            .to_string_lossy()
            .into_owned();
        (server, scratch, path, tracks)
    }

    /// Authenticates normally, then discards the next response only
    /// after the real server has finished that request.
    fn lose_next_reply(server: &Server) -> (Window, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the proxy listens");
        let address = listener.local_addr().expect("the proxy address");
        let upstream = server.json_addr().expect("the server address");
        let proxy = std::thread::spawn(move || {
            let (mut downstream, _) = listener.accept().expect("the backend connects");
            downstream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("a timeout");
            let mut downstream_reader = BufReader::new(downstream.try_clone().expect("a reader"));
            let mut upstream = TcpStream::connect(upstream).expect("the real server connects");
            upstream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("a timeout");
            let mut upstream_reader = BufReader::new(upstream.try_clone().expect("a reader"));
            for forward in [true, false] {
                let mut request = String::new();
                assert!(
                    downstream_reader
                        .read_line(&mut request)
                        .expect("a request")
                        > 0
                );
                upstream
                    .write_all(request.as_bytes())
                    .expect("forwards the request");
                upstream.flush().expect("flushes");
                let mut response = String::new();
                assert!(
                    upstream_reader
                        .read_line(&mut response)
                        .expect("the server answered")
                        > 0
                );
                if forward {
                    downstream
                        .write_all(response.as_bytes())
                        .expect("forwards auth");
                    downstream.flush().expect("flushes auth");
                }
            }
        });
        let window = Window::connect(address, server.token()).expect("connects through the proxy");
        (window, proxy)
    }

    #[test]
    fn a_lost_edit_reply_never_applies_the_edit_twice() {
        let (server, _scratch, path, tracks) = server();
        let (mut window, proxy) = lose_next_reply(&server);
        let request = serde_json::from_value(json!({
            "method": "edit.apply",
            "path": path,
            "command": { "op": "addTrack", "bottom": false }
        }))
        .expect("an edit request");
        let mut reconnected = false;
        let answer = window.call_with_reconnect(request, || {
            reconnected = true;
            Window::connect(server.json_addr().expect("listening"), server.token())
        });
        proxy.join().expect("the reply was discarded");
        let seen = result(&server, Request::ProjectGet { path });
        assert_eq!(
            seen["project"]["timelines"][0]["tracks"]
                .as_array()
                .expect("tracks")
                .len(),
            tracks + 1,
            "the first request applied once despite its missing response"
        );
        assert!(!reconnected, "a mutation is never replayed");
        let error = answer.expect_err("the mutation's outcome cannot be known from the socket");
        assert!(error.message.contains("outcome is unknown"));
        assert!(error.message.contains("before retrying"));
        drop(window);
        server.stop();
    }

    #[test]
    fn a_read_can_reconnect_after_its_reply_was_lost() {
        let (server, _scratch, path, tracks) = server();
        let (mut window, proxy) = lose_next_reply(&server);
        let mut reconnected = false;
        let answer = window
            .call_with_reconnect(Request::ProjectGet { path }, || {
                reconnected = true;
                Window::connect(server.json_addr().expect("listening"), server.token())
            })
            .expect("the read is retried");
        proxy.join().expect("the first reply was discarded");
        assert!(reconnected);
        assert_eq!(
            answer["project"]["timelines"][0]["tracks"]
                .as_array()
                .expect("tracks")
                .len(),
            tracks
        );
        assert_eq!(window.describe()["mode"], "window");
        drop(window);
        server.stop();
    }

    #[test]
    fn a_preview_written_before_its_reply_was_lost_is_never_replayed() {
        let (server, scratch, path, _) = server();
        let (mut window, proxy) = lose_next_reply(&server);
        let output = scratch.path().join("written.png");
        let mut reconnected = false;
        let answer = window.call_with_reconnect(
            Request::PreviewFrame {
                path,
                time: 0.0,
                output: Some(output.to_string_lossy().into_owned()),
                width: Some(16),
                height: Some(9),
            },
            || {
                reconnected = true;
                Window::connect(server.json_addr().expect("listening"), server.token())
            },
        );
        proxy.join().expect("the reply was discarded");
        assert!(
            output.is_file(),
            "the first request already wrote the frame"
        );
        assert!(!reconnected, "writing a preview is a mutation too");
        assert!(
            answer
                .expect_err("an unknown outcome")
                .message
                .contains("outcome is unknown")
        );
        drop(window);
        server.stop();
    }
}
