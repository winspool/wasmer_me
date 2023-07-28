use std::{
    future::Future,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, RwLock},
    task::Poll,
    time::Duration,
};

#[cfg(feature = "enable-serde")]
use serde_derive::{Deserialize, Serialize};
use virtual_mio::{
    FilteredHandler, FilteredHandlerSubscriptions, InterestHandler, InterestType,
    StatefulHandlerState,
};
use virtual_net::{
    NetworkError, VirtualIcmpSocket, VirtualNetworking, VirtualRawSocket, VirtualTcpListener,
    VirtualTcpSocket, VirtualUdpSocket,
};
use wasmer_types::MemorySize;
use wasmer_wasix_types::wasi::{Addressfamily, Errno, Rights, SockProto, Sockoption, Socktype};

use crate::{net::net_error_into_wasi_err, VirtualTaskManager};

#[derive(Debug)]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub enum InodeHttpSocketType {
    /// Used to feed the bytes into the request itself
    Request,
    /// Used to receive the bytes from the HTTP server
    Response,
    /// Used to read the headers from the HTTP server
    Headers,
}

#[derive(Debug)]
//#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub enum InodeSocketKind {
    PreSocket {
        family: Addressfamily,
        ty: Socktype,
        pt: SockProto,
        addr: Option<SocketAddr>,
        only_v6: bool,
        reuse_port: bool,
        reuse_addr: bool,
        send_buf_size: Option<usize>,
        recv_buf_size: Option<usize>,
        write_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
        accept_timeout: Option<Duration>,
        connect_timeout: Option<Duration>,
    },
    Icmp(Box<dyn VirtualIcmpSocket + Sync>),
    Raw(Box<dyn VirtualRawSocket + Sync>),
    TcpListener {
        socket: Box<dyn VirtualTcpListener + Sync>,
        accept_timeout: Option<Duration>,
    },
    TcpStream {
        socket: Box<dyn VirtualTcpSocket + Sync>,
        write_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
    },
    UdpSocket {
        socket: Box<dyn VirtualUdpSocket + Sync>,
        peer: Option<SocketAddr>,
    },
}

pub enum WasiSocketOption {
    Noop,
    ReusePort,
    ReuseAddr,
    NoDelay,
    DontRoute,
    OnlyV6,
    Broadcast,
    MulticastLoopV4,
    MulticastLoopV6,
    Promiscuous,
    Listening,
    LastError,
    KeepAlive,
    Linger,
    OobInline,
    RecvBufSize,
    SendBufSize,
    RecvLowat,
    SendLowat,
    RecvTimeout,
    SendTimeout,
    ConnectTimeout,
    AcceptTimeout,
    Ttl,
    MulticastTtlV4,
    Type,
    Proto,
}

impl From<Sockoption> for WasiSocketOption {
    fn from(opt: Sockoption) -> Self {
        use WasiSocketOption::*;
        match opt {
            Sockoption::Noop => Noop,
            Sockoption::ReusePort => ReusePort,
            Sockoption::ReuseAddr => ReuseAddr,
            Sockoption::NoDelay => NoDelay,
            Sockoption::DontRoute => DontRoute,
            Sockoption::OnlyV6 => OnlyV6,
            Sockoption::Broadcast => Broadcast,
            Sockoption::MulticastLoopV4 => MulticastLoopV4,
            Sockoption::MulticastLoopV6 => MulticastLoopV6,
            Sockoption::Promiscuous => Promiscuous,
            Sockoption::Listening => Listening,
            Sockoption::LastError => LastError,
            Sockoption::KeepAlive => KeepAlive,
            Sockoption::Linger => Linger,
            Sockoption::OobInline => OobInline,
            Sockoption::RecvBufSize => RecvBufSize,
            Sockoption::SendBufSize => SendBufSize,
            Sockoption::RecvLowat => RecvLowat,
            Sockoption::SendLowat => SendLowat,
            Sockoption::RecvTimeout => RecvTimeout,
            Sockoption::SendTimeout => SendTimeout,
            Sockoption::ConnectTimeout => ConnectTimeout,
            Sockoption::AcceptTimeout => AcceptTimeout,
            Sockoption::Ttl => Ttl,
            Sockoption::MulticastTtlV4 => MulticastTtlV4,
            Sockoption::Type => Type,
            Sockoption::Proto => Proto,
        }
    }
}

#[derive(Debug)]
pub enum WasiSocketStatus {
    Opening,
    Opened,
    Closed,
    Failed,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TimeType {
    ReadTimeout,
    WriteTimeout,
    AcceptTimeout,
    ConnectTimeout,
    BindTimeout,
    Linger,
}

#[derive(Debug)]
//#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub(crate) struct InodeSocketProtected {
    pub kind: InodeSocketKind,
    pub notifications: InodeSocketNotifications,
    pub aggregate_handler: Option<FilteredHandlerSubscriptions>,
    pub handler_state: StatefulHandlerState,
}

#[derive(Debug, Default)]
//#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub(crate) struct InodeSocketNotifications {
    pub closed: bool,
    pub failed: bool,
}

