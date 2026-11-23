//! One render at a time on a worker thread.

use fibre::mpsc::{self, UnboundedSyncSender as Sender};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::protocol::{Finished, Job, Rendering};

/// Runs a job and returns the file it wrote. Progress is reported from 0 to 1.
pub type Runner = Box<dyn FnMut(&Job, &mut dyn FnMut(f64)) -> Result<String, String> + Send>;
pub type OnDone = Box<dyn Fn(&Finished) + Send>;

#[derive(Debug, Clone, Default)]
pub struct QueueStatus {
  pub rendering: Option<Rendering>,
  pub queued: VecDeque<Job>,
  pub last: Option<Finished>,
}

pub struct RenderQueue {
  tx: Sender<Job>,
  status: Arc<Mutex<QueueStatus>>,
}

impl RenderQueue {
  pub fn start(mut runner: Runner, on_done: OnDone) -> RenderQueue {
    let (tx, rx) = mpsc::unbounded::<Job>();
    let status = Arc::new(Mutex::new(QueueStatus::default()));
    let shared = Arc::clone(&status);
    thread::spawn(move || {
      while let Ok(job) = rx.recv() {
        {
          let mut s = shared.lock().unwrap();
          s.queued.retain(|q| q != &job);
          s.rendering = Some(Rendering {
            job: job.clone(),
            progress: 0.0,
          });
        }
        let progress_status = Arc::clone(&shared);
        let result = runner(&job, &mut |p| {
          if let Some(r) = progress_status.lock().unwrap().rendering.as_mut() {
            r.progress = p;
          }
        });
        let finished = Finished { job, result };
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

  /// A job already waiting is not queued twice.
  pub fn push(&mut self, job: Job) {
    let mut s = self.status.lock().unwrap();
    if s.queued.contains(&job) {
      return;
    }
    s.queued.push_back(job.clone());
    drop(s);
    let _ = self.tx.send(job);
  }

  pub fn status(&self) -> QueueStatus {
    self.status.lock().unwrap().clone()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use fibre::mpsc;
  use std::time::Duration;

  #[test]
  fn renders_in_order_and_reports_each() {
    let (done_tx, done_rx) = mpsc::bounded(2);
    let mut q = RenderQueue::start(
      Box::new(|job, progress| {
        progress(0.5);
        if job.id == "bad" {
          Err("broken".into())
        } else {
          Ok(format!("{}/render.mp4", job.id))
        }
      }),
      Box::new(move |f| done_tx.send(f.clone()).unwrap()),
    );
    q.push(Job::render("a", None));
    q.push(Job::render("bad", None));
    let first = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.result, Ok("a/render.mp4".into()));
    assert_eq!(second.result, Err("broken".into()));
    assert_eq!(q.status().last, Some(second));
  }
}
