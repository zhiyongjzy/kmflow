#[cfg(test)]
mod tests {
    use crate::quic::QuicTransport;
    use crate::tls::TlsIdentity;
    use kmflow_proto::{
        ControlCommand, Edge, EventFrame, InputEvent, PROTOCOL_VERSION, ScreenInfo, ScreenPosition,
    };

    #[tokio::test]
    async fn quic_datagram_roundtrip() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        let identity_a = TlsIdentity::load_or_generate(dir_a.path()).await.unwrap();
        let identity_b = TlsIdentity::load_or_generate(dir_b.path()).await.unwrap();

        let transport_a = QuicTransport::bind(0, &identity_a).await.unwrap();
        let transport_b = QuicTransport::bind(0, &identity_b).await.unwrap();

        let addr_a = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            transport_a.local_addr().unwrap().port(),
        );

        // Spawn accept on A
        let accept_handle = tokio::spawn(async move { transport_a.accept().await.unwrap() });

        // B connects to A
        let peer_at_b = transport_b.connect(addr_a).await.unwrap();
        let peer_at_a = accept_handle.await.unwrap();

        // Send datagram from B to A
        let frame = EventFrame {
            seq: 1,
            timestamp_us: 12345,
            events: vec![InputEvent::MouseMove { dx: 5.0, dy: -3.0 }],
        };
        peer_at_b.send_datagram(&frame).unwrap();

        // Receive at A
        let received = peer_at_a.recv_datagram().await.unwrap();
        assert_eq!(received.seq, 1);
        assert_eq!(received.events.len(), 1);
        match received.events[0] {
            InputEvent::MouseMove { dx, dy } => {
                assert_eq!(dx, 5.0);
                assert_eq!(dy, -3.0);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[tokio::test]
    async fn quic_control_stream_roundtrip() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        let identity_a = TlsIdentity::load_or_generate(dir_a.path()).await.unwrap();
        let identity_b = TlsIdentity::load_or_generate(dir_b.path()).await.unwrap();

        let transport_a = QuicTransport::bind(0, &identity_a).await.unwrap();
        let transport_b = QuicTransport::bind(0, &identity_b).await.unwrap();

        let addr_a = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            transport_a.local_addr().unwrap().port(),
        );

        let accept_handle = tokio::spawn(async move { transport_a.accept().await.unwrap() });

        let peer_at_b = transport_b.connect(addr_a).await.unwrap();
        let peer_at_a = accept_handle.await.unwrap();

        // B opens stream and sends handshake
        let (mut send_b, _recv_b) = peer_at_b.open_control_stream().await.unwrap();
        let cmd = ControlCommand::Handshake {
            protocol_version: PROTOCOL_VERSION,
            hostname: "test-b".to_string(),
            screen_info: ScreenInfo {
                width: 2560,
                height: 1440,
                scale_factor: 2.0,
                position: ScreenPosition {
                    edge: Edge::Left,
                    monitor_id: 0,
                },
            },
            edge: Some(Edge::Right),
        };
        peer_at_b.send_control(&mut send_b, &cmd).await.unwrap();

        // A receives
        let (_send_a, mut recv_a) = peer_at_a.accept_control_stream().await.unwrap();
        let received = peer_at_a.recv_control(&mut recv_a).await.unwrap();
        match received {
            ControlCommand::Handshake {
                protocol_version,
                hostname,
                screen_info,
                ..
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(hostname, "test-b");
                assert_eq!(screen_info.width, 2560);
                assert_eq!(screen_info.scale_factor, 2.0);
            }
            _ => panic!("wrong command"),
        }
    }
}
