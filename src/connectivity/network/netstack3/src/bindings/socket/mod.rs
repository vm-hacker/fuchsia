// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Socket features exposed by netstack3.

pub(crate) mod datagram;
pub(crate) mod packet;
pub(crate) mod raw;
pub(crate) mod stream;

use std::convert::Infallible as Never;
use std::num::NonZeroU64;

use fidl_fuchsia_net as fnet;
use fidl_fuchsia_posix::Errno;
use fidl_fuchsia_posix_socket as psocket;
use fuchsia_zircon as zx;
use futures::{TryFutureExt as _, TryStreamExt as _};
use net_types::{
    ip::{Ip, IpAddress, Ipv4, Ipv4Addr, Ipv6, Ipv6Addr},
    ScopeableAddress, SpecifiedAddr,
};
use netstack3_core::{
    error::{LocalAddressError, NetstackError, RemoteAddressError, SocketError, ZonedAddressError},
    ip::socket::{IpSockCreationError, IpSockRouteError, IpSockSendError, IpSockUnroutableError},
    socket::datagram::{MulticastInterfaceSelector, SetMulticastMembershipError},
    transport::udp::{
        UdpConnectListenerError, UdpSendError, UdpSendListenerError, UdpSockCreationError,
    },
    Ctx,
};

use crate::bindings::{
    devices::Devices,
    util::{IntoCore as _, IntoFidl as _},
    LockableContext,
};

// Socket constants defined in FDIO in
// https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/lib/fdio/socket.cc
// TODO(brunodalbo) Come back to this, see if we can have those definitions in a
// public header from FDIO somehow so we don't need to redefine.
const ZXSIO_SIGNAL_INCOMING: zx::Signals = zx::Signals::USER_0;
const ZXSIO_SIGNAL_OUTGOING: zx::Signals = zx::Signals::USER_1;
const ZXSIO_SIGNAL_CONNECTED: zx::Signals = zx::Signals::USER_3;

/// Common properties for socket workers.
#[derive(Debug)]
struct SocketWorkerProperties {}

pub(crate) async fn serve<C>(
    ctx: C,
    stream: psocket::ProviderRequestStream,
) -> Result<(), fidl::Error>
where
    C: LockableContext,
    C::NonSyncCtx:
        AsRef<Devices> + datagram::SocketWorkerDispatcher + stream::SocketWorkerDispatcher,
    C: Clone + Send + Sync + 'static,
{
    stream
        .try_fold(ctx, |ctx, req| async {
            match req {
                psocket::ProviderRequest::InterfaceIndexToName { index, responder } => {
                    let mut response = {
                        let ctx = ctx.lock().await;
                        let Ctx { sync_ctx: _, non_sync_ctx } = &*ctx;
                        let result = AsRef::<Devices>::as_ref(&non_sync_ctx)
                            .get_device(index)
                            .map(|device_info| device_info.info().common_info().name.clone())
                            .ok_or(zx::Status::NOT_FOUND.into_raw());
                        result
                    };
                    responder_send!(responder, &mut response);
                }
                psocket::ProviderRequest::InterfaceNameToIndex { name, responder } => {
                    let mut response = {
                        let ctx = ctx.lock().await;
                        let Ctx { sync_ctx: _, non_sync_ctx } = &*ctx;
                        let devices = AsRef::<Devices>::as_ref(&non_sync_ctx);
                        let result = devices
                            .get_device_by_name(&name)
                            .map(|d| d.id())
                            .ok_or(zx::Status::NOT_FOUND.into_raw());
                        result
                    };
                    responder_send!(responder, &mut response);
                }
                psocket::ProviderRequest::InterfaceNameToFlags { name: _, responder } => {
                    // TODO(https://fxbug.dev/48969): implement this method.
                    responder_send!(responder, &mut Err(zx::Status::NOT_FOUND.into_raw()));
                }
                psocket::ProviderRequest::StreamSocket { domain, proto, responder } => {
                    match fidl::endpoints::create_request_stream() {
                        Ok((client, request_stream)) => {
                            responder_send!(
                                responder,
                                &mut stream::spawn_worker(
                                    domain,
                                    proto,
                                    ctx.clone(),
                                    request_stream
                                )
                                .await
                                .map(|()| client)
                            );
                        }
                        Err(_) => responder_send!(responder, &mut Err(Errno::Enobufs)),
                    }
                }
                psocket::ProviderRequest::DatagramSocketDeprecated { domain, proto, responder } => {
                    let mut response = (|| {
                        let (client, request_stream) = fidl::endpoints::create_request_stream()
                            .map_err(|_: fidl::Error| Errno::Enobufs)?;
                        let () = datagram::spawn_worker(
                            domain,
                            proto,
                            ctx.clone(),
                            request_stream,
                            SocketWorkerProperties {},
                        )?;
                        Ok(client)
                    })();
                    responder_send!(responder, &mut response);
                }
                psocket::ProviderRequest::DatagramSocket { domain, proto, responder } => {
                    let mut response = (|| {
                        let (client, request_stream) = fidl::endpoints::create_request_stream()
                            .map_err(|_: fidl::Error| Errno::Enobufs)?;
                        let () = datagram::spawn_worker(
                            domain,
                            proto,
                            ctx.clone(),
                            request_stream,
                            SocketWorkerProperties {},
                        )?;
                        Ok(psocket::ProviderDatagramSocketResponse::SynchronousDatagramSocket(
                            client,
                        ))
                    })();
                    responder_send!(responder, &mut response);
                }
                psocket::ProviderRequest::GetInterfaceAddresses { responder } => {
                    // TODO(https://fxbug.dev/54162): implement this method.
                    responder_send!(responder, &mut std::iter::empty());
                }
            }
            Ok(ctx)
        })
        .map_ok(|_: C| ())
        .await
}

