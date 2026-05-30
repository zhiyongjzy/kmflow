use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::path::{Path, PathBuf};
use tokio::fs;

pub struct TlsIdentity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
    pub fingerprint: String,
}

impl TlsIdentity {
    pub async fn load_or_generate(config_dir: &Path) -> Result<Self> {
        let cert_path = config_dir.join("cert.der");
        let key_path = config_dir.join("key.der");

        if cert_path.exists() && key_path.exists() {
            let cert_bytes = fs::read(&cert_path).await?;
            let key_bytes = fs::read(&key_path).await?;
            let cert_der = CertificateDer::from(cert_bytes);
            let fingerprint = compute_fingerprint(&cert_der);
            let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes));
            return Ok(Self {
                cert_der,
                key_der,
                fingerprint,
            });
        }

        tracing::info!("generating new TLS identity");
        let identity = Self::generate()?;
        fs::create_dir_all(config_dir).await?;
        fs::write(&cert_path, identity.cert_der.as_ref()).await?;
        if let PrivateKeyDer::Pkcs8(ref pkcs8) = identity.key_der {
            fs::write(&key_path, pkcs8.secret_pkcs8_der()).await?;
        }
        Ok(identity)
    }

    fn generate() -> Result<Self> {
        let key_pair = KeyPair::generate().context("generate keypair")?;
        let mut params =
            CertificateParams::new(vec!["kmflow.local".to_string()]).context("cert params")?;
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("kmflow".to_string()),
        );
        let cert = params.self_signed(&key_pair).context("self-sign cert")?;
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let fingerprint = compute_fingerprint(&cert_der);
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der().to_vec()));
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }

    pub fn config_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "kmflow")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".kmflow"))
    }
}

pub fn compute_fingerprint(cert_der: &CertificateDer<'_>) -> String {
    let digest = sha256(cert_der.as_ref());
    let mut out = String::with_capacity(95);
    for (i, byte) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        use std::fmt::Write;
        write!(out, "{byte:02x}").unwrap();
    }
    out
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use ring::digest;
    digest::digest(&digest::SHA256, data).as_ref().to_vec()
}

pub struct TofuVerifier {
    known_peers_path: PathBuf,
}

impl TofuVerifier {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            known_peers_path: config_dir.join("known_peers.toml"),
        }
    }

    pub async fn is_known(&self, fingerprint: &str) -> Result<bool> {
        if !self.known_peers_path.exists() {
            return Ok(false);
        }
        let content = fs::read_to_string(&self.known_peers_path).await?;
        Ok(content.contains(fingerprint))
    }

    pub async fn trust_peer(&self, identity: &kmflow_proto::PeerIdentity) -> Result<()> {
        let mut content = if self.known_peers_path.exists() {
            fs::read_to_string(&self.known_peers_path).await?
        } else {
            String::from("# KMFlow known peers\n\n")
        };

        content.push_str(&format!(
            "[[peers]]\nfingerprint = \"{}\"\nhostname = \"{}\"\nlast_seen = {}\n\n",
            identity.fingerprint, identity.hostname, identity.last_seen
        ));

        if let Some(parent) = self.known_peers_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.known_peers_path, content).await?;
        tracing::info!(hostname = %identity.hostname, "peer trusted and saved");
        Ok(())
    }
}
