//! Removes one address from a network interface.
//!
//! `defguard_wireguard_rs` can add an address but has no call to remove one
//! (`configure_interface` flushes on the kernel backend only), and its
//! `remove_interface` cannot be relied on, so the interface keeps the address
//! it was brought up with. This is the missing half of a live address change
//! (see `live_interface`); mirrored in `wayhouse::netlink_addr` rather than shared,
//! as the two binaries have no common library.

use std::io;

use defguard_wireguard_rs::net::IpAddrMask;
use netlink_packet_core::{NetlinkMessage, NetlinkPayload, NLM_F_ACK, NLM_F_REQUEST};
use netlink_packet_route::address::{AddressAttribute, AddressMessage};
use netlink_packet_route::link::{LinkAttribute, LinkMessage};
use netlink_packet_route::AddressFamily;
use netlink_packet_route::RouteNetlinkMessage;
use netlink_sys::constants::NETLINK_ROUTE;
use netlink_sys::{Socket, SocketAddr};

use crate::live_interface::{DeleteAddress, DeleteLink};

/// `EADDRNOTAVAIL`: the kernel's answer to deleting an address that is not
/// there.
const EADDRNOTAVAIL: i32 = 99;

/// `ENODEV`: the kernel's answer to deleting a link that is not there.
const ENODEV: i32 = 19;

/// A [`DeleteAddress`] for the interface `ifname`.
pub fn deleter(ifname: String) -> DeleteAddress {
    Box::new(move |address| delete_address(&ifname, address))
}

/// A [`DeleteLink`] for the interface `ifname`.
pub fn link_deleter(ifname: String) -> DeleteLink {
    Box::new(move || delete_link(&ifname))
}

/// Deletes the link `ifname`, which takes its addresses with it. A link that
/// is already gone is not an error.
pub fn delete_link(ifname: &str) -> io::Result<()> {
    let mut message = LinkMessage::default();
    message
        .attributes
        .push(LinkAttribute::IfName(ifname.to_string()));
    match request_ack(RouteNetlinkMessage::DelLink(message)) {
        Err(e) if e.raw_os_error() == Some(ENODEV) => Ok(()),
        other => other,
    }
}

/// Sends `message` and waits for the kernel's acknowledgement; its error
/// code, if any, is the returned error.
fn request_ack(message: RouteNetlinkMessage) -> io::Result<()> {
    let mut request = NetlinkMessage::from(message);
    request.header.flags = NLM_F_REQUEST | NLM_F_ACK;
    request.finalize();
    let mut buf = vec![0u8; request.buffer_len()];
    request.serialize(&mut buf);

    let socket = Socket::new(NETLINK_ROUTE)?;
    socket.connect(&SocketAddr::new(0, 0))?;
    if socket.send(&buf, 0)? != buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short netlink write",
        ));
    }
    let mut reply = [0u8; 4096];
    let n = socket.recv(&mut &mut reply[..], 0)?;
    let response = NetlinkMessage::<RouteNetlinkMessage>::deserialize(&reply[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    match response.payload {
        NetlinkPayload::Error(e) if e.code.is_none() => Ok(()),
        NetlinkPayload::Error(e) => Err(e.to_io()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected netlink reply",
        )),
    }
}

/// Deletes `address` from the interface `ifname`. An address that is already
/// gone is not an error.
pub fn delete_address(ifname: &str, address: &IpAddrMask) -> io::Result<()> {
    let index = interface_index(ifname)?;
    let mut message = AddressMessage::default();
    message.header.index = index;
    message.header.prefix_len = address.cidr;
    message.header.family = if address.address.is_ipv4() {
        AddressFamily::Inet
    } else {
        AddressFamily::Inet6
    };
    message
        .attributes
        .push(AddressAttribute::Address(address.address));
    if address.address.is_ipv4() {
        message
            .attributes
            .push(AddressAttribute::Local(address.address));
    }

    match request_ack(RouteNetlinkMessage::DelAddress(message)) {
        Err(e)
            if e.raw_os_error() == Some(EADDRNOTAVAIL) || e.kind() == io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        other => other,
    }
}

/// The kernel's index of `ifname`, asked of the kernel through netlink so it
/// is the caller's own network namespace that answers (sysfs would show the
/// namespace it was mounted in), and for any link kind (the lookup in
/// `defguard_wireguard_rs` only matches WireGuard-kind links, not the tun
/// device boringtun creates).
fn interface_index(ifname: &str) -> io::Result<u32> {
    let mut message = LinkMessage::default();
    message
        .attributes
        .push(LinkAttribute::IfName(ifname.to_string()));
    let mut request = NetlinkMessage::from(RouteNetlinkMessage::GetLink(message));
    request.header.flags = NLM_F_REQUEST;
    request.finalize();
    let mut buf = vec![0u8; request.buffer_len()];
    request.serialize(&mut buf);

    let socket = Socket::new(NETLINK_ROUTE)?;
    socket.connect(&SocketAddr::new(0, 0))?;
    if socket.send(&buf, 0)? != buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short netlink write",
        ));
    }
    let mut reply = [0u8; 8192];
    let n = socket.recv(&mut &mut reply[..], 0)?;
    let response = NetlinkMessage::<RouteNetlinkMessage>::deserialize(&reply[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    match response.payload {
        NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewLink(link)) => Ok(link.header.index),
        NetlinkPayload::Error(e) if e.code.is_some() => Err(e.to_io()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected netlink reply",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs `CAP_NET_ADMIN` and `ip`; without them it does nothing (the
    /// tunnel e2e covers the call end to end in CI). Not `#[ignore]`d, since
    /// the CI jobs that run ignored tests have no such privilege.
    #[test]
    fn deletes_an_address_and_tolerates_one_that_is_gone() {
        let ip = |args: &[&str]| {
            let out = std::process::Command::new("ip")
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let privileged = std::process::Command::new("ip")
            .args(["addr", "add", "10.99.77.1/24", "dev", "lo"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !privileged {
            eprintln!("skipped: needs CAP_NET_ADMIN and ip");
            return;
        }
        ip(&["addr", "del", "10.99.77.1/24", "dev", "lo"]);
        let address: IpAddrMask = "10.99.77.1/24".parse().unwrap();
        ip(&["addr", "add", "10.99.77.1/24", "dev", "lo"]);
        assert!(ip(&["addr", "show", "dev", "lo"]).contains("10.99.77.1/24"));
        delete_address("lo", &address).unwrap();
        assert!(!ip(&["addr", "show", "dev", "lo"]).contains("10.99.77.1/24"));
        delete_address("lo", &address).unwrap();
    }

    /// Same privilege rule as above.
    #[test]
    fn deletes_a_link_and_tolerates_one_that_is_gone() {
        let ip = |args: &[&str]| {
            std::process::Command::new("ip")
                .args(args)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !ip(&["link", "add", "wayhouse-del0", "type", "dummy"]) {
            eprintln!("skipped: needs CAP_NET_ADMIN and ip");
            return;
        }
        assert!(ip(&["link", "show", "wayhouse-del0"]));
        delete_link("wayhouse-del0").unwrap();
        assert!(!ip(&["link", "show", "wayhouse-del0"]));
        delete_link("wayhouse-del0").unwrap();
    }
}
