//! Support for signing mft, crl, certificates, roas..
//! Common objects for TAs and CAs
use std::{ops::Deref, path::Path};
use std::sync::{Arc, RwLock};
use std::convert::TryFrom;
use std::ops::DerefMut;

use bytes::Bytes;

use rpki::cert::{Cert, KeyUsage, Overclaim, TbsCert};
use rpki::crl::{Crl, CrlEntry, TbsCertList};
use rpki::crypto::{DigestAlgorithm, KeyIdentifier, PublicKey, PublicKeyFormat, Signature, SignatureAlgorithm, Signer};
use rpki::csr::Csr;
use rpki::manifest::{FileAndHash, Manifest, ManifestContent};
use rpki::roa::{Roa, RoaBuilder};
use rpki::sigobj::SignedObjectBuilder;
use rpki::x509::{Name, Serial, Time, Validity};
use rpki::{rta, uri};

use crate::{commons::api::{IssuedCert, RcvdCert, ReplacedObject, RepoInfo, RequestResourceLimit, ResourceSet}, daemon::config::{Config, SignerType}};
#[cfg(feature = "hsm")]
use crate::commons::crypto::signing::{Pkcs11Signer, KmipSigner};
use crate::commons::crypto::{self, CryptoResult};
use crate::commons::error::Error;
use crate::commons::util::AllowedUri;
use crate::commons::KrillResult;
use crate::daemon::ca::CertifiedKey;

use super::{OpenSslSigner, SignerError};

//------------ KeyMeta -------------------------------------------------------

// This type is used as an abstraction over Sled so that the signer implementations don't become tied to specifics of
// Sled.

#[derive(Debug, Clone)]
pub struct KeyMap {
    // Sled is "It is fully thread-safe, and all operations are atomic".
    db: sled::Db,
}

impl KeyMap {
    pub fn persistent(data_dir: &Path) -> KrillResult<Self> {
        let db_path = data_dir.join("keys/map.db");
        debug!("Opening key map database at '{}'", &db_path.display());
        let db = sled::Config::new()
            .mode(sled::Mode::HighThroughput)
            .path(&db_path)
            .open()
            .map_err(|err| Error::SignerError(
                format!("Failed to open key map database '{}': {}", db_path.display(), err)))?;

        Ok(Self { db })
    }

    pub fn in_memory() -> KrillResult<Self> {
        // useful for testing
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(|err| Error::SignerError(
                format!("Failed to open in-memory key map database: {}", err)))?;

        Ok(Self { db })
    }

    pub fn add_key(&self, key_id: KeyIdentifier, key_handle: &[u8]) {
        debug!("Add key {} => {:?}", &key_id, key_handle);

        fn add_and_flush(db: &sled::Db, key_id: KeyIdentifier, key_handle: &[u8]) -> Result<(), SignerError> {
            db
                .compare_and_swap(key_id, None as Option<KeyIdentifier>, Some(key_handle.to_vec()))
                .map_err(|err| SignerError::KeyMapError(format!("Insert failed: {}", err)))?
                .map_err(|err| SignerError::KeyMapError(format!("Insert failed: Key already exists! (underlying error: {})", err)))?;
            db
                .flush()
                .map_err(|err| SignerError::KeyMapError(format!("Flush failed: {}", err)))?;

            Ok(())
        }

        if let Err(err) = add_and_flush(&self.db, key_id.clone(), key_handle) {
            // Abort Krill because if we cannot write the key mapping record completely to disk we will never be
            // able to sign with this key or show in the history which signer this key was used with.
            panic!("Failed to add key {} to key map: {}", key_id, err);
        }
    }

    pub fn get_key(&self, key_id: &KeyIdentifier) -> Result<Vec<u8>, SignerError> {
        debug!("Get key {}", &key_id);
        let possible_value = self.db.get(key_id)
            .map_err(|err| SignerError::KeyMapError(format!("Failed to access key meta: {}", err)))?
            .and_then(|v| Some(v.deref().to_vec()));

        possible_value.ok_or(SignerError::KeyNotFound)
    }
}

impl Drop for KeyMap {
    fn drop(&mut self) {
        debug!("Closing key map database");
    }
}

//------------ Signer --------------------------------------------------------

#[derive(Debug)]
pub enum SignerImpl {
    OpenSsl(OpenSslSigner),
    #[cfg(feature = "hsm")]
    Pkcs11(Pkcs11Signer),
    #[cfg(feature = "hsm")]
    Kmip(KmipSigner)
}

