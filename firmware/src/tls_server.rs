//! HTTPS on the board: a certificate it makes for itself, and the esp-tls handshake
//! that puts it to work.
//!
//! Two listeners, never one. Plain http on `mcp.port` stays exactly as it was, so a
//! mistake in here cannot lock the owner out of their own board; https is an extra port
//! with its own accept loop. That asymmetry is deliberate — it is the difference between
//! "https is down, use http" and "the board is unreachable".
//!
//! The certificate is generated **once**, on the first boot that needs it, and kept in
//! NVS. It is self-signed, so a browser will warn; the fingerprint this module returns is
//! what a person compares against the warning page, and it is the SHA-256 of the DER
//! certificate — the same value `openssl x509 -fingerprint -sha256` prints.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use esp32_agent_core::http::{Handshake, Stream};
use esp32_agent_core::store::KvStore;
use esp_idf_svc::sys;
use esp_idf_svc::tls::{EspTls, ServerConfig, Socket, X509};

/// NVS keys. Two separate strings rather than one blob: a partial write then costs the
/// certificate, not the key, and the pair can be inspected with any NVS dumper.
const CERT_KEY: &str = "tls_cert";
const KEY_KEY: &str = "tls_key";

/// Where https listens. Fixed rather than configurable: it is a browser-facing port, and
/// a second knob for it is a second way to get it wrong.
pub const TLS_PORT: u16 = 443;

pub struct Identity {
    /// PEM, NUL-terminated, exactly as esp-tls wants it.
    cert: Vec<u8>,
    key: Vec<u8>,
    /// Hex with colons, lower case.
    pub fingerprint: String,
}

impl Identity {
    pub fn cert_pem(&self) -> String {
        String::from_utf8_lossy(&self.cert[..self.cert.len().saturating_sub(1)]).into_owned()
    }

    /// Hand the identity to the core's TLS listener. The closure owns the bytes: esp-tls
    /// parses them during the handshake and keeps pointers into them only for that call,
    /// but keeping them alive for the life of the listener costs a kilobyte and removes
    /// a class of use-after-free.
    pub fn handshake(self) -> Handshake {
        let cert = Arc::new(self.cert);
        let key = Arc::new(self.key);
        Arc::new(move |stream: TcpStream| -> Option<Box<dyn Stream>> {
            let mut tls = match EspTls::adopt(Owned(Some(stream))) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("tls: could not create a session: {e}");
                    return None;
                }
            };
            let cfg = ServerConfig {
                server_cert: Some(X509::pem_until_nul(cert.as_slice())),
                server_key: Some(X509::pem_until_nul(key.as_slice())),
                ..Default::default()
            };
            if let Err(e) = tls.negotiate_server(&cfg) {
                log::warn!("tls: handshake failed: {e}");
                return None;
            }
            Some(Box::new(TlsStream { tls }))
        })
    }
}

/// Load the board's certificate, generating and storing it on the first call.
///
/// Generation happens here — before the radio, before the clock — because a certificate
/// with fixed dates does not need either. `2020-01-01 .. 2035-01-01` is deliberately
/// wide: a board whose clock is still 1970 cannot issue a certificate that a correct
/// client accepts, and waiting for SNTP would mean https that comes up minutes late.
pub fn identity(store: &mut dyn KvStore) -> Result<Identity, String> {
    if let (Some(cert), Some(key)) = (store.get(CERT_KEY), store.get(KEY_KEY)) {
        // A stored pair is only useful if it still parses; a corrupted NVS entry is a
        // board that serves https with nothing, which is worse than generating again.
        match fingerprint(&cert) {
            Ok(fp) => {
                log::info!("tls: certificate loaded ({} bytes, {fp})", cert.len());
                let mut c = cert.into_bytes();
                c.push(0);
                let mut k = key.into_bytes();
                k.push(0);
                return Ok(Identity { cert: c, key: k, fingerprint: fp });
            }
            Err(e) => log::warn!("tls: stored certificate is unusable ({e}); generating a new one"),
        }
    }
    let (cert, key, fp) = generate()?;
    log::info!("tls: generated a self-signed certificate ({fp})");
    if !store.set(CERT_KEY, &cert) || !store.set(KEY_KEY, &key) {
        // Serving what was just generated is still better than serving nothing; the next
        // boot simply generates again.
        log::warn!("tls: could not store the certificate; it will be regenerated next boot");
    }
    let mut c = cert.into_bytes();
    c.push(0);
    let mut k = key.into_bytes();
    k.push(0);
    Ok(Identity { cert: c, key: k, fingerprint: fp })
}

// ---------------------------------------------------------------- generation

