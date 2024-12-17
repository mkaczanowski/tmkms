use abscissa_core::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::error::Error;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use ureq::{
    config::AutoHeaderValue,
    http::Response,
    tls::{Certificate, RootCerts, TlsConfig},
    Agent, Body,
};

use crate::config::provider::hashicorp::VaultEndpointConfig;
use crate::keyring::ed25519;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Vault message envelop
#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root<T> {
    #[serde(rename = "request_id")]
    pub request_id: String,
    #[serde(rename = "lease_id")]
    pub lease_id: String,
    pub renewable: bool,
    #[serde(rename = "lease_duration")]
    pub lease_duration: i64,
    pub data: Option<T>,
    #[serde(rename = "wrap_info")]
    pub wrap_info: Value,
    pub warnings: Value,
    pub auth: Value,
}

/// Sign Request Struct
#[derive(Debug, Serialize)]
pub(crate) struct SignRequest {
    pub input: String, // Base64 encoded
}

/// Sign Response Struct
#[derive(Debug, Deserialize)]
pub(crate) struct SignResponse {
    pub signature: String, // Base64 encoded
}

#[derive(Debug, Serialize)]
pub(crate) struct ImportRequest {
    pub r#type: String,
    pub ciphertext: String,
    pub hash_function: String,
    pub exportable: bool,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum ExportKeyType {
    Encryption,
    Signing,
    Hmac,
}
impl std::fmt::Display for ExportKeyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportKeyType::Encryption => write!(f, "encryption-key"),
            ExportKeyType::Signing => write!(f, "signing-key"),
            ExportKeyType::Hmac => write!(f, "hmac-key"),
        }
    }
}
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum CreateKeyType {
    /// AES-128 wrapped with GCM using a 96-bit nonce size AEAD (symmetric, supports derivation and convergent encryption)
    Aes128Gcm96,
    /// AES-256 wrapped with GCM using a 96-bit nonce size AEAD (symmetric, supports derivation and convergent encryption, default)
    Aes256Gcm96,
    /// ChaCha20-Poly1305 AEAD (symmetric, supports derivation and convergent encryption)
    Chacha20Poly1305,
    /// ED25519 (asymmetric, supports derivation). When using derivation, a sign operation with the same context will derive the same key and signature; this is a signing analogue to convergent_encryption.
    Ed25519,
    /// ECDSA using the P-256 elliptic curve (asymmetric)
    EcdsaP256,
    /// ECDSA using the P-384 elliptic curve (asymmetric)
    EcdsaP384,
    /// ECDSA using the P-521 elliptic curve (asymmetric)
    EcdsaP521,
    /// RSA with bit size of 2048 (asymmetric)
    Rsa2048,
    /// RSA with bit size of 3072 (asymmetric)
    Rsa3072,
    /// RSA with bit size of 4096 (asymmetric)
    Rsa4096,
}

