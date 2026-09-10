//! An owned Link executor. Caller runtimes never execute its background work.
#![forbid(unsafe_code)]

use std::{
    future::Future,
    io,
    sync::{mpsc, Mutex, OnceLock},
    thread::{self, JoinHandle},
};
use tokio::{runtime::Handle, sync::oneshot};

use super::thread::ThreadPriority;

struct PriorityGuard(ThreadPriority);

impl Drop for PriorityGuard {
    fn drop(&mut self) {
        self.0.reset();
    }
}

enum Command {
    Priority(bool, oneshot::Sender<io::Result<()>>),
}

struct Reaper {
    sender: mpsc::Sender<JoinHandle<()>>,
    // Retain ownership of the process-wide join service itself.
    _thread: JoinHandle<()>,
}

fn reaper() -> io::Result<mpsc::Sender<JoinHandle<()>>> {
    static REAPER: OnceLock<Mutex<Option<Reaper>>> = OnceLock::new();
    let mut reaper = REAPER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| io::Error::other("Link join service poisoned"))?;
    if reaper.is_none() {
        let (sender, receiver) = mpsc::channel::<JoinHandle<()>>();
        let thread = thread::Builder::new()
            .name("Link thread joiner".into())
            .spawn(move || {
                for thread in receiver {
                    if thread.join().is_err() {
                        tracing::error!("Link IO thread panicked during reentrant shutdown");
                    }
                }
            })?;
        *reaper = Some(Reaper {
            sender,
            _thread: thread,
        });
    }
    Ok(reaper.as_ref().expect("initialized above").sender.clone())
}

pub(crate) struct IoContext {
    handle: Handle,
    stop: Option<oneshot::Sender<()>>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    thread: Option<JoinHandle<()>>,
    reaper: mpsc::Sender<JoinHandle<()>>,
}

impl IoContext {
    pub(crate) fn new() -> io::Result<Self> {
        let reaper = reaper()?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        let thread = thread::Builder::new()
            .name("Link IO".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if ready_tx.send(Ok(runtime.handle().clone())).is_err() {
                    return;
                }
                let mut priority = PriorityGuard(ThreadPriority::new());
                runtime.block_on(async {
                    tokio::pin!(stopped);
                    loop {
                        tokio::select! {
                            biased;
                            _ = &mut stopped => break,
                            command = command_rx.recv() => match command {
                                Some(Command::Priority(high, result)) => {
                                    let outcome = if high {
                                        priority.0.try_set_high()
                                    } else {
                                        priority.0.try_reset()
                                    };
                                    let _ = result.send(outcome);
                                }
                                None => break,
                            }
                        }
                    }
                });
                // Drop cancels async tasks and waits for blocking-pool work.
                // Restore priority on this same thread, including during unwind.
                drop(runtime);
            })?;
        let handle = match ready_rx.recv() {
            Ok(Ok(handle)) => handle,
            outcome => {
                if thread.join().is_err() {
                    return Err(io::Error::other("Link IO startup panicked"));
                }
                return Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => io::Error::other("Link IO startup channel closed"),
                });
            }
        };
        Ok(Self {
            handle,
            stop: Some(stop),
            commands,
            thread: Some(thread),
            reaper,
        })
    }

    pub(crate) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    pub(crate) async fn set_priority(&self, high: bool) -> io::Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Priority(high, reply))
            .map_err(|_| io::Error::other("Link IO thread stopped"))?;
        result
            .await
            .map_err(|_| io::Error::other("Link IO priority request cancelled"))?
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            if thread.thread().id() == thread::current().id() {
                // The executing callback must return before its thread can be
                // joined. A retained join service owns that completion, not a
                // detached task or an abort-only fallback.
                self.reaper
                    .send(thread)
                    .expect("retained Link join service must remain connected");
            } else if thread.join().is_err() {
                tracing::error!("Link IO thread panicked during shutdown");
            }
        }
    }
}

impl Drop for IoContext {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn work_and_descendants_run_on_owned_thread() {
        let io = IoContext::new().unwrap();
        let caller = thread::current().id();
        let (first, child) = io
            .spawn(async {
                let first = thread::current().id();
                let child = tokio::spawn(async { thread::current().id() })
                    .await
                    .unwrap();
                (first, child)
            })
            .await
            .unwrap();
        assert_ne!(first, caller);
        assert_eq!(first, child);
        assert_eq!(thread::current().id(), caller);
    }

    #[tokio::test]
    async fn real_priority_request_and_restore_stay_on_owned_thread() {
        let io = IoContext::new().unwrap();
        let before = io.spawn(async { thread::current().id() }).await.unwrap();
        let outcome = io.set_priority(true).await;
        if std::env::var_os("LINK_IO_REQUIRE_PRIORITY").is_some() {
            assert!(
                outcome.is_ok(),
                "privileged priority request failed: {outcome:?}"
            );
        }
        match outcome {
            Ok(()) => {
                println!("Link IO real-time priority: OS request accepted");
                io.set_priority(true).await.unwrap();
            }
            Err(error) => {
                println!("Link IO real-time priority: OS request denied: {error}");
            }
        }
        io.set_priority(false).await.unwrap();
        io.set_priority(false).await.unwrap();
        let after = io.spawn(async { thread::current().id() }).await.unwrap();
        assert_eq!(before, after);
        assert_ne!(after, thread::current().id());
    }

    #[tokio::test]
    async fn repeated_shutdown_rejects_priority_commands() {
        let mut io = IoContext::new().unwrap();
        io.shutdown();
        io.shutdown();
        assert!(io.set_priority(true).await.is_err());
        assert!(io.set_priority(false).await.is_err());
    }

    #[tokio::test]
    async fn owned_socket_sends_to_caller_runtime() {
        let io = IoContext::new().unwrap();
        let peer = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let destination = peer.local_addr().unwrap();
        io.spawn(async move {
            let socket = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
            socket.send_to(b"owned", destination).await.unwrap();
        })
        .await
        .unwrap();
        let mut bytes = [0; 16];
        let (size, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            peer.recv_from(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&bytes[..size], b"owned");
    }

    #[tokio::test]
    async fn shutdown_releases_parked_tasks_without_polling_caller_runtime() {
        let io = IoContext::new().unwrap();
        let resource = Arc::new(());
        let weak = Arc::downgrade(&resource);
        let (ready, running) = oneshot::channel();
        io.spawn(async move {
            let _resource = resource;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        running.await.unwrap();
        drop(io);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn reentrant_shutdown_is_owned_and_does_not_self_join() {
        let io = IoContext::new().unwrap();
        let handle = io.handle.clone();
        let (finished, done) = mpsc::sync_channel(1);
        struct Finished(mpsc::SyncSender<()>);
        impl Drop for Finished {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let witness = Finished(finished);
        handle.spawn(async move {
            let _witness = witness;
            drop(io);
        });
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}
