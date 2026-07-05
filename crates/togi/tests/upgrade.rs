//! Integration tests for `togi upgrade` against a local fixture server
//! standing in for the GitHub releases API, reached via the internal
//! `TOGI_GITHUB_API_BASE_URL` override. No test here touches the real
//! network, and none can ever replace the test binary: every served
//! response shape stops the upgrade before the download step, and the
//! release download host is pointed at a closed port as a backstop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use predicates::prelude::*;

/// The API path `togi upgrade` queries for the latest release.
const LATEST_PATH: &str = "/repos/StanfordHPDS/togi/releases/latest";

/// GitHub's JSON body for a repository (or release) that does not exist.
const NOT_FOUND_BODY: &str = r#"{
  "message": "Not Found",
  "documentation_url": "https://docs.github.com/rest/releases/releases#get-the-latest-release",
  "status": "404"
}"#;

/// A local HTTP server serving canned responses by path, recording every
/// request path it sees. Unknown paths get a 404 with an empty body,
/// like GitHub serves for a repository with no releases.
struct ApiServer {
    server: Arc<tiny_http::Server>,
    base_url: String,
    hits: Arc<Mutex<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ApiServer {
    fn serve(routes: HashMap<String, (u16, String)>) -> ApiServer {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind"));
        let addr = server.server_addr().to_ip().expect("ip listener");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let server = Arc::clone(&server);
            let hits = Arc::clone(&hits);
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    let url = request.url().to_string();
                    hits.lock().expect("hits lock").push(url.clone());
                    let response = match routes.get(&url) {
                        Some((status, body)) => {
                            tiny_http::Response::from_string(body.clone()).with_status_code(*status)
                        }
                        None => tiny_http::Response::from_string("").with_status_code(404),
                    };
                    let _ = request.respond(response);
                }
            })
        };
        ApiServer {
            server,
            base_url: format!("http://{addr}"),
            hits,
            handle: Some(handle),
        }
    }

    /// A server with no routes at all: every request 404s with an empty
    /// body.
    fn empty() -> ApiServer {
        ApiServer::serve(HashMap::new())
    }

    /// A server answering the latest-release path with `status`/`body`.
    fn latest_release(status: u16, body: &str) -> ApiServer {
        let mut routes = HashMap::new();
        routes.insert(LATEST_PATH.to_string(), (status, body.to_string()));
        ApiServer::serve(routes)
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("hits lock").clone()
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A `http://127.0.0.1:<port>` URL nothing listens on, so any attempt to
/// download a release asset fails instead of touching the real network.
fn dead_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// `togi upgrade` sandboxed: release lookups go to the fixture server,
/// asset downloads to a closed port, and config/data dirs to a tempdir.
fn upgrade_cmd(sandbox: &tempfile::TempDir, api_base: &str) -> Command {
    let mut cmd = Command::cargo_bin("togi").expect("togi binary should build");
    cmd.current_dir(sandbox.path())
        .env("TOGI_CONFIG_DIR", sandbox.path().join("config"))
        .env("TOGI_DATA_DIR", sandbox.path().join("data"))
        .env("TOGI_RELEASE_BASE_URL", dead_url())
        .env("TOGI_GITHUB_API_BASE_URL", api_base)
        .arg("upgrade");
    cmd
}

#[test]
fn no_releases_yet_prints_a_friendly_message_and_exits_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let server = ApiServer::empty();

    upgrade_cmd(&sandbox, &server.base_url)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "togi has published no releases yet",
        ));

    assert_eq!(
        server.hits(),
        vec![LATEST_PATH.to_string()],
        "upgrade must query the fixture server, not the real API"
    );
}

#[test]
fn a_missing_repository_reads_as_no_releases_yet() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let server = ApiServer::latest_release(404, NOT_FOUND_BODY);

    upgrade_cmd(&sandbox, &server.base_url)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "togi has published no releases yet",
        ))
        .stderr(predicate::str::contains("error").not());

    assert_eq!(server.hits(), vec![LATEST_PATH.to_string()]);
}

#[test]
fn an_install_matching_the_latest_release_is_up_to_date() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let current = env!("CARGO_PKG_VERSION");
    let body = format!(r#"{{"tag_name": "v{current}"}}"#);
    let server = ApiServer::latest_release(200, &body);

    upgrade_cmd(&sandbox, &server.base_url)
        .assert()
        .success()
        .stdout(predicate::str::contains("up to date").and(predicate::str::contains(current)));

    assert_eq!(server.hits(), vec![LATEST_PATH.to_string()]);
}

#[test]
fn a_rate_limited_api_fails_with_guidance() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let server = ApiServer::latest_release(403, r#"{"message": "API rate limit exceeded"}"#);

    upgrade_cmd(&sandbox, &server.base_url)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("HTTP 403").and(predicate::str::contains("hint:")));

    assert_eq!(server.hits(), vec![LATEST_PATH.to_string()]);
}