trait CanClone: Send + 'static {}

impl CanClone for fidl_fuchsia_io::NodeMarker {}
impl CanClone for fidl_fuchsia_unknown::CloneableMarker {}

/// A trait generalizing the data structures passed as arguments to POSIX socket
/// calls.
///
/// `SockAddr` implementers are typically passed to POSIX socket calls as a blob
/// of bytes. It represents a type that can be parsed from a C API `struct
/// sockaddr`, expressed as a stream of bytes.
pub(crate) trait SockAddr: std::fmt::Debug + Sized {
    /// The concrete address type for this `SockAddr`.
    type AddrType: IpAddress + ScopeableAddress;
    /// The socket's domain.
    const DOMAIN: psocket::Domain;
    /// The type of a zone identifier for this `SockAddr`.
    type Zone;

    /// Creates a new `SockAddr`.
    ///
    /// Implementations must set their family field to `Self::FAMILY`.
    fn new(addr: Self::AddrType, port: u16) -> Self;

    /// Gets this `SockAddr`'s address.
    fn addr(&self) -> Self::AddrType;

    /// Set this [`SockAddr`]'s address.
    fn set_addr(&mut self, addr: Self::AddrType);

    /// Gets this `SockAddr`'s port.
    fn port(&self) -> u16;

    /// Set this [`SockAddr`]'s port.
    fn set_port(&mut self, port: u16);

    /// Gets a `SpecifiedAddr` witness type for this `SockAddr`'s address.
    fn get_specified_addr(&self) -> Option<SpecifiedAddr<Self::AddrType>> {
        SpecifiedAddr::<Self::AddrType>::new(self.addr())
    }

    /// Gets this `SockAddr`'s zone identifier.
    fn zone(&self) -> Option<Self::Zone>;

    /// Converts this `SockAddr` into an [`fnet::SocketAddress`].
    fn into_sock_addr(self) -> fnet::SocketAddress;

    /// Converts an [`fnet::SocketAddress`] into a `SockAddr`.
    fn from_sock_addr(addr: fnet::SocketAddress) -> Result<Self, Errno>;
}

impl SockAddr for fnet::Ipv6SocketAddress {
    type AddrType = Ipv6Addr;
    const DOMAIN: psocket::Domain = psocket::Domain::Ipv6;
    type Zone = NonZeroU64;

    /// Creates a new `SockAddr6`.
    fn new(addr: Ipv6Addr, port: u16) -> Self {
        fnet::Ipv6SocketAddress { address: addr.into_fidl(), port, zone_index: 0 }
    }

    fn addr(&self) -> Ipv6Addr {
        self.address.into_core()
    }