#[derive(Debug)]
//#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub(crate) struct InodeSocketInner {
    pub protected: RwLock<InodeSocketProtected>,
}

#[derive(Debug, Clone)]
//#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub struct InodeSocket {
    pub(crate) inner: Arc<InodeSocketInner>,
}

impl InodeSocket {
    pub fn new(kind: InodeSocketKind) -> Self {
        let handler_state: StatefulHandlerState = Default::default();
        if let InodeSocketKind::TcpStream { .. } = &kind {
            handler_state.set(InterestType::Writable);
        }
        Self {
            inner: Arc::new(InodeSocketInner {
                protected: RwLock::new(InodeSocketProtected {
                    kind,
                    notifications: Default::default(),
                    aggregate_handler: None,
                    handler_state,
                }),
            }),
        }
    }

    pub async fn bind(
        &self,
        tasks: &dyn VirtualTaskManager,
        net: &dyn VirtualNetworking,
        set_addr: SocketAddr,
    ) -> Result<Option<InodeSocket>, Errno> {
        let timeout = self
            .opt_time(TimeType::BindTimeout)
            .ok()
            .flatten()
            .unwrap_or(Duration::from_secs(30));

        let socket = {
            let mut inner = self.inner.protected.write().unwrap();
            match &mut inner.kind {
                InodeSocketKind::PreSocket {
                    family,
                    ty,
                    addr,
                    reuse_port,
                    reuse_addr,
                    ..
                } => {
                    match *family {
                        Addressfamily::Inet4 => {
                            if !set_addr.is_ipv4() {
                                tracing::debug!(
                                    "IP address is the wrong type IPv4 ({set_addr}) vs IPv6 family"
                                );
                                return Err(Errno::Inval);
                            }
                        }
                        Addressfamily::Inet6 => {
                            if !set_addr.is_ipv6() {
                                tracing::debug!(
                                    "IP address is the wrong type IPv6 ({set_addr}) vs IPv4 family"
                                );
                                return Err(Errno::Inval);
                            }
                        }
                        _ => {
                            return Err(Errno::Notsup);
                        }
                    }

                    addr.replace(set_addr);
                    let addr = (*addr).unwrap();

                    match *ty {
                        Socktype::Stream => {
                            // we already set the socket address - next we need a bind or connect so nothing
                            // more to do at this time
                            return Ok(None);
                        }
                        Socktype::Dgram => {
                            let reuse_port = *reuse_port;
                            let reuse_addr = *reuse_addr;
                            drop(inner);

                            net.bind_udp(addr, reuse_port, reuse_addr)
                        }
                        _ => return Err(Errno::Inval),
                    }
                }
                _ => return Err(Errno::Notsup),
            }
        };

        tokio::select! {
            socket = socket => {
                let socket = socket.map_err(net_error_into_wasi_err)?;
                Ok(Some(InodeSocket::new(InodeSocketKind::UdpSocket { socket, peer: None })))
            },
            _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
        }
    }

    pub async fn listen(
        &self,
        tasks: &dyn VirtualTaskManager,
        net: &dyn VirtualNetworking,
        _backlog: usize,
    ) -> Result<Option<InodeSocket>, Errno> {
        let timeout = self
            .opt_time(TimeType::AcceptTimeout)
            .ok()
            .flatten()
            .unwrap_or(Duration::from_secs(30));

        let socket = {
            let inner = self.inner.protected.read().unwrap();
            match &inner.kind {
                InodeSocketKind::PreSocket {
                    ty,
                    addr,
                    only_v6,
                    reuse_port,
                    reuse_addr,
                    ..
                } => match *ty {
                    Socktype::Stream => {
                        if addr.is_none() {
                            tracing::warn!("wasi[?]::sock_listen - failed - address not set");
                            return Err(Errno::Inval);
                        }
                        let addr = *addr.as_ref().unwrap();
                        let only_v6 = *only_v6;
                        let reuse_port = *reuse_port;
                        let reuse_addr = *reuse_addr;
                        drop(inner);

                        net.listen_tcp(addr, only_v6, reuse_port, reuse_addr)
                    }
                    _ => {
                        tracing::warn!("wasi[?]::sock_listen - failed - not supported(1)");
                        return Err(Errno::Notsup);
                    }
                },
                _ => {
                    tracing::warn!("wasi[?]::sock_listen - failed - not supported(2)");
                    return Err(Errno::Notsup);
                }
            }
        };

        tokio::select! {
            socket = socket => {
                let socket = socket.map_err(net_error_into_wasi_err)?;
                Ok(Some(InodeSocket::new(InodeSocketKind::TcpListener {
                    socket,
                    accept_timeout: Some(timeout),
                })))
            },
            _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
        }
    }

