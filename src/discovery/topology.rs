use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

#[derive(Default)]
struct Events {
    epoch: AtomicU64,
    changed: Notify,
}

impl Events {
    fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

/// Raw notifications, not snapshot differences: remove/add with an identical
/// final configuration must still invalidate every previously issued lease.
pub(super) struct Topology {
    events: Arc<Events>,
    source: platform::Source,
}

impl Topology {
    pub fn new() -> io::Result<Self> {
        let events = Arc::new(Events::default());
        let source = platform::Source::new(events.clone())?;
        Ok(Self { events, source })
    }

    pub fn check(&self) -> io::Result<u64> {
        self.source.drain(&self.events)?;
        Ok(self.events.epoch.load(Ordering::SeqCst))
    }

    pub async fn changed(&self, epoch: u64) -> io::Result<()> {
        loop {
            let notified = self.events.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check()? != epoch {
                return Ok(());
            }
            tokio::select! {
                _ = &mut notified => {}
                result = self.source.ready() => result?,
            }
        }
    }

    #[cfg(test)]
    pub fn invalidate(&self) {
        self.events.invalidate();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use super::*;
    use nix::{
        fcntl::{fcntl, FcntlArg, OFlag},
        sys::socket::{recv, socket, AddressFamily, MsgFlags, SockFlag, SockType},
    };
    use std::{
        os::fd::{AsRawFd, OwnedFd},
        sync::Mutex,
    };
    use tokio::io::unix::AsyncFd;

    pub struct Source {
        fd: AsyncFd<OwnedFd>,
        reader: Mutex<()>,
    }

    impl Source {
        pub fn new(_events: Arc<Events>) -> io::Result<Self> {
            #[cfg(target_os = "linux")]
            let fd = {
                use nix::sys::socket::{bind, NetlinkAddr, SockProtocol};
                let fd = socket(
                    AddressFamily::Netlink,
                    SockType::Raw,
                    SockFlag::SOCK_CLOEXEC,
                    Some(SockProtocol::NetlinkRoute),
                )?;
                bind(fd.as_raw_fd(), &NetlinkAddr::new(0, 0x01 | 0x10))?;
                fd
            };
            #[cfg(target_os = "macos")]
            let fd = {
                let fd = socket(AddressFamily::Route, SockType::Raw, SockFlag::empty(), None)?;
                fcntl(&fd, FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC))?;
                fd
            };
            fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
            Ok(Self {
                fd: AsyncFd::new(fd)?,
                reader: Mutex::new(()),
            })
        }

        pub fn drain(&self, events: &Events) -> io::Result<()> {
            let _reader = self
                .reader
                .lock()
                .map_err(|_| io::Error::other("topology reader poisoned"))?;
            let mut buffer = [0; 65536];
            let mut changed = false;
            loop {
                match recv(
                    self.fd.get_ref().as_raw_fd(),
                    &mut buffer,
                    MsgFlags::empty(),
                ) {
                    Ok(0) => {
                        events.invalidate();
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "topology event stream ended",
                        ));
                    }
                    Ok(size) => {
                        #[cfg(target_os = "linux")]
                        {
                            let _ = size;
                            changed = true;
                        }
                        #[cfg(target_os = "macos")]
                        {
                            // Multicast membership/route messages are not
                            // interface lifetimes and must not cause a rebuild loop.
                            if size < 4
                                || matches!(
                                    i32::from(buffer[3]),
                                    libc::RTM_IFINFO
                                        | libc::RTM_IFINFO2
                                        | libc::RTM_NEWADDR
                                        | libc::RTM_DELADDR
                                )
                            {
                                changed = true;
                            }
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => break,
                    Err(error) => {
                        events.invalidate();
                        return Err(error.into());
                    }
                }
            }
            if changed {
                events.invalidate();
            }
            Ok(())
        }

        pub async fn ready(&self) -> io::Result<()> {
            let mut ready = self.fd.readable().await?;
            ready.clear_ready();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_monitor_releases_notification_context() {
        let topology = Topology::new().unwrap();
        let events = Arc::downgrade(&topology.events);
        drop(topology);
        assert!(
            events.upgrade().is_none(),
            "native subscriptions must release context after successful cancellation"
        );
    }
}

// netwatcher was evaluated, but its UpdateCursor drops equal snapshots and its
// Unix drain coalesces notifications before enumerating. That loses precisely
// the remove/add-to-identical-state transition this module must retain. Neither
// socket2 nor network-interface wraps raw Windows topology notifications.
#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        NetworkManagement::IpHelper::{
            CancelMibChangeNotify2, NotifyIpInterfaceChange, NotifyUnicastIpAddressChange,
            MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW,
        },
        Networking::WinSock::AF_INET,
    };

    pub struct Source {
        handles: Vec<usize>,
        context: usize,
    }

    impl Source {
        pub fn new(events: Arc<Events>) -> io::Result<Self> {
            let context = Box::into_raw(Box::new(events));
            let mut source = Self {
                handles: Vec::new(),
                context: context as usize,
            };
            let mut handle: HANDLE = std::ptr::null_mut();
            // SAFETY: the boxed Arc has a stable address until cancellation
            // completes; callbacks only access its thread-safe Events.
            let result = unsafe {
                NotifyIpInterfaceChange(
                    AF_INET,
                    Some(interface_changed),
                    context.cast(),
                    false,
                    &mut handle,
                )
            };
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result as i32));
            }
            source.handles.push(handle as usize);
            let result = unsafe {
                NotifyUnicastIpAddressChange(
                    AF_INET,
                    Some(address_changed),
                    context.cast(),
                    false,
                    &mut handle,
                )
            };
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result as i32));
            }
            source.handles.push(handle as usize);
            Ok(source)
        }

        pub fn drain(&self, _events: &Events) -> io::Result<()> {
            Ok(())
        }
        pub async fn ready(&self) -> io::Result<()> {
            std::future::pending().await
        }
    }

    unsafe extern "system" fn interface_changed(
        context: *const c_void,
        _row: *const MIB_IPINTERFACE_ROW,
        _kind: MIB_NOTIFICATION_TYPE,
    ) {
        // SAFETY: registration owns this boxed Arc through callback completion.
        let events = unsafe { &*context.cast::<Arc<Events>>() };
        events.invalidate();
    }

    unsafe extern "system" fn address_changed(
        context: *const c_void,
        _row: *const MIB_UNICASTIPADDRESS_ROW,
        _kind: MIB_NOTIFICATION_TYPE,
    ) {
        let events = unsafe { &*context.cast::<Arc<Events>>() };
        events.invalidate();
    }

    impl Drop for Source {
        fn drop(&mut self) {
            let mut cancelled = true;
            for handle in &self.handles {
                // Never cancel from the callback; Windows waits for callbacks
                // here, so freeing context afterward cannot race OS access.
                let result = unsafe { CancelMibChangeNotify2(*handle as HANDLE) };
                if result != 0 {
                    cancelled = false;
                    tracing::error!(
                        "failed to cancel topology notification: {}; retaining callback context",
                        result
                    );
                }
            }
            if cancelled {
                unsafe {
                    drop(Box::from_raw(self.context as *mut Arc<Events>));
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub struct Source;
    impl Source {
        pub fn new(_events: Arc<Events>) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "discovery topology notifications unavailable",
            ))
        }
        pub fn drain(&self, _events: &Events) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "discovery topology notifications unavailable",
            ))
        }
        pub async fn ready(&self) -> io::Result<()> {
            std::future::pending().await
        }
    }
}