    fn set_addr(&mut self, addr: Ipv6Addr) {
        self.address = addr.into_fidl();
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn set_port(&mut self, port: u16) {
        self.port = port
    }

    fn zone(&self) -> Option<Self::Zone> {
        NonZeroU64::new(self.zone_index)
    }

    fn into_sock_addr(self) -> fnet::SocketAddress {
        fnet::SocketAddress::Ipv6(self)
    }

    fn from_sock_addr(addr: fnet::SocketAddress) -> Result<Self, Errno> {
        match addr {
            fnet::SocketAddress::Ipv6(a) => Ok(a),
            fnet::SocketAddress::Ipv4(_) => Err(Errno::Eafnosupport),
        }
    }
}

impl SockAddr for fnet::Ipv4SocketAddress {
    type AddrType = Ipv4Addr;
    const DOMAIN: psocket::Domain = psocket::Domain::Ipv4;
    type Zone = Never;

    /// Creates a new `SockAddr4`.
    fn new(addr: Ipv4Addr, port: u16) -> Self {
        fnet::Ipv4SocketAddress { address: addr.into_fidl(), port }
    }

    fn addr(&self) -> Ipv4Addr {
        self.address.into_core()
    }

    fn set_addr(&mut self, addr: Ipv4Addr) {
        self.address = addr.into_fidl();
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn set_port(&mut self, port: u16) {
        self.port = port
    }

    fn zone(&self) -> Option<Self::Zone> {
        None
    }

    fn into_sock_addr(self) -> fnet::SocketAddress {
        fnet::SocketAddress::Ipv4(self)
    }

    fn from_sock_addr(addr: fnet::SocketAddress) -> Result<Self, Errno> {
        match addr {
            fnet::SocketAddress::Ipv4(a) => Ok(a),
            fnet::SocketAddress::Ipv6(_) => Err(Errno::Eafnosupport),
        }
    }
}

/// Extension trait that associates a [`SockAddr`] and [`MulticastMembership`]
/// implementation to an IP version. We provide implementations for [`Ipv4`] and
/// [`Ipv6`].
pub(crate) trait IpSockAddrExt: Ip {
    type SocketAddress: SockAddr<AddrType = Self::Addr>;
    type MulticastMembership: SockMulticastMembership<AddrType = Self::Addr> + Send;
}

impl IpSockAddrExt for Ipv4 {
    type SocketAddress = fnet::Ipv4SocketAddress;
    type MulticastMembership = psocket::IpMulticastMembership;
}

impl IpSockAddrExt for Ipv6 {
    type SocketAddress = fnet::Ipv6SocketAddress;
    type MulticastMembership = psocket::Ipv6MulticastMembership;
}

pub(crate) enum IpMulticastMembership {
    V4(psocket::IpMulticastMembership),
    V6(psocket::Ipv6MulticastMembership),
}

impl From<psocket::IpMulticastMembership> for IpMulticastMembership {
    fn from(membership: psocket::IpMulticastMembership) -> Self {
        Self::V4(membership)
    }
}

impl From<psocket::Ipv6MulticastMembership> for IpMulticastMembership {
    fn from(membership: psocket::Ipv6MulticastMembership) -> Self {
        Self::V6(membership)
    }
}

/// A trait for generalizing over multicast membership options for sockets.
pub(crate) trait SockMulticastMembership: Sized {
    type AddrType: IpAddress;

    fn new(membership: impl Into<IpMulticastMembership>) -> Option<Self>;

    fn into_addr_selector(
        self,
    ) -> (Self::AddrType, MulticastInterfaceSelector<Self::AddrType, NonZeroU64>);
}

impl SockMulticastMembership for psocket::IpMulticastMembership {
    type AddrType = Ipv4Addr;

    fn new(membership: impl Into<IpMulticastMembership>) -> Option<Self> {
        match membership.into() {
            IpMulticastMembership::V4(m) => Some(m),
            IpMulticastMembership::V6(_) => None,
        }
    }

    fn into_addr_selector(
        self,
    ) -> (Self::AddrType, MulticastInterfaceSelector<Self::AddrType, NonZeroU64>) {
        let Self { mcast_addr, iface, local_addr } = self;
        // Match Linux behavior by ignoring the address if an interface
        // identifier is provided.
        let selector = NonZeroU64::new(iface)
            .map(MulticastInterfaceSelector::Interface)
            .or_else(|| {
                SpecifiedAddr::new(local_addr.into_core())
                    .map(MulticastInterfaceSelector::LocalAddress)
            })
            .unwrap_or(MulticastInterfaceSelector::AnyInterfaceWithRoute);
        (mcast_addr.into_core(), selector)
    }
}

impl SockMulticastMembership for psocket::Ipv6MulticastMembership {
    type AddrType = Ipv6Addr;

