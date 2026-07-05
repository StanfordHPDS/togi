//! Test-only helpers shared by the tool installer tests: a local HTTP
//! server handing out fixture release assets, and builders for the archive
//! formats the real releases ship in.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

/// A local HTTP server handing out fixture release assets, recording
/// every request path it sees. Unknown paths get a 404, like GitHub.
pub(crate) struct FixtureServer {
    server: Arc<tiny_http::Server>,
    pub(crate) base_url: String,
    hits: Arc<Mutex<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FixtureServer {
    pub(crate) fn serve(routes: HashMap<String, Vec<u8>>) -> FixtureServer {
        FixtureServer::serve_responses(
            routes
                .into_iter()
                .map(|(url, bytes)| (url, (200, bytes)))
                .collect(),
        )
    }

    /// Like [`FixtureServer::serve`], but each route carries its own
    /// HTTP status (for simulating rate limits and server errors).
    pub(crate) fn serve_responses(routes: HashMap<String, (u16, Vec<u8>)>) -> FixtureServer {
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
                        Some((status, bytes)) => {
                            tiny_http::Response::from_data(bytes.clone()).with_status_code(*status)
                        }
                        None => tiny_http::Response::from_data(Vec::new()).with_status_code(404),
                    };
                    let _ = request.respond(response);
                }
            })
        };
        FixtureServer {
            server,
            base_url: format!("http://{addr}"),
            hits,
            handle: Some(handle),
        }
    }

    pub(crate) fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("hits lock").clone()
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A `.tar.gz` holding one file at `entry_path` (mode 0644: the
/// installer, not the archive, is responsible for the exec bit).
pub(crate) fn targz_with(entry_path: &str, contents: &[u8]) -> Vec<u8> {
    targz_of(&[(entry_path, contents)])
}

/// A `.tar.gz` holding the given files (all mode 0644).
pub(crate) fn targz_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (entry_path, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, entry_path, *contents)
            .expect("append tar entry");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

/// A `.zip` holding one file at `entry_path`.
pub(crate) fn zip_with(entry_path: &str, contents: &[u8]) -> Vec<u8> {
    zip_of(&[(entry_path, contents)])
}

/// A `.zip` holding the given files.
pub(crate) fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (entry_path, contents) in entries {
        writer
            .start_file(*entry_path, zip::write::SimpleFileOptions::default())
            .expect("start zip entry");
        writer.write_all(contents).expect("write zip entry");
    }
    writer.finish().expect("finish zip").into_inner()
}

/// Lowercase hex sha256 of `bytes`, as published in checksum assets.
pub(crate) fn sha256_hex_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