// This is an enum in preparation of other supported signer types
#[derive(Clone, Debug)]
pub struct KrillSigner {
    // use a blocking lock to avoid having to be async, for signing operations
    // this should be fine.
    signer: Arc<RwLock<SignerImpl>>,
    key_lookup: Arc<KeyMap>,
}

impl KrillSigner {
    pub fn build(config: Arc<Config>) -> KrillResult<Self> {
        let key_lookup = Arc::new(KeyMap::persistent(&config.data_dir)?);

        let signer = match config.signer_type {
            SignerType::OpenSsl => SignerImpl::OpenSsl(OpenSslSigner::build(&config.data_dir, key_lookup.clone())?),
            #[cfg(feature = "hsm")]
            SignerType::Pkcs11 => SignerImpl::Pkcs11(Pkcs11Signer::build(config.clone(), key_lookup.clone())?),
            #[cfg(feature = "hsm")]
            SignerType::Kmip => SignerImpl::Kmip(KmipSigner::build(config.clone(), key_lookup.clone())?),
        };

        let signer = Arc::new(RwLock::new(signer));

        Ok(KrillSigner { signer, key_lookup })
    }

    pub fn test(data_dir: &Path) -> KrillResult<Self> {
        let key_lookup = Arc::new(KeyMap::in_memory()?);

        let signer = SignerImpl::OpenSsl(OpenSslSigner::build(&data_dir, key_lookup.clone())?);

        let signer = Arc::new(RwLock::new(signer));

        Ok(KrillSigner { signer, key_lookup })
    }
}

impl KrillSigner {
    pub fn create_key(&self) -> CryptoResult<KeyIdentifier> {
        match self.signer.write().unwrap().deref_mut() {
            SignerImpl::OpenSsl(signer) => signer.create_key(PublicKeyFormat::Rsa),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => signer.create_key(PublicKeyFormat::Rsa),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => signer.create_key(PublicKeyFormat::Rsa),
        }
        .map_err(crypto::Error::signer)
    }

    pub fn destroy_key(&self, key_id: &KeyIdentifier) -> CryptoResult<()> {
        match self.signer.write().unwrap().deref_mut() {
            SignerImpl::OpenSsl(signer) => signer.destroy_key(key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => signer.destroy_key(key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => signer.destroy_key(key_id),
        }
        .map_err(crypto::Error::signer)
    }

    pub fn get_key_info(&self, key_id: &KeyIdentifier) -> CryptoResult<PublicKey> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => signer.get_key_info(key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => signer.get_key_info(key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => signer.get_key_info(key_id),
        }
        .map_err(crypto::Error::key_error)
    }

    pub fn random_serial(&self) -> CryptoResult<Serial> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => Serial::random(signer),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => Serial::random(signer),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => Serial::random(signer),
        }
        .map_err(crypto::Error::signer)
    }