/// mbedTLS wants an RNG callback; the chip's hardware RNG is one.
unsafe extern "C" fn rng(_ctx: *mut core::ffi::c_void, buf: *mut u8, len: usize) -> core::ffi::c_int {
    sys::esp_fill_random(buf as *mut core::ffi::c_void, len);
    0
}

fn check(what: &str, rc: core::ffi::c_int) -> Result<(), String> {
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed (mbedtls error -0x{:04x})", rc.unsigned_abs()))
    }
}

/// PEM writers place their output at the **end** of the buffer and leave the front
/// untouched — a difference that reads as "empty string" if you take the start.
fn tail(buf: Vec<u8>) -> String {
    let first = buf.iter().position(|b| *b != 0).unwrap_or(buf.len());
    let text = &buf[first..];
    String::from_utf8_lossy(text).trim_end_matches('\0').to_string()
}

fn generate() -> Result<(String, String, String), String> {
    unsafe {
        let mut key: sys::mbedtls_pk_context = std::mem::zeroed();
        sys::mbedtls_pk_init(&mut key);
        let out = generate_with(&mut key);
        sys::mbedtls_pk_free(&mut key);
        let (cert_pem, key_pem) = out?;
        let fp = fingerprint(&cert_pem)?;
        Ok((cert_pem, key_pem, fp))
    }
}

unsafe fn generate_with(key: &mut sys::mbedtls_pk_context) -> Result<(String, String), String> {
    let info = sys::mbedtls_pk_info_from_type(sys::mbedtls_pk_type_t_MBEDTLS_PK_ECKEY);
    check("pk_setup", sys::mbedtls_pk_setup(key, info))?;
    // For an ECKEY context `pk_ctx` **is** the key pair.
    let ec = key.private_pk_ctx as *mut sys::mbedtls_ecp_keypair;
    check(
        "ecp_gen_key",
        sys::mbedtls_ecp_gen_key(
            sys::mbedtls_ecp_group_id_MBEDTLS_ECP_DP_SECP256R1,
            ec,
            Some(rng),
            std::ptr::null_mut(),
        ),
    )?;

    let mut kbuf = vec![0u8; 2048];
    check("pk_write_key_pem", sys::mbedtls_pk_write_key_pem(key, kbuf.as_mut_ptr(), kbuf.len()))?;
    let key_pem = tail(kbuf);

    let mut crt: sys::mbedtls_x509write_cert = std::mem::zeroed();
    sys::mbedtls_x509write_crt_init(&mut crt);
    let built = (|| -> Result<String, String> {
        // Returns nothing: the only failure it has is a null context.
        sys::mbedtls_x509write_crt_set_version(&mut crt, sys::MBEDTLS_X509_CRT_VERSION_3 as i32);
        // Without this the certificate writer signs with "no hash", cannot find an OID
        // for that combination, and fails with MBEDTLS_ERR_OID_NOT_FOUND (-0x2e) — which
        // reads like a missing feature rather than a missing line.
        sys::mbedtls_x509write_crt_set_md_alg(&mut crt, sys::mbedtls_md_type_t_MBEDTLS_MD_SHA256);
        let mut serial = [0u8; 16];
        sys::esp_fill_random(serial.as_mut_ptr() as *mut core::ffi::c_void, serial.len());
        // A positive big-endian integer: high bit clear, and not zero.
        serial[0] &= 0x7f;
        serial[0] |= 0x01;
        check(
            "crt_set_serial",
            sys::mbedtls_x509write_crt_set_serial_raw(&mut crt, serial.as_mut_ptr(), serial.len()),
        )?;
        let name = b"CN=esp32-agent,O=esp32-agent,C=CN\0";
        check("crt_set_subject", sys::mbedtls_x509write_crt_set_subject_name(&mut crt, name.as_ptr()))?;
        check("crt_set_issuer", sys::mbedtls_x509write_crt_set_issuer_name(&mut crt, name.as_ptr()))?;
        // Fixed dates, deliberately wider than any clock this board can have. Waiting for
        // SNTP would give https that arrives minutes late; issuing from a 1970 clock would
        // give a certificate no correct client accepts.
        let not_before = b"20200101000000\0";
        let not_after = b"20350101000000\0";
        check(
            "crt_set_validity",
            sys::mbedtls_x509write_crt_set_validity(&mut crt, not_before.as_ptr(), not_after.as_ptr()),
        )?;
        sys::mbedtls_x509write_crt_set_basic_constraints(&mut crt, 0, 0);
        sys::mbedtls_x509write_crt_set_subject_key_identifier(&mut crt);
        sys::mbedtls_x509write_crt_set_authority_key_identifier(&mut crt);
        sys::mbedtls_x509write_crt_set_subject_key(&mut crt, key);
        sys::mbedtls_x509write_crt_set_issuer_key(&mut crt, key);

        let mut cbuf = vec![0u8; 4096];
        check(
            "crt_write_pem",
            sys::mbedtls_x509write_crt_pem(
                &mut crt,
                cbuf.as_mut_ptr(),
                cbuf.len(),
                Some(rng),
                std::ptr::null_mut(),
            ),
        )?;
        Ok(tail(cbuf))
    })();
    sys::mbedtls_x509write_crt_free(&mut crt);
    Ok((built?, key_pem))
}

