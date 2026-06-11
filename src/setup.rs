use std::net::UdpSocket;

/// Detect the LAN IP of this machine using the UDP socket trick.
/// Opens a UDP socket "connecting" to 8.8.8.8:80 (no packets sent),
/// then reads the local address the OS assigned — which is the LAN IP.
/// Returns `None` if the machine has no network interface.
///
/// Works correctly outside Docker and inside host-networked Docker containers
/// (the default `network_mode: host`). With bridge networking the returned IP
/// is the container bridge address, which is unreachable from the LAN — but
/// bridge networking requires manual port-mapping config anyway, so the QR
/// cannot be made to work automatically in that case regardless.
pub fn public_lan_ip_for_setup() -> Option<String> {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .ok()
        .map(|a| a.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_lan_ip_for_setup_returns_valid_address_or_none() {
        if let Some(ip) = public_lan_ip_for_setup() {
            assert!(
                ip.parse::<std::net::IpAddr>().is_ok(),
                "must return a valid IP address, got: {ip}"
            );
        }
        // None is acceptable in isolated CI environments with no network.
    }

    #[test]
    fn public_lan_ip_for_setup_is_not_loopback_when_present() {
        if let Some(ip) = public_lan_ip_for_setup() {
            assert!(
                !ip.starts_with("127."),
                "LAN IP must not be loopback, got: {ip}"
            );
            assert!(ip != "::1", "LAN IP must not be IPv6 loopback");
        }
    }
}