impl std::fmt::Display for CreateKeyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateKeyType::Aes128Gcm96 => write!(f, "aes128-gcm96"),
            CreateKeyType::Aes256Gcm96 => write!(f, "aes256-gcm96"),
            CreateKeyType::Chacha20Poly1305 => write!(f, "chacha20-poly1305"),
            CreateKeyType::Ed25519 => write!(f, "ed25519"),
            CreateKeyType::EcdsaP256 => write!(f, "ecdsa-p256"),
            CreateKeyType::EcdsaP384 => write!(f, "ecdsa-p384"),
            CreateKeyType::EcdsaP521 => write!(f, "ecdsa-p521"),
            CreateKeyType::Rsa2048 => write!(f, "rsa-2048"),
            CreateKeyType::Rsa3072 => write!(f, "rsa-3072"),
            CreateKeyType::Rsa4096 => write!(f, "rsa-4096"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct VaultClient {
    agent: Agent,
    api_endpoint: String,
    endpoints: VaultEndpointConfig,
    token: String,
    exit_on_error: Vec<u16>,
}

pub const VAULT_TOKEN: &str = "X-Vault-Token";
pub const CONSENUS_KEY_TYPE: &str = "ed25519";

impl VaultClient {
    pub fn new(
        api_endpoint: &str,
        token: &str,
        endpoints: Option<VaultEndpointConfig>,
        ca_cert: Option<String>,
        skip_verify: Option<bool>,
        exit_on_error: Option<Vec<u16>>,
    ) -> Self {
        // this call performs token self lookup, to fail fast
        // let mut client = Client::new(host, token)?;

        // default conect timeout is 30s, this should be ok, since we block
        let mut agent_builder = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .user_agent(AutoHeaderValue::Provided(Arc::new(format!(
                "{}/{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))));

        if ca_cert.is_some() || skip_verify.is_some() {
            if skip_verify.is_some_and(|x| x) {
                let tls_config = TlsConfig::builder().disable_verification(true).build();

                agent_builder = agent_builder.tls_config(tls_config);
            } else if let Some(ca_cert) = ca_cert {
                let cert = read_cert(&ca_cert);
                let certs: Vec<Certificate<'static>> = vec![Certificate::from_der(cert)];
                let root_certs = RootCerts::new_with_certs(certs.as_slice());
                let tls_config = TlsConfig::builder().root_certs(root_certs).build();

                agent_builder = agent_builder.tls_config(tls_config);
            }
        }

        let agent: Agent = agent_builder.build().new_agent();

        VaultClient {
            api_endpoint: api_endpoint.into(),
            endpoints: endpoints.unwrap_or_default(),
            agent,
            token: token.into(),
            exit_on_error: exit_on_error.unwrap_or_default(),
        }
    }

    pub fn public_key(
        &self,
        key_name: &str,
    ) -> Result<[u8; ed25519::VerifyingKey::BYTE_SIZE], Error> {
        /// Response struct
        #[derive(Debug, Deserialize)]
        struct PublicKeyResponse {
            keys: BTreeMap<usize, HashMap<String, String>>,
        }

        let uri = format!("{}{}/{}", self.api_endpoint, self.endpoints.keys, key_name);

        // https://developer.hashicorp.com/vault/api-docs/secret/transit#read-key
        let res = self.agent.get(&uri).header(VAULT_TOKEN, &self.token).call();

        let response = self.check_response_status_code(&uri, res)?;
        let data = if let Some(data) = response
            .into_body()
            .read_json::<Root<PublicKeyResponse>>()?
            .data
        {
            data
        } else {
            return Err(Error::InvalidPubKey(
                "Public key: Vault response unavailable".into(),
            ));
        };

        // latest key version
        let key_data = data.keys.iter().last();

        let pubk = if let Some((version, map)) = key_data {
            debug!("public key version:{}", version);
            if let Some(pubk) = map.get("public_key") {
                if let Some(key_type) = map.get("name") {
                    if CONSENUS_KEY_TYPE != key_type {
                        return Err(Error::InvalidPubKey(format!(
                            "Public key \"{}\": expected key type:{}, received:{}",
                            key_name, CONSENUS_KEY_TYPE, key_type
                        )));
                    }
                } else {
                    return Err(Error::InvalidPubKey(format!(
                        "Public key \"{}\": expected key type:{}, unable to determine type",
                        key_name, CONSENUS_KEY_TYPE
                    )));
                }
                pubk
            } else {
                return Err(Error::InvalidPubKey(
                    "Public key: unable to retrieve - \"public_key\" key is not found!".into(),
                ));
            }
        } else {
            return Err(Error::InvalidPubKey(
                "Public key: unable to retrieve last version - not available!".into(),
            ));
        };

        debug!("Public key: fetched {}={}...", key_name, pubk);

        let pubk = base64::decode(pubk)?;

        debug!(
            "Public key: base64 decoded {}, size: {}",
            key_name,
            pubk.len()
        );

        let mut array = [0u8; ed25519::VerifyingKey::BYTE_SIZE];
        array.copy_from_slice(&pubk[..ed25519::VerifyingKey::BYTE_SIZE]);

        Ok(array)
    }

    pub fn handshake(&self) -> Result<(), Error> {
        let uri = format!("{}{}", self.api_endpoint, self.endpoints.handshake,);

        let res = self.agent.get(&uri).header(VAULT_TOKEN, &self.token).call();

        self.check_response_status_code(&uri, res)?;
        Ok(())
    }

    // vault write transit/sign/cosmoshub-sign-key plaintext=$(base64 <<< "some-data")
    // "https://127.0.0.1:8200/v1/transit/sign/cosmoshub-sign-key"
    /// Sign message
    pub fn sign(
        &self,
        key_name: &str,
        message: &[u8],
    ) -> Result<[u8; ed25519::Signature::BYTE_SIZE], Error> {
        debug!("signing request: received");
        if message.is_empty() {
            return Err(Error::InvalidEmptyMessage);
        }

        let body = SignRequest {
            input: base64::encode(message),
        };

        debug!("signing request: base64 encoded and about to submit for signing...");

        let uri = format!("{}{}/{}", self.api_endpoint, self.endpoints.sign, key_name);

        let res = self
            .agent
            .post(&uri)
            .header(VAULT_TOKEN, &self.token)
            .send_json(body);

        let response = self.check_response_status_code(&uri, res)?;
        let data = if let Some(data) = response.into_body().read_json::<Root<SignResponse>>()?.data
        {
            data
        } else {
            return Err(Error::NoSignature);
        };

        let parts = data.signature.split(':').collect::<Vec<&str>>();
        if parts.len() != 3 {
            return Err(Error::InvalidSignature(format!(
                "expected 3 parts, received:{} full:{}",
                parts.len(),
                data.signature
            )));
        }

        // signature: "vault:v1:/bcnnk4p8Uvidrs1/IX9s66UCOmmfdJudcV1/yek9a2deMiNGsVRSjirz6u+ti2wqUZfG6UukaoSHIDSSRV5Cw=="
        let base64_signature = if let Some(sign) = parts.last() {
            sign.to_owned()
        } else {
            // this should never happen
            return Err(Error::InvalidSignature("last part is not available".into()));
        };

        let signature = base64::decode(base64_signature)?;
        if signature.len() != 64 {
            return Err(Error::InvalidSignature(format!(
                "invalid signature length! 64 == {}",
                signature.len()
            )));
        }

        let mut array = [0u8; ed25519::Signature::BYTE_SIZE];
        array.copy_from_slice(&signature[..ed25519::Signature::BYTE_SIZE]);
        Ok(array)
    }

    pub fn wrapping_key_pem(&self) -> Result<String, Error> {
        #[derive(Debug, Deserialize)]
        struct PublicKeyResponse {
            public_key: String,
        }

        let uri = format!("{}{}", self.api_endpoint, self.endpoints.wrapping_key);

        let res = self.agent.get(&uri).header(VAULT_TOKEN, &self.token).call();

        let response = self.check_response_status_code(&uri, res)?;
        let data = if let Some(data) = response
            .into_body()
            .read_json::<Root<PublicKeyResponse>>()?
            .data
        {
            data
        } else {
            return Err(Error::InvalidPubKey("Error getting wrapping key!".into()));
        };

        Ok(data.public_key.trim().to_owned())
    }

    pub fn import_key(
        &self,
        key_name: &str,
        key_type: CreateKeyType,
        ciphertext: &str,
        exportable: bool,
    ) -> Result<(), Error> {
        let body = ImportRequest {
            r#type: key_type.to_string(),
            ciphertext: ciphertext.into(),
            hash_function: "SHA256".into(),
            exportable,
        };

        let uri = format!(
            "{}{}/{}/import",
            self.api_endpoint, self.endpoints.keys, key_name
        );

        let res = self
            .agent
            .post(&uri)
            .header(VAULT_TOKEN, &self.token)
            .send_json(body);

        self.check_response_status_code(&uri, res)?;

        Ok(())
    }

    fn check_response_status_code(
        &self,
        uri: &str,
        response: Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, Error> {
        match response {
            Ok(response) => Ok(response),
            Err(ureq::Error::StatusCode(code)) => {
                if self.exit_on_error.contains(&code) {
                    panic!(
                        "{}",
                        Error::ProhibitedResponseCode(code.to_string(), uri.into())
                    );
                } else {
                    Err(ureq::Error::StatusCode(code))?
                }
            }
            Err(err) => Err(err.into()),
        }
    }
}

fn read_cert(path: &str) -> &'static [u8] {
    // a static cache to store file contents per file path
    static CERT_CACHE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();

    // initialize the cache
    let cache = CERT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    // access the cache and ensure the content for the given path
    let mut map = cache.lock().unwrap();
    if !map.contains_key(path) {
        let content = fs::read(path).expect("Failed to read CA certificate");

        // NOTE: `Certificate::from_pem` from ureq crate does not parse the PEM file correctly
        // in version 3.0.0-rc3, so we use `CertificateDer::from_pem_slice` from rustls crate
        let cert_der: CertificateDer<'static> = CertificateDer::from_pem_slice(&content).unwrap();
        map.insert(path.to_string(), cert_der.to_vec());
    }

    // leak the cert to get a 'static reference
    let content = map.get(path).unwrap();
    let static_content: &'static [u8] = Box::leak(content.clone().into_boxed_slice());
    static_content
}