/// SHA-256 over the DER form of a PEM certificate, formatted the way openssl prints it.
fn fingerprint(cert_pem: &str) -> Result<String, String> {
    let mut with_nul = cert_pem.as_bytes().to_vec();
    if with_nul.last() != Some(&0) {
        with_nul.push(0);
    }
    unsafe {
        let mut crt: sys::mbedtls_x509_crt = std::mem::zeroed();
        sys::mbedtls_x509_crt_init(&mut crt);
        let parsed =
            check("x509_crt_parse", sys::mbedtls_x509_crt_parse(&mut crt, with_nul.as_ptr(), with_nul.len()));
        let out = parsed.map(|_| {
            let der = std::slice::from_raw_parts(crt.raw.p as *const u8, crt.raw.len);
            let mut digest = [0u8; 32];
            let rc = sys::mbedtls_sha256(der.as_ptr(), der.len(), digest.as_mut_ptr(), 0);
            let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
            if rc == 0 {
                Ok(hex)
            } else {
                Err(format!("sha256 failed (mbedtls error -0x{:04x})", rc.unsigned_abs()))
            }
        });
        sys::mbedtls_x509_crt_free(&mut crt);
        out?
    }
}

// ---------------------------------------------------------------- the connection

/// A socket esp-tls is allowed to close once it owns it.
///
/// `release()` is what esp-tls calls before destroying the session, and handing the
/// descriptor over there is the only way to avoid a double close: esp-tls closes it.
/// Keeping the stream in an `Option` means a *failed* adopt still drops (and closes) the
/// socket normally — the earlier `ManuallyDrop` version leaked a descriptor on every
/// handshake that never got that far, which ends with the socket pool empty.
struct Owned(Option<TcpStream>);

impl Socket for Owned {
    fn handle(&self) -> i32 {
        self.0.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1)
    }
    fn release(&mut self) -> Result<(), esp_idf_svc::sys::EspError> {
        if let Some(stream) = self.0.take() {
            // esp-tls closes the descriptor from here on; dropping it here as well would
            // close whatever the next connection was handed that number for.
            std::mem::forget(stream);
        }
        Ok(())
    }
}

struct TlsStream {
    tls: EspTls<Owned>,
}

// Sound by construction: a `TlsStream` is moved to the connection thread that owns it
// and then used from that thread only. The raw pointer inside `EspTls` is the mbedTLS
// session, and mbedTLS contexts carry no thread affinity — the pointer's `Send` bound is
// missing because bindgen cannot say that, not because it is false.
unsafe impl Send for TlsStream {}

/// esp-tls reports "would block" as an error rather than retrying. The socket has a
/// timeout set, so these are transient by construction — a full TCP send buffer, not a
/// dead connection.
fn transient(code: i32) -> bool {
    // ESP_TLS_ERR_SSL_WANT_WRITE (-0x6900) and ESP_TLS_ERR_SSL_WANT_READ (-0x6880).
    code == -0x6900 || code == -0x6880
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.tls.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if transient(e.code()) => continue,
                Err(e) => {
                    log::warn!("tls: read failed: {e} (code {})", e.code());
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")));
                }
            }
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            match self.tls.write(buf) {
                Ok(n) => return Ok(n),
                Err(e) if transient(e.code()) => continue,
                Err(e) => {
                    // Without this a body that spans TLS records dies as "the server
                    // closed abruptly" on the client and as nothing at all here.
                    log::warn!("tls: write of {} bytes failed: {e} (code {})", buf.len(), e.code());
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")));
                }
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        // esp-tls writes straight to the socket through the TLS record layer; there is
        // no application-side buffer to push.
        Ok(())
    }
}

impl Stream for TlsStream {
    // The timeouts were set on the socket before it was adopted and esp-tls reads and
    // writes through that same descriptor, so they still apply — there is nothing to do
    // here, and pretending otherwise would mean reaching into esp-tls' internals.
    fn set_read_timeout(&self, _d: Option<Duration>) -> std::io::Result<()> {
        Ok(())
    }
    fn set_write_timeout(&self, _d: Option<Duration>) -> std::io::Result<()> {
        Ok(())
    }
    fn set_nodelay(&self, _on: bool) -> std::io::Result<()> {
        Ok(())
    }
}
