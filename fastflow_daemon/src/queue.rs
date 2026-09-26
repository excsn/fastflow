//! One render at a time on a worker thread.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::protocol::{Finished, Rendering};

/// Renders recording `id` and returns the output path. Progress is reported from 0 to 1.
pub type Runner = Box<dyn FnMut(&str, &mut dyn FnMut(f64)) -> Result<String, String> + Send>;
pub type OnDone = Box<dyn Fn(&Finished) + Send>;

#[derive(Debug, Clone, Default)]
pub struct QueueStatus {
  pub rendering: Option<Rendering>,
  pub queued: VecDeque<String>,
  pub last: Option<Finished>,
}

pub struct RenderQueue {
  tx: Sender<String>,
  status: Arc<Mutex<QueueStatus>>,
}

impl RenderQueue {
  pub fn start(mut runner: Runner, on_done: OnDone) -> RenderQueue {
    let (tx, rx) = mpsc::channel::<String>();
    let status = Arc::new(Mutex::new(QueueStatus::default()));
    let shared = Arc::clone(&status);
    thread::spawn(move || {
      for id in rx {
        {
          let mut s = shared.lock().unwrap();
          s.queued.retain(|q| q != &id);
          s.rendering = Some(Rendering {
            id: id.clone(),
            progress: 0.0,
          });
        }
        let progress_status = Arc::clone(&shared);
        let result = runner(&id, &mut |p| {
          if let Some(r) = progress_status.lock().unwrap().rendering.as_mut() {
            r.progress = p;
          }
        });
        let finished = Finished { id, result };
        {
          let mut s = shared.lock().unwrap();
          s.rendering = None;
          s.last = Some(finished.clone());
        }
        on_done(&finished);
      }
    });
    RenderQueue { tx, status }
  }

  /// A recording already waiting is not queued twice.
  pub fn push(&self, id: &str) {
    let mut s = self.status.lock().unwrap();
    if s.queued.iter().any(|q| q == id) {
      return;
    }
    s.queued.push_back(id.to_owned());
    drop(s);
    let _ = self.tx.send(id.to_owned());
  }

  pub fn status(&self) -> QueueStatus {
    self.status.lock().unwrap().clone()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::mpsc;
  use std::time::Duration;

  #[test]
  fn renders_in_order_and_reports_each() {
    let (done_tx, done_rx) = mpsc::channel();
    let q = RenderQueue::start(
      Box::new(|id, progress| {
        progress(0.5);
        if id == "bad" {
          Err("broken".into())
        } else {
          Ok(format!("{id}/render.mp4"))
        }
      }),
      Box::new(move |f| done_tx.send(f.clone()).unwrap()),
    );
    q.push("a");
    q.push("bad");
    let first = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.result, Ok("a/render.mp4".into()));
    assert_eq!(second.result, Err("broken".into()));
    assert_eq!(q.status().last, Some(second));
  }
}