    pub async fn accept(
        &self,
        tasks: &dyn VirtualTaskManager,
        nonblocking: bool,
        timeout: Option<Duration>,
    ) -> Result<(Box<dyn VirtualTcpSocket + Sync>, SocketAddr), Errno> {
        struct SocketAccepter<'a> {
            sock: &'a InodeSocket,
            nonblocking: bool,
            handler_registered: bool,
        }
        impl<'a> Drop for SocketAccepter<'a> {
            fn drop(&mut self) {
                if self.handler_registered {
                    let mut inner = self.sock.inner.protected.write().unwrap();
                    inner.remove_handler();
                }
            }
        }
        impl<'a> Future for SocketAccepter<'a> {
            type Output = Result<(Box<dyn VirtualTcpSocket + Sync>, SocketAddr), Errno>;
            fn poll(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                loop {
                    let mut inner = self.sock.inner.protected.write().unwrap();
                    return match &mut inner.kind {
                        InodeSocketKind::TcpListener { socket, .. } => match socket.try_accept() {
                            Ok((child, addr)) => Poll::Ready(Ok((child, addr))),
                            Err(NetworkError::WouldBlock) if self.nonblocking => {
                                Poll::Ready(Err(Errno::Again))
                            }
                            Err(NetworkError::WouldBlock) if !self.handler_registered => {
                                let res = socket.set_handler(cx.waker().into());
                                if let Err(err) = res {
                                    return Poll::Ready(Err(net_error_into_wasi_err(err)));
                                }
                                drop(inner);
                                self.handler_registered = true;
                                continue;
                            }
                            Err(NetworkError::WouldBlock) => Poll::Pending,
                            Err(err) => Poll::Ready(Err(net_error_into_wasi_err(err))),
                        },
                        InodeSocketKind::PreSocket { .. } => Poll::Ready(Err(Errno::Notconn)),
                        _ => Poll::Ready(Err(Errno::Notsup)),
                    };
                }
            }
        }