    fn new(membership: impl Into<IpMulticastMembership>) -> Option<Self> {
        match membership.into() {
            IpMulticastMembership::V6(m) => Some(m),
            IpMulticastMembership::V4(_) => None,
        }
    }

    fn into_addr_selector(
        self,
    ) -> (Self::AddrType, MulticastInterfaceSelector<Self::AddrType, NonZeroU64>) {
        let Self { mcast_addr, iface } = self;
        let selector = NonZeroU64::new(iface)
            .map(MulticastInterfaceSelector::Interface)
            .unwrap_or(MulticastInterfaceSelector::AnyInterfaceWithRoute);
        (mcast_addr.into_core(), selector)
    }
}

#[cfg(test)]
mod testutil {
    use net_types::ip::{AddrSubnetEither, IpAddr};

    use super::*;

    /// A trait that exposes common test behavior to implementers of
    /// [`SockAddr`].
    pub(crate) trait TestSockAddr: SockAddr {
        /// A different domain.
        ///
        /// `Ipv4SocketAddress` defines it as `Ipv6SocketAddress` and
        /// vice-versa.
        type DifferentDomain: TestSockAddr;
        /// The local address used for tests.
        const LOCAL_ADDR: Self::AddrType;
        /// The remote address used for tests.
        const REMOTE_ADDR: Self::AddrType;
        /// An alternate remote address used for tests.
        const REMOTE_ADDR_2: Self::AddrType;
        /// An non-local address which is unreachable, used for tests.
        const UNREACHABLE_ADDR: Self::AddrType;

        /// The default subnet prefix used for tests.
        const DEFAULT_PREFIX: u8;

        /// Creates an [`fnet::SocketAddress`] with the given `addr` and `port`.
        fn create(addr: Self::AddrType, port: u16) -> fnet::SocketAddress {
            Self::new(addr, port).into_sock_addr()
        }

        /// Gets the local address and prefix configured for the test
        /// [`SockAddr`].
        fn config_addr_subnet() -> AddrSubnetEither {
            AddrSubnetEither::new(IpAddr::from(Self::LOCAL_ADDR), Self::DEFAULT_PREFIX).unwrap()
        }

        /// Gets the remote address and prefix to use for the test [`SockAddr`].
        fn config_addr_subnet_remote() -> AddrSubnetEither {
            AddrSubnetEither::new(IpAddr::from(Self::REMOTE_ADDR), Self::DEFAULT_PREFIX).unwrap()
        }
    }

    impl TestSockAddr for fnet::Ipv6SocketAddress {
        type DifferentDomain = fnet::Ipv4SocketAddress;

        const LOCAL_ADDR: Ipv6Addr =
            Ipv6Addr::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 1]);
        const REMOTE_ADDR: Ipv6Addr =
            Ipv6Addr::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 2]);
        const REMOTE_ADDR_2: Ipv6Addr =
            Ipv6Addr::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 3]);
        const UNREACHABLE_ADDR: Ipv6Addr =
            Ipv6Addr::from_bytes([0, 0, 0, 0, 0, 0, 0, 42, 0, 0, 0, 0, 192, 168, 0, 1]);
        const DEFAULT_PREFIX: u8 = 64;
    }

    impl TestSockAddr for fnet::Ipv4SocketAddress {
        type DifferentDomain = fnet::Ipv6SocketAddress;

        const LOCAL_ADDR: Ipv4Addr = Ipv4Addr::new([192, 168, 0, 1]);
        const REMOTE_ADDR: Ipv4Addr = Ipv4Addr::new([192, 168, 0, 2]);
        const REMOTE_ADDR_2: Ipv4Addr = Ipv4Addr::new([192, 168, 0, 3]);
        const UNREACHABLE_ADDR: Ipv4Addr = Ipv4Addr::new([192, 168, 42, 1]);
        const DEFAULT_PREFIX: u8 = 24;
    }
}

/// Trait expressing the conversion of error types into
/// [`fidl_fuchsia_posix::Errno`] errors for the POSIX-lite wrappers.
pub(crate) trait IntoErrno {
    /// Returns the most equivalent POSIX error code for `self`.
    fn into_errno(self) -> Errno;
}

impl IntoErrno for LocalAddressError {
    fn into_errno(self) -> Errno {
        match self {
            LocalAddressError::CannotBindToAddress
            | LocalAddressError::FailedToAllocateLocalPort => Errno::Eaddrnotavail,
            LocalAddressError::AddressMismatch => Errno::Einval,
            LocalAddressError::AddressInUse => Errno::Eaddrinuse,
            LocalAddressError::Zone(e) => e.into_errno(),
        }
    }
}