    pub fn sign<D: AsRef<[u8]> + ?Sized>(&self, key_id: &KeyIdentifier, data: &D) -> CryptoResult<Signature> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => signer.sign(key_id, SignatureAlgorithm::default(), data),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => signer.sign(key_id, SignatureAlgorithm::default(), data),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => signer.sign(key_id, SignatureAlgorithm::default(), data),
        }
        .map_err(crypto::Error::signing)
    }

    pub fn sign_one_off<D: AsRef<[u8]> + ?Sized>(&self, data: &D) -> CryptoResult<(Signature, PublicKey)> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => signer.sign_one_off(SignatureAlgorithm::default(), data),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => signer.sign_one_off(SignatureAlgorithm::default(), data),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => signer.sign_one_off(SignatureAlgorithm::default(), data),
        }
        .map_err(crypto::Error::signer)
    }

    pub fn sign_csr(&self, base_repo: &RepoInfo, name_space: &str, key: &KeyIdentifier) -> CryptoResult<Csr> {
        let signer = self.signer.read().unwrap();
        let pub_key = match signer.deref() {
            SignerImpl::OpenSsl(signer) => signer.get_key_info(key),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => signer.get_key_info(key),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => signer.get_key_info(key),
        }
        .map_err(crypto::Error::key_error)?;
        let enc = match signer.deref() {
            SignerImpl::OpenSsl(signer) => {
                Csr::construct(
                    signer,
                    key,
                    &base_repo.ca_repository(name_space).join(&[]), // force trailing slash
                    &base_repo.rpki_manifest(name_space, &pub_key.key_identifier()),
                    Some(&base_repo.rpki_notify()),
                )
            }
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => {
                Csr::construct(
                    signer,
                    key,
                    &base_repo.ca_repository(name_space).join(&[]), // force trailing slash
                    &base_repo.rpki_manifest(name_space, &pub_key.key_identifier()),
                    Some(&base_repo.rpki_notify()),
                )
            }
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => {
                Csr::construct(
                    signer,
                    key,
                    &base_repo.ca_repository(name_space).join(&[]), // force trailing slash
                    &base_repo.rpki_manifest(name_space, &pub_key.key_identifier()),
                    Some(&base_repo.rpki_notify()),
                )
            }
        }
        .map_err(crypto::Error::signing)?;
        Ok(Csr::decode(enc.as_slice())?)
    }

    pub fn sign_cert(&self, tbs: TbsCert, key_id: &KeyIdentifier) -> CryptoResult<Cert> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => tbs.into_cert(signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => tbs.into_cert(signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => tbs.into_cert(signer, key_id),
        }
        .map_err(crypto::Error::signing)
    }

    pub fn sign_crl(&self, tbs: TbsCertList<Vec<CrlEntry>>, key_id: &KeyIdentifier) -> CryptoResult<Crl> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => tbs.into_crl(signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => tbs.into_crl(signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => tbs.into_crl(signer, key_id),
        }
        .map_err(crypto::Error::signing)
    }

    pub fn sign_manifest(
        &self,
        content: ManifestContent,
        builder: SignedObjectBuilder,
        key_id: &KeyIdentifier,
    ) -> CryptoResult<Manifest> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => content.into_manifest(builder, signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => content.into_manifest(builder, signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => content.into_manifest(builder, signer, key_id),
        }
        .map_err(crypto::Error::signing)
    }

    pub fn sign_roa(
        &self,
        roa_builder: RoaBuilder,
        object_builder: SignedObjectBuilder,
        key_id: &KeyIdentifier,
    ) -> CryptoResult<Roa> {
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => roa_builder.finalize(object_builder, signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => roa_builder.finalize(object_builder, signer, key_id),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => roa_builder.finalize(object_builder, signer, key_id),
        }
        .map_err(crypto::Error::signing)
    }

    pub fn sign_rta(&self, rta_builder: &mut rta::RtaBuilder, ee: Cert) -> CryptoResult<()> {
        let key = ee.subject_key_identifier();
        rta_builder.push_cert(ee);
        match self.signer.read().unwrap().deref() {
            SignerImpl::OpenSsl(signer) => rta_builder.sign(signer, &key, None, None),
            #[cfg(feature = "hsm")]
            SignerImpl::Pkcs11(signer) => rta_builder.sign(signer, &key, None, None),
            #[cfg(feature = "hsm")]
            SignerImpl::Kmip(signer) => rta_builder.sign(signer, &key, None, None),
        }
        .map_err(crypto::Error::signing)
    }
}

// //------------ Signer --------------------------------------------------------
//
// pub trait Signer: crypto::Signer<KeyId = KeyIdentifier> + Clone + Sized + Sync + Send + 'static {}
// impl<T: crypto::Signer<KeyId = KeyIdentifier> + Clone + Sized + Sync + Send + 'static> Signer for T {}

//------------ CsrInfo -------------------------------------------------------

pub type CaRepository = uri::Rsync;
pub type RpkiManifest = uri::Rsync;
pub type RpkiNotify = uri::Https;

pub struct CsrInfo {
    ca_repository: CaRepository,
    rpki_manifest: RpkiManifest,
    rpki_notify: Option<RpkiNotify>,
    key: PublicKey,
}

impl CsrInfo {
    pub fn new(
        ca_repository: CaRepository,
        rpki_manifest: RpkiManifest,
        rpki_notify: Option<RpkiNotify>,
        key: PublicKey,
    ) -> Self {
        CsrInfo {
            ca_repository,
            rpki_manifest,
            rpki_notify,
            key,
        }
    }

    pub fn global_uris(&self) -> bool {
        self.ca_repository.seems_global_uri()
            && self.rpki_manifest.seems_global_uri()
            && self
                .rpki_notify
                .as_ref()
                .map(|uri| uri.seems_global_uri())
                .unwrap_or_else(|| true)
    }

