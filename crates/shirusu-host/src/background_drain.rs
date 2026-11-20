//! Owns one ready reader while process management is busy.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(crate) struct BackgroundDrain<T, E> {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<(T, Result<(), E>)>>,
}

impl<T: Send + 'static, E: Send + 'static> BackgroundDrain<T, E> {
    pub(crate) fn start(
        mut reader: T,
        interval: Duration,
        mut poll: impl FnMut(&mut T) -> Result<bool, E> + Send + 'static,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let requested = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("tree-ring-reader".into())
            .spawn(move || {
                let result = loop {
                    if requested.load(Ordering::Acquire) {
                        break Ok(());
                    }
                    match poll(&mut reader) {
                        Ok(true) => {}
                        Ok(false) => thread::park_timeout(interval),
                        Err(e) => break Err(e),
                    }
                };
                (reader, result)
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    /// (worker を持たないのは finish か drop の後だけ: 終わっている)
    pub(crate) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn finish(mut self) -> io::Result<(T, Result<(), E>)> {
        self.stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .take()
            .ok_or_else(|| io::Error::other("tree ring reader already finished"))?;
        worker.thread().unpark();
        worker
            .join()
            .map_err(|_| io::Error::other("tree ring reader panicked"))
    }
}

impl<T, E> Drop for BackgroundDrain<T, E> {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stop.store(true, Ordering::Release);
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_reader_keeps_draining_while_child_setup_waits() {
        let (observed, wait) = std::sync::mpsc::channel();
        let reader = BackgroundDrain::start(0, Duration::from_secs(60), move |n| {
            *n += 1;
            observed.send(*n).unwrap();
            Ok::<_, ()>(*n < 2)
        })
        .unwrap();
        // Hold the management thread here until independent polls have finished.
        for n in 1..=2 {
            assert_eq!(wait.recv_timeout(Duration::from_secs(5)).unwrap(), n);
        }
        assert!(!reader.is_finished());
        assert_eq!(reader.finish().unwrap(), (2, Ok(())));
    }
}