impl IntoErrno for RemoteAddressError {
    fn into_errno(self) -> Errno {
        match self {
            RemoteAddressError::NoRoute => Errno::Enetunreach,
        }
    }
}

impl IntoErrno for SocketError {
    fn into_errno(self) -> Errno {
        match self {
            SocketError::Remote(e) => e.into_errno(),
            SocketError::Local(e) => e.into_errno(),
        }
    }
}

impl IntoErrno for IpSockRouteError {
    fn into_errno(self) -> Errno {
        match self {
            IpSockRouteError::NoLocalAddrAvailable => Errno::Eaddrnotavail,
            IpSockRouteError::Unroutable(e) => e.into_errno(),
        }
    }
}

impl IntoErrno for IpSockUnroutableError {
    fn into_errno(self) -> Errno {
        match self {
            IpSockUnroutableError::LocalAddrNotAssigned => Errno::Eaddrnotavail,
            IpSockUnroutableError::NoRouteToRemoteAddr => Errno::Enetunreach,
        }
    }
}

impl IntoErrno for IpSockCreationError {
    fn into_errno(self) -> Errno {
        match self {
            IpSockCreationError::Route(e) => e.into_errno(),
        }
    }
}

impl IntoErrno for IpSockSendError {
    fn into_errno(self) -> Errno {
        match self {
            IpSockSendError::Mtu => Errno::Einval,
            IpSockSendError::Unroutable(e) => e.into_errno(),
        }
    }
}

impl IntoErrno for netstack3_core::ip::icmp::IcmpSockCreationError {
    fn into_errno(self) -> Errno {
        match self {
            netstack3_core::ip::icmp::IcmpSockCreationError::Ip(e) => e.into_errno(),
            netstack3_core::ip::icmp::IcmpSockCreationError::SockAddrConflict => Errno::Eaddrinuse,
        }
    }
}

impl IntoErrno for UdpSendError {
    fn into_errno(self) -> Errno {
        match self {
            UdpSendError::CreateSock(err) => err.into_errno(),
            UdpSendError::Send(err) => err.into_errno(),
            UdpSendError::Zone(err) => err.into_errno(),
        }
    }
}

impl IntoErrno for UdpSendListenerError {
    fn into_errno(self) -> Errno {
        match self {
            UdpSendListenerError::CreateSock(err) => err.into_errno(),
            UdpSendListenerError::LocalIpAddrMismatch => Errno::Einval,
            UdpSendListenerError::Mtu => Errno::Emsgsize,
        }
    }
}

impl IntoErrno for UdpSockCreationError {
    fn into_errno(self) -> Errno {
        match self {
            UdpSockCreationError::Ip(err) => err.into_errno(),
            UdpSockCreationError::Zone(err) => err.into_errno(),
            UdpSockCreationError::CouldNotAllocateLocalPort => Errno::Eaddrnotavail,
            UdpSockCreationError::SockAddrConflict => Errno::Eaddrinuse,
        }
    }
}

impl IntoErrno for UdpConnectListenerError {
    fn into_errno(self) -> Errno {
        match self {
            Self::Ip(err) => err.into_errno(),
            Self::Zone(err) => err.into_errno(),
        }
    }
}

impl IntoErrno for SetMulticastMembershipError {
    fn into_errno(self) -> Errno {
        match self {
            Self::AddressNotAvailable | Self::NoDeviceWithAddress | Self::NoDeviceAvailable => {
                Errno::Enodev
            }
            Self::NoMembershipChange => Errno::Eaddrinuse,
            Self::WrongDevice => Errno::Einval,
        }
    }
}

impl IntoErrno for ZonedAddressError {
    fn into_errno(self) -> Errno {
        match self {
            Self::RequiredZoneNotProvided => Errno::Einval,
            Self::DeviceZoneMismatch => Errno::Einval,
        }
    }
}

impl IntoErrno for NetstackError {
    fn into_errno(self) -> Errno {
        match self {
            NetstackError::Parse(_) => Errno::Einval,
            NetstackError::Exists => Errno::Ealready,
            NetstackError::NotFound => Errno::Efault,
            NetstackError::SendUdp(s) => s.into_errno(),
            NetstackError::Connect(c) => c.into_errno(),
            NetstackError::NoRoute => Errno::Ehostunreach,
            NetstackError::Mtu => Errno::Emsgsize,
        }
    }
}