    pub fn unpack(self) -> (CaRepository, RpkiManifest, Option<RpkiNotify>, PublicKey) {
        (self.ca_repository, self.rpki_manifest, self.rpki_notify, self.key)
    }

    pub fn key_id(&self) -> KeyIdentifier {
        self.key.key_identifier()
    }
}

impl TryFrom<&Csr> for CsrInfo {
    type Error = Error;

    fn try_from(csr: &Csr) -> KrillResult<CsrInfo> {
        csr.validate().map_err(|_| Error::invalid_csr("invalid signature"))?;
        let ca_repository = csr
            .ca_repository()
            .cloned()
            .ok_or_else(|| Error::invalid_csr("missing ca repository"))?;
        let rpki_manifest = csr
            .rpki_manifest()
            .cloned()
            .ok_or_else(|| Error::invalid_csr("missing rpki manifest"))?;
        let rpki_notify = csr.rpki_notify().cloned();
        let key = csr.public_key().clone();
        Ok(CsrInfo {
            ca_repository,
            rpki_manifest,
            rpki_notify,
            key,
        })
    }
}

impl From<&Cert> for CsrInfo {
    fn from(issued: &Cert) -> Self {
        let ca_repository = issued.ca_repository().cloned().unwrap();
        let rpki_manifest = issued.rpki_manifest().cloned().unwrap();
        let rpki_notify = issued.rpki_notify().cloned();
        let key = issued.subject_public_key_info().clone();
        CsrInfo {
            ca_repository,
            rpki_manifest,
            rpki_notify,
            key,
        }
    }
}

//------------ CaSignSupport -------------------------------------------------

/// Support signing by CAs
pub struct SignSupport;

impl SignSupport {
    /// Create an IssuedCert
    pub fn make_issued_cert(
        csr: CsrInfo,
        resources: &ResourceSet,
        limit: RequestResourceLimit,
        replaces: Option<ReplacedObject>,
        signing_key: &CertifiedKey,
        weeks: i64,
        signer: &KrillSigner,
    ) -> KrillResult<IssuedCert> {
        let signing_cert = signing_key.incoming_cert();
        let resources = resources.apply_limit(&limit)?;
        if !signing_cert.resources().contains(&resources) {
            return Err(Error::MissingResources);
        }

        let validity = Self::sign_validity_weeks(weeks);
        let request = CertRequest::Ca(csr, validity);

        let tbs = Self::make_tbs_cert(&resources, signing_cert, request, signer)?;
        let cert = signer.sign_cert(tbs, &signing_key.key_id())?;

        let cert_uri = signing_cert.uri_for_object(&cert);

        Ok(IssuedCert::new(cert_uri, limit, resources, cert, replaces))
    }

    /// Create an EE certificate for use in ResourceTaggedAttestations.
    /// Note that for RPKI signed objects such as ROAs and Manifests, the
    /// EE certificate is created by the rpki.rs library instead.
    pub fn make_rta_ee_cert(
        resources: &ResourceSet,
        signing_key: &CertifiedKey,
        validity: Validity,
        pub_key: PublicKey,
        signer: &KrillSigner,
    ) -> KrillResult<Cert> {
        let signing_cert = signing_key.incoming_cert();
        let request = CertRequest::Ee(pub_key, validity);
        let tbs = Self::make_tbs_cert(resources, signing_cert, request, signer)?;

        let cert = signer.sign_cert(tbs, &signing_key.key_id())?;
        Ok(cert)
    }

