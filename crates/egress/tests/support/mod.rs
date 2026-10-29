use std::{
    future::Future,
    net::{SocketAddr, TcpListener as StdListener},
    sync::mpsc,
    thread,
    time::Duration,
};

use nddev_device_sync_cache_egress::{Limits, Policy, serve};
use tokio::{io, net::TcpListener, sync::oneshot, task::JoinSet};

/// One owned runtime/thread per fixture. After the server returns its runtime stays alive
/// until Drop, so a stop assertion cannot mistake runtime teardown for an owned server drain.
pub struct Server {
    pub address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Sender<()>>,
    completed: mpsc::Receiver<io::Result<usize>>,
    worker: Option<thread::JoinHandle<()>>,
    stopped: Option<usize>,
}

impl Server {
    fn start<F, Fut>(run: F) -> Self
    where
        F: FnOnce(TcpListener, oneshot::Receiver<()>) -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<()>>,
    {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop, stopped) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let (finished, completed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let listener = TcpListener::from_std(listener).unwrap();
                let result = run(listener, stopped).await;
                let alive = tokio::runtime::Handle::current()
                    .metrics()
                    .num_alive_tasks();
                let _ = finished.send(result.map(|()| alive));
                let _ = tokio::time::timeout(Duration::from_secs(10), released).await;
            });
        });
        Self {
            address,
            stop: Some(stop),
            release: Some(release),
            completed,
            worker: Some(worker),
            stopped: None,
        }
    }

    pub fn stop(&mut self) -> usize {
        if let Some(alive) = self.stopped {
            return alive;
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let alive = self
            .completed
            .recv_timeout(Duration::from_secs(5))
            .expect("fixture did not acknowledge shutdown")
            .expect("fixture failed");
        self.stopped = Some(alive);
        alive
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "owned fixture thread panicked");
            }
        }
    }
}

pub fn relay(policy: Policy, limits: Limits) -> Server {
    Server::start(move |listener, stopped| async move {
        serve(listener, policy, limits, async move {
            tokio::select! {
                _ = stopped => {},
                _ = tokio::time::sleep(Duration::from_secs(10)) => {},
            }
        })
        .await
    })
}

pub fn echo() -> Server {
    Server::start(|listener, stopped| async move {
        let mut workers = JoinSet::new();
        tokio::pin!(stopped);
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                _ = &mut deadline => break,
                _ = workers.join_next(), if !workers.is_empty() => {},
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted?;
                    if workers.len() < 8 {
                        workers.spawn(async move {
                            let (mut read, mut write) = stream.split();
                            let _ = io::copy(&mut read, &mut write).await;
                        });
                    }
                }
            }
        }
        drop(listener);
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        Ok(())
    })
}