        let acceptor = SocketAccepter {
            sock: self,
            nonblocking,
            handler_registered: false,
        };
        if let Some(timeout) = timeout {
            tokio::select! {
                res = acceptor => res,
                _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
            }
        } else {
            acceptor.await
        }
    }

    pub fn close(&self) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::TcpListener { .. } => {}
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.close().map_err(net_error_into_wasi_err)?;
            }
            InodeSocketKind::Icmp(_) => {}
            InodeSocketKind::UdpSocket { .. } => {}
            InodeSocketKind::Raw(_) => {}
            InodeSocketKind::PreSocket { .. } => return Err(Errno::Notconn),
        };
        Ok(())
    }

    pub async fn connect(
        &mut self,
        tasks: &dyn VirtualTaskManager,
        net: &dyn VirtualNetworking,
        peer: SocketAddr,
        timeout: Option<std::time::Duration>,
    ) -> Result<Option<InodeSocket>, Errno> {
        let new_write_timeout;
        let new_read_timeout;

        let timeout = timeout.unwrap_or(Duration::from_secs(30));

        let connect = {
            let mut inner = self.inner.protected.write().unwrap();
            match &mut inner.kind {
                InodeSocketKind::PreSocket {
                    ty,
                    addr,
                    write_timeout,
                    read_timeout,
                    ..
                } => {
                    new_write_timeout = *write_timeout;
                    new_read_timeout = *read_timeout;
                    match *ty {
                        Socktype::Stream => {
                            let addr = match addr {
                                Some(a) => *a,
                                None => {
                                    let ip = match peer.is_ipv4() {
                                        true => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                        false => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                                    };
                                    SocketAddr::new(ip, 0)
                                }
                            };
                            net.connect_tcp(addr, peer)
                        }
                        Socktype::Dgram => return Err(Errno::Inval),
                        _ => return Err(Errno::Notsup),
                    }
                }
                InodeSocketKind::UdpSocket {
                    peer: target_peer, ..
                } => {
                    target_peer.replace(peer);
                    return Ok(None);
                }
                _ => return Err(Errno::Notsup),
            }
        };

        let socket = tokio::select! {
            res = connect => res.map_err(net_error_into_wasi_err)?,
            _ = tasks.sleep_now(timeout) => return Err(Errno::Timedout)
        };
        Ok(Some(InodeSocket::new(InodeSocketKind::TcpStream {
            socket,
            write_timeout: new_write_timeout,
            read_timeout: new_read_timeout,
        })))
    }

    pub fn status(&self) -> Result<WasiSocketStatus, Errno> {
        let inner = self.inner.protected.read().unwrap();
        Ok(match &inner.kind {
            InodeSocketKind::PreSocket { .. } => WasiSocketStatus::Opening,
            InodeSocketKind::TcpListener { .. } => WasiSocketStatus::Opened,
            InodeSocketKind::TcpStream { .. } => WasiSocketStatus::Opened,
            InodeSocketKind::UdpSocket { .. } => WasiSocketStatus::Opened,
            _ => WasiSocketStatus::Failed,
        })
    }

    pub fn addr_local(&self) -> Result<SocketAddr, Errno> {
        let inner = self.inner.protected.read().unwrap();
        Ok(match &inner.kind {
            InodeSocketKind::PreSocket { family, addr, .. } => {
                if let Some(addr) = addr {
                    *addr
                } else {
                    SocketAddr::new(
                        match *family {
                            Addressfamily::Inet4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                            Addressfamily::Inet6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                            _ => return Err(Errno::Inval),
                        },
                        0,
                    )
                }
            }
            InodeSocketKind::Icmp(sock) => sock.addr_local().map_err(net_error_into_wasi_err)?,
            InodeSocketKind::TcpListener { socket, .. } => {
                socket.addr_local().map_err(net_error_into_wasi_err)?
            }
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.addr_local().map_err(net_error_into_wasi_err)?
            }
            InodeSocketKind::UdpSocket { socket, .. } => {
                socket.addr_local().map_err(net_error_into_wasi_err)?
            }
            _ => return Err(Errno::Notsup),
        })
    }

    pub fn addr_peer(&self) -> Result<SocketAddr, Errno> {
        let inner = self.inner.protected.read().unwrap();
        Ok(match &inner.kind {
            InodeSocketKind::PreSocket { family, .. } => SocketAddr::new(
                match *family {
                    Addressfamily::Inet4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    Addressfamily::Inet6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    _ => return Err(Errno::Inval),
                },
                0,
            ),
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.addr_peer().map_err(net_error_into_wasi_err)?
            }
            InodeSocketKind::UdpSocket { socket, .. } => socket
                .addr_peer()
                .map_err(net_error_into_wasi_err)?
                .map(Ok)
                .unwrap_or_else(|| {
                    socket
                        .addr_local()
                        .map_err(net_error_into_wasi_err)
                        .map(|addr| {
                            SocketAddr::new(
                                match addr {
                                    SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                    SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                                },
                                0,
                            )
                        })
                })?,
            _ => return Err(Errno::Notsup),
        })
    }

    pub fn set_opt_flag(&mut self, option: WasiSocketOption, val: bool) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::PreSocket {
                only_v6,
                reuse_port,
                reuse_addr,
                ..
            } => {
                match option {
                    WasiSocketOption::OnlyV6 => *only_v6 = val,
                    WasiSocketOption::ReusePort => *reuse_port = val,
                    WasiSocketOption::ReuseAddr => *reuse_addr = val,
                    _ => return Err(Errno::Inval),
                };
            }
            InodeSocketKind::Raw(sock) => match option {
                WasiSocketOption::Promiscuous => {
                    sock.set_promiscuous(val).map_err(net_error_into_wasi_err)?
                }
                _ => return Err(Errno::Inval),
            },
            InodeSocketKind::TcpStream { socket, .. } => match option {
                WasiSocketOption::NoDelay => {
                    socket.set_nodelay(val).map_err(net_error_into_wasi_err)?
                }
                _ => return Err(Errno::Inval),
            },
            InodeSocketKind::UdpSocket { socket, .. } => match option {
                WasiSocketOption::Broadcast => {
                    socket.set_broadcast(val).map_err(net_error_into_wasi_err)?
                }
                WasiSocketOption::MulticastLoopV4 => socket
                    .set_multicast_loop_v4(val)
                    .map_err(net_error_into_wasi_err)?,
                WasiSocketOption::MulticastLoopV6 => socket
                    .set_multicast_loop_v6(val)
                    .map_err(net_error_into_wasi_err)?,
                _ => return Err(Errno::Inval),
            },
            _ => return Err(Errno::Notsup),
        }
        Ok(())
    }

    pub fn get_opt_flag(&self, option: WasiSocketOption) -> Result<bool, Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        Ok(match &mut inner.kind {
            InodeSocketKind::PreSocket {
                only_v6,
                reuse_port,
                reuse_addr,
                ..
            } => match option {
                WasiSocketOption::OnlyV6 => *only_v6,
                WasiSocketOption::ReusePort => *reuse_port,
                WasiSocketOption::ReuseAddr => *reuse_addr,
                _ => return Err(Errno::Inval),
            },
            InodeSocketKind::Raw(sock) => match option {
                WasiSocketOption::Promiscuous => {
                    sock.promiscuous().map_err(net_error_into_wasi_err)?
                }
                _ => return Err(Errno::Inval),
            },
            InodeSocketKind::TcpStream { socket, .. } => match option {
                WasiSocketOption::NoDelay => socket.nodelay().map_err(net_error_into_wasi_err)?,
                _ => return Err(Errno::Inval),
            },
            InodeSocketKind::UdpSocket { socket, .. } => match option {
                WasiSocketOption::Broadcast => {
                    socket.broadcast().map_err(net_error_into_wasi_err)?
                }
                WasiSocketOption::MulticastLoopV4 => socket
                    .multicast_loop_v4()
                    .map_err(net_error_into_wasi_err)?,
                WasiSocketOption::MulticastLoopV6 => socket
                    .multicast_loop_v6()
                    .map_err(net_error_into_wasi_err)?,
                _ => return Err(Errno::Inval),
            },
            _ => return Err(Errno::Notsup),
        })
    }

    pub fn set_send_buf_size(&mut self, size: usize) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::PreSocket { send_buf_size, .. } => {
                *send_buf_size = Some(size);
            }
            InodeSocketKind::TcpStream { socket, .. } => {
                socket
                    .set_send_buf_size(size)
                    .map_err(net_error_into_wasi_err)?;
            }
            _ => return Err(Errno::Notsup),
        }
        Ok(())
    }

    pub fn send_buf_size(&self) -> Result<usize, Errno> {
        let inner = self.inner.protected.read().unwrap();
        match &inner.kind {
            InodeSocketKind::PreSocket { send_buf_size, .. } => {
                Ok((*send_buf_size).unwrap_or_default())
            }
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.send_buf_size().map_err(net_error_into_wasi_err)
            }
            _ => Err(Errno::Notsup),
        }
    }

    pub fn set_recv_buf_size(&mut self, size: usize) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::PreSocket { recv_buf_size, .. } => {
                *recv_buf_size = Some(size);
            }
            InodeSocketKind::TcpStream { socket, .. } => {
                socket
                    .set_recv_buf_size(size)
                    .map_err(net_error_into_wasi_err)?;
            }
            _ => return Err(Errno::Notsup),
        }
        Ok(())
    }

    pub fn recv_buf_size(&self) -> Result<usize, Errno> {
        let inner = self.inner.protected.read().unwrap();
        match &inner.kind {
            InodeSocketKind::PreSocket { recv_buf_size, .. } => {
                Ok((*recv_buf_size).unwrap_or_default())
            }
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.recv_buf_size().map_err(net_error_into_wasi_err)
            }
            _ => Err(Errno::Notsup),
        }
    }

    pub fn set_linger(&mut self, linger: Option<std::time::Duration>) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.set_linger(linger).map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn linger(&self) -> Result<Option<std::time::Duration>, Errno> {
        let inner = self.inner.protected.read().unwrap();
        match &inner.kind {
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.linger().map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn set_opt_time(
        &self,
        ty: TimeType,
        timeout: Option<std::time::Duration>,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::TcpStream {
                write_timeout,
                read_timeout,
                ..
            } => {
                match ty {
                    TimeType::WriteTimeout => *write_timeout = timeout,
                    TimeType::ReadTimeout => *read_timeout = timeout,
                    _ => return Err(Errno::Inval),
                }
                Ok(())
            }
            InodeSocketKind::TcpListener { accept_timeout, .. } => {
                match ty {
                    TimeType::AcceptTimeout => *accept_timeout = timeout,
                    _ => return Err(Errno::Inval),
                }
                Ok(())
            }
            InodeSocketKind::PreSocket {
                read_timeout,
                write_timeout,
                connect_timeout,
                accept_timeout,
                ..
            } => {
                match ty {
                    TimeType::ConnectTimeout => *connect_timeout = timeout,
                    TimeType::AcceptTimeout => *accept_timeout = timeout,
                    TimeType::ReadTimeout => *read_timeout = timeout,
                    TimeType::WriteTimeout => *write_timeout = timeout,
                    _ => return Err(Errno::Io),
                }
                Ok(())
            }
            _ => Err(Errno::Notsup),
        }
    }

    pub fn opt_time(&self, ty: TimeType) -> Result<Option<std::time::Duration>, Errno> {
        let inner = self.inner.protected.read().unwrap();
        match &inner.kind {
            InodeSocketKind::TcpStream {
                read_timeout,
                write_timeout,
                ..
            } => Ok(match ty {
                TimeType::ReadTimeout => *read_timeout,
                TimeType::WriteTimeout => *write_timeout,
                _ => return Err(Errno::Inval),
            }),
            InodeSocketKind::TcpListener { accept_timeout, .. } => Ok(match ty {
                TimeType::AcceptTimeout => *accept_timeout,
                _ => return Err(Errno::Inval),
            }),
            InodeSocketKind::PreSocket {
                read_timeout,
                write_timeout,
                connect_timeout,
                accept_timeout,
                ..
            } => match ty {
                TimeType::ConnectTimeout => Ok(*connect_timeout),
                TimeType::AcceptTimeout => Ok(*accept_timeout),
                TimeType::ReadTimeout => Ok(*read_timeout),
                TimeType::WriteTimeout => Ok(*write_timeout),
                _ => Err(Errno::Inval),
            },
            _ => Err(Errno::Notsup),
        }
    }

    pub fn set_ttl(&self, ttl: u32) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.set_ttl(ttl).map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::UdpSocket { socket, .. } => {
                socket.set_ttl(ttl).map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn ttl(&self) -> Result<u32, Errno> {
        let inner = self.inner.protected.read().unwrap();
        match &inner.kind {
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.ttl().map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::UdpSocket { socket, .. } => {
                socket.ttl().map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::UdpSocket { socket, .. } => socket
                .set_multicast_ttl_v4(ttl)
                .map_err(net_error_into_wasi_err),
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn multicast_ttl_v4(&self) -> Result<u32, Errno> {
        let inner = self.inner.protected.read().unwrap();
        match &inner.kind {
            InodeSocketKind::UdpSocket { socket, .. } => {
                socket.multicast_ttl_v4().map_err(net_error_into_wasi_err)
            }
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn join_multicast_v4(&self, multiaddr: Ipv4Addr, iface: Ipv4Addr) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::UdpSocket { socket, .. } => socket
                .join_multicast_v4(multiaddr, iface)
                .map_err(net_error_into_wasi_err),
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn leave_multicast_v4(&self, multiaddr: Ipv4Addr, iface: Ipv4Addr) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::UdpSocket { socket, .. } => socket
                .leave_multicast_v4(multiaddr, iface)
                .map_err(net_error_into_wasi_err),
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn join_multicast_v6(&self, multiaddr: Ipv6Addr, iface: u32) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::UdpSocket { socket, .. } => socket
                .join_multicast_v6(multiaddr, iface)
                .map_err(net_error_into_wasi_err),
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub fn leave_multicast_v6(&mut self, multiaddr: Ipv6Addr, iface: u32) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::UdpSocket { socket, .. } => socket
                .leave_multicast_v6(multiaddr, iface)
                .map_err(net_error_into_wasi_err),
            InodeSocketKind::PreSocket { .. } => Err(Errno::Io),
            _ => Err(Errno::Notsup),
        }
    }

    pub async fn send(
        &self,
        tasks: &dyn VirtualTaskManager,
        buf: &[u8],
        timeout: Option<Duration>,
        nonblocking: bool,
    ) -> Result<usize, Errno> {
        struct SocketSender<'a, 'b> {
            inner: &'a InodeSocketInner,
            data: &'b [u8],
            nonblocking: bool,
            handler_registered: bool,
        }
        impl<'a, 'b> Drop for SocketSender<'a, 'b> {
            fn drop(&mut self) {
                if self.handler_registered {
                    let mut inner = self.inner.protected.write().unwrap();
                    inner.remove_handler();
                }
            }
        }
        impl<'a, 'b> Future for SocketSender<'a, 'b> {
            type Output = Result<usize, Errno>;
            fn poll(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> Poll<Self::Output> {
                loop {
                    let mut inner = self.inner.protected.write().unwrap();
                    let res = match &mut inner.kind {
                        InodeSocketKind::Raw(socket) => socket.try_send(self.data),
                        InodeSocketKind::TcpStream { socket, .. } => socket.try_send(self.data),
                        InodeSocketKind::UdpSocket { socket, peer } => {
                            if let Some(peer) = peer {
                                socket.try_send_to(self.data, *peer)
                            } else {
                                Err(NetworkError::NotConnected)
                            }
                        }
                        InodeSocketKind::PreSocket { .. } => {
                            return Poll::Ready(Err(Errno::Notconn))
                        }
                        _ => return Poll::Ready(Err(Errno::Notsup)),
                    };
                    return match res {
                        Ok(amt) => Poll::Ready(Ok(amt)),
                        Err(NetworkError::WouldBlock) if self.nonblocking => {
                            Poll::Ready(Err(Errno::Again))
                        }
                        Err(NetworkError::WouldBlock) if !self.handler_registered => {
                            let res = inner.set_handler(cx.waker().into());
                            if let Err(err) = res {
                                return Poll::Ready(Err(net_error_into_wasi_err(err)));
                            }
                            drop(inner);
                            self.handler_registered = true;
                            continue;
                        }
                        Err(NetworkError::WouldBlock) => Poll::Pending,
                        Err(err) => Poll::Ready(Err(net_error_into_wasi_err(err))),
                    };
                }
            }
        }

        let poller = SocketSender {
            inner: &self.inner,
            data: buf,
            nonblocking,
            handler_registered: false,
        };
        if let Some(timeout) = timeout {
            tokio::select! {
                res = poller => res,
                _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
            }
        } else {
            poller.await
        }
    }

    pub async fn send_to<M: MemorySize>(
        &self,
        tasks: &dyn VirtualTaskManager,
        buf: &[u8],
        addr: SocketAddr,
        timeout: Option<Duration>,
        nonblocking: bool,
    ) -> Result<usize, Errno> {
        struct SocketSender<'a, 'b> {
            inner: &'a InodeSocketInner,
            data: &'b [u8],
            addr: SocketAddr,
            nonblocking: bool,
            handler_registered: bool,
        }
        impl<'a, 'b> Drop for SocketSender<'a, 'b> {
            fn drop(&mut self) {
                if self.handler_registered {
                    let mut inner = self.inner.protected.write().unwrap();
                    inner.remove_handler();
                }
            }
        }
        impl<'a, 'b> Future for SocketSender<'a, 'b> {
            type Output = Result<usize, Errno>;
            fn poll(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> Poll<Self::Output> {
                loop {
                    let mut inner = self.inner.protected.write().unwrap();
                    let res = match &mut inner.kind {
                        InodeSocketKind::Icmp(socket) => socket.try_send_to(self.data, self.addr),
                        InodeSocketKind::UdpSocket { socket, .. } => {
                            socket.try_send_to(self.data, self.addr)
                        }
                        InodeSocketKind::PreSocket { .. } => {
                            return Poll::Ready(Err(Errno::Notconn))
                        }
                        _ => return Poll::Ready(Err(Errno::Notsup)),
                    };
                    return match res {
                        Ok(amt) => Poll::Ready(Ok(amt)),
                        Err(NetworkError::WouldBlock) if self.nonblocking => {
                            Poll::Ready(Err(Errno::Again))
                        }
                        Err(NetworkError::WouldBlock) if !self.handler_registered => {
                            let res = inner.set_handler(cx.waker().into());
                            if let Err(err) = res {
                                return Poll::Ready(Err(net_error_into_wasi_err(err)));
                            }
                            self.handler_registered = true;
                            drop(inner);
                            continue;
                        }
                        Err(NetworkError::WouldBlock) => Poll::Pending,
                        Err(err) => Poll::Ready(Err(net_error_into_wasi_err(err))),
                    };
                }
            }
        }

        let poller = SocketSender {
            inner: &self.inner,
            data: buf,
            addr,
            nonblocking,
            handler_registered: false,
        };
        if let Some(timeout) = timeout {
            tokio::select! {
                res = poller => res,
                _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
            }
        } else {
            poller.await
        }
    }

    pub async fn recv(
        &self,
        tasks: &dyn VirtualTaskManager,
        buf: &mut [MaybeUninit<u8>],
        timeout: Option<Duration>,
        nonblocking: bool,
    ) -> Result<usize, Errno> {
        struct SocketReceiver<'a, 'b> {
            inner: &'a InodeSocketInner,
            data: &'b mut [MaybeUninit<u8>],
            nonblocking: bool,
            handler_registered: bool,
        }
        impl<'a, 'b> Drop for SocketReceiver<'a, 'b> {
            fn drop(&mut self) {
                if self.handler_registered {
                    let mut inner = self.inner.protected.write().unwrap();
                    inner.remove_handler();
                }
            }
        }
        impl<'a, 'b> Future for SocketReceiver<'a, 'b> {
            type Output = Result<usize, Errno>;
            fn poll(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> Poll<Self::Output> {
                loop {
                    let mut inner = self.inner.protected.write().unwrap();
                    let res = match &mut inner.kind {
                        InodeSocketKind::Raw(socket) => socket.try_recv(self.data),
                        InodeSocketKind::TcpStream { socket, .. } => socket.try_recv(self.data),
                        InodeSocketKind::UdpSocket { socket, peer } => {
                            if let Some(peer) = peer {
                                match socket.try_recv_from(self.data) {
                                    Ok((amt, addr)) if addr == *peer => Ok(amt),
                                    Ok(_) => Err(NetworkError::WouldBlock),
                                    Err(err) => Err(err),
                                }
                            } else {
                                Err(NetworkError::NotConnected)
                            }
                        }
                        InodeSocketKind::PreSocket { .. } => {
                            return Poll::Ready(Err(Errno::Notconn))
                        }
                        _ => return Poll::Ready(Err(Errno::Notsup)),
                    };
                    return match res {
                        Ok(amt) => Poll::Ready(Ok(amt)),
                        Err(NetworkError::WouldBlock) if self.nonblocking => {
                            Poll::Ready(Err(Errno::Again))
                        }
                        Err(NetworkError::WouldBlock) if !self.handler_registered => {
                            let res = inner.set_handler(cx.waker().into());
                            if let Err(err) = res {
                                return Poll::Ready(Err(net_error_into_wasi_err(err)));
                            }
                            self.handler_registered = true;
                            drop(inner);
                            continue;
                        }

                        Err(NetworkError::WouldBlock) => Poll::Pending,
                        Err(err) => Poll::Ready(Err(net_error_into_wasi_err(err))),
                    };
                }
            }
        }

        let poller = SocketReceiver {
            inner: &self.inner,
            data: buf,
            nonblocking,
            handler_registered: false,
        };
        if let Some(timeout) = timeout {
            tokio::select! {
                res = poller => res,
                _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
            }
        } else {
            poller.await
        }
    }

    pub async fn recv_from(
        &self,
        tasks: &dyn VirtualTaskManager,
        buf: &mut [MaybeUninit<u8>],
        timeout: Option<Duration>,
        nonblocking: bool,
    ) -> Result<(usize, SocketAddr), Errno> {
        struct SocketReceiver<'a, 'b> {
            inner: &'a InodeSocketInner,
            data: &'b mut [MaybeUninit<u8>],
            nonblocking: bool,
            handler_registered: bool,
        }
        impl<'a, 'b> Drop for SocketReceiver<'a, 'b> {
            fn drop(&mut self) {
                if self.handler_registered {
                    let mut inner = self.inner.protected.write().unwrap();
                    inner.remove_handler();
                }
            }
        }
        impl<'a, 'b> Future for SocketReceiver<'a, 'b> {
            type Output = Result<(usize, SocketAddr), Errno>;
            fn poll(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> Poll<Self::Output> {
                let mut inner = self.inner.protected.write().unwrap();
                loop {
                    let res = match &mut inner.kind {
                        InodeSocketKind::Icmp(socket) => socket.try_recv_from(self.data),
                        InodeSocketKind::UdpSocket { socket, .. } => {
                            socket.try_recv_from(self.data)
                        }
                        InodeSocketKind::PreSocket { .. } => {
                            return Poll::Ready(Err(Errno::Notconn))
                        }
                        _ => return Poll::Ready(Err(Errno::Notsup)),
                    };
                    return match res {
                        Ok((amt, addr)) => Poll::Ready(Ok((amt, addr))),
                        Err(NetworkError::WouldBlock) if self.nonblocking => {
                            Poll::Ready(Err(Errno::Again))
                        }
                        Err(NetworkError::WouldBlock) if !self.handler_registered => {
                            let res = inner.set_handler(cx.waker().into());
                            if let Err(err) = res {
                                return Poll::Ready(Err(net_error_into_wasi_err(err)));
                            }
                            self.handler_registered = true;
                            continue;
                        }
                        Err(NetworkError::WouldBlock) => Poll::Pending,
                        Err(err) => Poll::Ready(Err(net_error_into_wasi_err(err))),
                    };
                }
            }
        }

        let poller = SocketReceiver {
            inner: &self.inner,
            data: buf,
            nonblocking,
            handler_registered: false,
        };
        if let Some(timeout) = timeout {
            tokio::select! {
                res = poller => res,
                _ = tasks.sleep_now(timeout) => Err(Errno::Timedout)
            }
        } else {
            poller.await
        }
    }

    pub fn shutdown(&mut self, how: std::net::Shutdown) -> Result<(), Errno> {
        let mut inner = self.inner.protected.write().unwrap();
        match &mut inner.kind {
            InodeSocketKind::TcpStream { socket, .. } => {
                socket.shutdown(how).map_err(net_error_into_wasi_err)?;
            }
            InodeSocketKind::PreSocket { .. } => return Err(Errno::Notconn),
            _ => return Err(Errno::Notsup),
        }
        Ok(())
    }

    pub async fn can_write(&self) -> bool {
        if let Ok(mut guard) = self.inner.protected.try_write() {
            #[allow(clippy::match_like_matches_macro)]
            match &mut guard.kind {
                InodeSocketKind::TcpStream { .. }
                | InodeSocketKind::UdpSocket { .. }
                | InodeSocketKind::Raw(..) => true,
                _ => false,
            }
        } else {
            false
        }
    }
}

impl InodeSocketProtected {
    pub fn remove_handler(&mut self) {
        match &mut self.kind {
            InodeSocketKind::TcpListener { socket, .. } => socket.remove_handler(),
            InodeSocketKind::TcpStream { socket, .. } => socket.remove_handler(),
            InodeSocketKind::UdpSocket { socket, .. } => socket.remove_handler(),
            InodeSocketKind::Raw(socket) => socket.remove_handler(),
            InodeSocketKind::Icmp(socket) => socket.remove_handler(),
            InodeSocketKind::PreSocket { .. } => {}
        }
    }

    pub fn set_handler(
        &mut self,
        handler: Box<dyn InterestHandler + Send + Sync>,
    ) -> virtual_net::Result<()> {
        match &mut self.kind {
            InodeSocketKind::TcpListener { socket, .. } => socket.set_handler(handler),
            InodeSocketKind::TcpStream { socket, .. } => socket.set_handler(handler),
            InodeSocketKind::UdpSocket { socket, .. } => socket.set_handler(handler),
            InodeSocketKind::Raw(socket) => socket.set_handler(handler),
            InodeSocketKind::Icmp(socket) => socket.set_handler(handler),
            InodeSocketKind::PreSocket { .. } => Err(virtual_net::NetworkError::NotConnected),
        }
    }

    pub fn add_handler(
        &mut self,
        handler: Box<dyn InterestHandler + Send + Sync>,
        interest: InterestType,
    ) -> virtual_net::Result<()> {
        if self.aggregate_handler.is_none() {
            let upper = FilteredHandler::new();
            let subs = upper.subscriptions().clone();

            self.set_handler(upper)?;
            self.aggregate_handler.replace(subs);
        }
        let upper = self.aggregate_handler.as_mut().unwrap();
        upper.add_interest(interest, handler);
        Ok(())
    }
}

#[derive(Default)]
struct IndefinitePoll {}

impl Future for IndefinitePoll {
    type Output = ();
    fn poll(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Pending
    }
}

// TODO: review allow...
#[allow(dead_code)]
pub(crate) fn all_socket_rights() -> Rights {
    Rights::FD_FDSTAT_SET_FLAGS
        .union(Rights::FD_FILESTAT_GET)
        .union(Rights::FD_READ)
        .union(Rights::FD_WRITE)
        .union(Rights::POLL_FD_READWRITE)
        .union(Rights::SOCK_SHUTDOWN)
        .union(Rights::SOCK_CONNECT)
        .union(Rights::SOCK_LISTEN)
        .union(Rights::SOCK_BIND)
        .union(Rights::SOCK_ACCEPT)
        .union(Rights::SOCK_RECV)
        .union(Rights::SOCK_SEND)
        .union(Rights::SOCK_ADDR_LOCAL)
        .union(Rights::SOCK_ADDR_REMOTE)
        .union(Rights::SOCK_RECV_FROM)
        .union(Rights::SOCK_SEND_TO)
}