    fn make_tbs_cert(
        resources: &ResourceSet,
        signing_cert: &RcvdCert,
        request: CertRequest,
        signer: &KrillSigner,
    ) -> KrillResult<TbsCert> {
        let serial = signer.random_serial()?;
        let issuer = signing_cert.cert().subject().clone();

        let validity = match &request {
            CertRequest::Ca(_, validity) => *validity,
            CertRequest::Ee(_, validity) => *validity,
        };

        let pub_key = match &request {
            CertRequest::Ca(info, _) => info.key.clone(),
            CertRequest::Ee(key, _) => key.clone(),
        };

        let subject = Some(Name::from_pub_key(&pub_key));

        let key_usage = match &request {
            CertRequest::Ca(_, _) => KeyUsage::Ca,
            CertRequest::Ee(_, _) => KeyUsage::Ee,
        };

        let overclaim = Overclaim::Refuse;

        let mut cert = TbsCert::new(serial, issuer, validity, subject, pub_key, key_usage, overclaim);

        let asns = resources.to_as_resources();
        if asns.is_inherited() || !asns.to_blocks().unwrap().is_empty() {
            cert.set_as_resources(asns);
        }

        let ipv4 = resources.to_ip_resources_v4();
        if ipv4.is_inherited() || !ipv4.to_blocks().unwrap().is_empty() {
            cert.set_v4_resources(ipv4);
        }

        let ipv6 = resources.to_ip_resources_v6();
        if ipv6.is_inherited() || !ipv6.to_blocks().unwrap().is_empty() {
            cert.set_v6_resources(ipv6);
        }

        cert.set_authority_key_identifier(Some(signing_cert.cert().subject_key_identifier()));
        cert.set_ca_issuer(Some(signing_cert.uri().clone()));
        cert.set_crl_uri(Some(signing_cert.crl_uri()));

        match request {
            CertRequest::Ca(csr, _) => {
                let (ca_repository, rpki_manifest, rpki_notify, _pub_key) = csr.unpack();
                cert.set_basic_ca(Some(true));
                cert.set_ca_repository(Some(ca_repository));
                cert.set_rpki_manifest(Some(rpki_manifest));
                cert.set_rpki_notify(rpki_notify);
            }
            CertRequest::Ee(_, _) => {
                // cert.set_signed_object() ??
            }
        }

        Ok(cert)
    }

    /// Returns a validity period from 5 minutes ago (in case of NTP mess-up), to
    /// X weeks from now.
    pub fn sign_validity_weeks(weeks: i64) -> Validity {
        let from = Time::five_minutes_ago();
        let until = Time::now() + chrono::Duration::weeks(weeks);
        Validity::new(from, until)
    }

    pub fn sign_validity_days(days: i64) -> Validity {
        let from = Time::five_minutes_ago();
        let until = Time::now() + chrono::Duration::days(days);
        Validity::new(from, until)
    }
}

#[allow(clippy::large_enum_variant)]
enum CertRequest {
    Ca(CsrInfo, Validity),
    Ee(PublicKey, Validity),
}

trait ManifestEntry {
    fn mft_bytes(&self) -> Bytes;
    fn mft_hash(&self) -> Bytes {
        let digest = DigestAlgorithm::default().digest(self.mft_bytes().as_ref());
        Bytes::copy_from_slice(digest.as_ref())
    }
    fn mft_entry(&self, name: &str) -> FileAndHash<Bytes, Bytes> {
        FileAndHash::new(Bytes::copy_from_slice(name.as_bytes()), self.mft_hash())
    }
}

impl ManifestEntry for Crl {
    fn mft_bytes(&self) -> Bytes {
        self.to_captured().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use std::convert::TryFrom;

    use rpki::crypto::KeyIdentifier;

    use crate::commons::crypto::SignerError;

    use super::KeyMap;

    fn make_key_id(n: u8) -> KeyIdentifier {
        let mut dummy_key_id_bytes: [u8; 20] = [0; 20];
        dummy_key_id_bytes[19] = n;
        KeyIdentifier::try_from(&dummy_key_id_bytes[..]).unwrap()
    }    

    #[test]
    fn lookup_add_key_should_succeed() {
        let lookup = KeyMap::in_memory().unwrap();
        lookup.add_key(make_key_id(1), &[]);
    }

    #[test]
    #[should_panic]
    fn lookup_add_dup_key_should_fail() {
        let lookup = KeyMap::in_memory().unwrap();
        let key_id = make_key_id(1);
        lookup.add_key(key_id.clone(), &[]);
        lookup.add_key(key_id.clone(), &[]);
    }

    #[test]
    fn lookup_get_key_should_succeed() {
        let lookup = KeyMap::in_memory().unwrap();
        let key_id = make_key_id(1);
        let handle = [1, 2, 3];
        lookup.add_key(key_id.clone(), &handle);
        assert_eq!(handle, lookup.get_key(&key_id).unwrap().as_slice());
    }

    #[test]
    fn lookup_get_nonexisting_key_should_fail() {
        let lookup = KeyMap::in_memory().unwrap();
        let key_id = make_key_id(1);
        assert!(matches!(lookup.get_key(&key_id), Err(SignerError::KeyNotFound)));
    }
}