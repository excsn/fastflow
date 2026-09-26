use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::protocol::{Request, Response};

pub trait Handler: Send + Sync + 'static {
  fn handle(&self, request: Request) -> Response;
}

/// Binds the socket and serves each connection on its own thread. A socket file left by a dead
/// daemon is removed; one that still answers means another daemon is running.
pub fn serve(path: &Path, handler: Arc<dyn Handler>) -> io::Result<JoinHandle<()>> {
  if path.exists() {
    if UnixStream::connect(path).is_ok() {
      return Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("another daemon is listening on {}", path.display()),
      ));
    }
    fs::remove_file(path)?;
  }
  if let Some(dir) = path.parent() {
    fs::create_dir_all(dir)?;
  }
  let listener = UnixListener::bind(path)?;
  Ok(thread::spawn(move || {
    for stream in listener.incoming().filter_map(|s| s.ok()) {
      let handler = Arc::clone(&handler);
      thread::spawn(move || {
        let _ = serve_connection(stream, handler.as_ref());
      });
    }
  }))
}

fn serve_connection(stream: UnixStream, handler: &dyn Handler) -> io::Result<()> {
  let mut writer = stream.try_clone()?;
  for line in BufReader::new(stream).lines() {
    let line = line?;
    if line.trim().is_empty() {
      continue;
    }
    let response = match serde_json::from_str::<Request>(&line) {
      Ok(request) => handler.handle(request),
      Err(e) => Response::error(format!("bad request: {e}")),
    };
    let mut out = serde_json::to_string(&response).map_err(io::Error::other)?;
    out.push('\n');
    writer.write_all(out.as_bytes())?;
  }
  Ok(())
}

/// Sends one request and reads one response.
pub fn request(path: &Path, request: &Request) -> io::Result<Response> {
  let stream = UnixStream::connect(path)?;
  let mut writer = stream.try_clone()?;
  let mut line = serde_json::to_string(request).map_err(io::Error::other)?;
  line.push('\n');
  writer.write_all(line.as_bytes())?;
  let mut reply = String::new();
  BufReader::new(stream).read_line(&mut reply)?;
  serde_json::from_str(&reply).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::protocol::Status;

  struct Echo;

  impl Handler for Echo {
    fn handle(&self, request: Request) -> Response {
      match request {
        Request::Status => Response {
          status: Some(Status::default()),
          ..Response::ok()
        },
        Request::Render { id } => Response::with_id(id),
        _ => Response::error("unsupported"),
      }
    }
  }

  #[test]
  fn round_trips_requests_and_replaces_a_stale_socket() {
    let dir = std::env::temp_dir().join(format!("fastflow-sock-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sock");
    fs::write(&path, b"").unwrap();

    serve(&path, Arc::new(Echo)).unwrap();
    let r = request(&path, &Request::Render { id: "abc".into() }).unwrap();
    assert_eq!(r, Response::with_id("abc"));
    assert!(request(&path, &Request::Status).unwrap().status.is_some());
    assert!(serve(&path, Arc::new(Echo)).is_err());
    let _ = fs::remove_dir_all(&dir);
  }
}
