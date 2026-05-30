use anyhow::{Context, Result};
use kmflow_proto::{ControlCommand, EventFrame};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

use crate::tls::{TlsIdentity, compute_fingerprint};

pub struct QuicTransport {
    endpoint: Endpoint,
    local_fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct PeerConnection {
    pub connection: Connection,
    pub fingerprint: String,
    pub remote_addr: SocketAddr,
}

impl QuicTransport {
    pub async fn bind(port: u16, identity: &TlsIdentity) -> Result<Self> {
        let server_config =
            Self::build_server_config(identity.cert_der.clone(), identity.key_der.clone_key())?;
        let client_config = Self::build_client_config()?;

        let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;

        let mut endpoint = Endpoint::server(server_config, addr).context("bind QUIC endpoint")?;
        endpoint.set_default_client_config(client_config);

        info!(%addr, fingerprint = %identity.fingerprint, "QUIC endpoint bound");

        Ok(Self {
            endpoint,
            local_fingerprint: identity.fingerprint.clone(),
        })
    }

    fn build_server_config(
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
    ) -> Result<quinn::ServerConfig> {
        let mut server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .context("server TLS config")?;
        server_crypto.alpn_protocols = vec![b"kmflow/1".to_vec()];

        let mut transport_config = quinn::TransportConfig::default();
        transport_config.max_idle_timeout(Some(
            std::time::Duration::from_secs(300).try_into().unwrap(),
        ));
        transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        transport_config.datagram_receive_buffer_size(Some(65536));

        let mut server_config =
            quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
        server_config.transport_config(Arc::new(transport_config));
        Ok(server_config)
    }

    fn build_client_config() -> Result<quinn::ClientConfig> {
        let mut client_crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        client_crypto.alpn_protocols = vec![b"kmflow/1".to_vec()];

        let client_config =
            quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto)?));
        Ok(client_config)
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<PeerConnection> {
        let connection = self
            .endpoint
            .connect(addr, "kmflow.local")
            .context("initiate QUIC connection")?
            .await
            .context("QUIC handshake")?;

        let fingerprint = self.extract_peer_fingerprint(&connection);
        info!(%addr, %fingerprint, "connected to peer");

        Ok(PeerConnection {
            connection,
            fingerprint,
            remote_addr: addr,
        })
    }

    pub async fn accept(&self) -> Result<PeerConnection> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow::anyhow!("endpoint closed"))?;

        let connection = incoming.await.context("accept QUIC connection")?;
        let addr = connection.remote_address();
        let fingerprint = self.extract_peer_fingerprint(&connection);
        info!(%addr, %fingerprint, "accepted peer connection");

        Ok(PeerConnection {
            connection,
            fingerprint,
            remote_addr: addr,
        })
    }

    pub fn local_fingerprint(&self) -> &str {
        &self.local_fingerprint
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr().context("get local addr")
    }

    fn extract_peer_fingerprint(&self, conn: &Connection) -> String {
        conn.peer_identity()
            .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
            .and_then(|certs| certs.first().cloned())
            .map(|cert| compute_fingerprint(&cert))
            .unwrap_or_else(|| "unknown".to_string())
    }

    pub async fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

impl PeerConnection {
    pub fn send_datagram(&self, frame: &EventFrame) -> Result<()> {
        let bytes = kmflow_proto::encode_frame(frame)?;
        self.connection
            .send_datagram(bytes.into())
            .context("send datagram")?;
        Ok(())
    }

    pub async fn recv_datagram(&self) -> Result<EventFrame> {
        let bytes = self
            .connection
            .read_datagram()
            .await
            .context("read datagram")?;
        kmflow_proto::decode_frame(&bytes).context("decode frame")
    }

    pub async fn open_control_stream(&self) -> Result<(SendStream, RecvStream)> {
        self.connection.open_bi().await.context("open bi stream")
    }

    pub async fn accept_control_stream(&self) -> Result<(SendStream, RecvStream)> {
        self.connection
            .accept_bi()
            .await
            .context("accept bi stream")
    }

    pub async fn send_control(&self, send: &mut SendStream, cmd: &ControlCommand) -> Result<()> {
        let bytes = kmflow_proto::encode_control(cmd)?;
        let len = (bytes.len() as u32).to_le_bytes();
        send.write_all(&len).await.context("write length")?;
        send.write_all(&bytes).await.context("write control")?;
        Ok(())
    }

    pub async fn recv_control(&self, recv: &mut RecvStream) -> Result<ControlCommand> {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await.context("read length")?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        recv.read_exact(&mut buf).await.context("read control")?;
        kmflow_proto::decode_control(&buf).context("decode control")
    }

    pub fn is_closed(&self) -> bool {
        self.connection.close_reason().is_some()
    }

    pub fn rtt(&self) -> std::time::Duration {
        self.connection.rtt()
    }
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // TOFU: we verify fingerprints at the application layer
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
