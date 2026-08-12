//! The one place a socket is opened.
//!
//! Everything else in this backend — encoding a request, framing the
//! reply, deciding what an error means — is a pure function of bytes, and
//! stays that way because the socket sits behind [`Transport`]. That
//! makes the protocol testable without a network, which matters more than
//! it sounds: the interesting cases are a stream split at an awkward
//! place, a refusal with no content, and a 529 in the middle of a reply,
//! and none of those can be produced on demand by a real endpoint.
//!
//! It is also the seam a desktop needs later, when requests have to go
//! through a proxy or a gateway the user configured.

use std::io::Read;
use std::time::Duration;

use crate::error::BackendError;

/// How long to wait for a connection. Short: a machine that cannot reach
/// the endpoint should say so while the user still remembers asking.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the response headers. The body has no deadline —
/// a hard turn can think for minutes before the first token, and the
/// provider's own keep-alives are what prove the connection is alive.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a transport hands back.
///
/// The body is a reader, not a buffer: a reply is consumed as it arrives,
/// so the agent can show text while the model is still writing it.
pub struct HttpResponse {
    pub status: u16,
    /// The `retry-after` header, when the provider sent one and gave it
    /// in seconds. A date-formatted value is reported as `None` rather
    /// than guessed at — the caller's own backoff is a better answer than
    /// a wrong deadline.
    pub retry_after: Option<Duration>,
    pub body: Box<dyn Read>,
}

/// One HTTP exchange.
///
/// `Send` because a turn runs on a worker thread. Not `Sync`: a
/// transport belongs to the backend that owns it.
pub trait Transport: Send {
    /// POST `body` to `url` and return the reply without reading it.
    ///
    /// A non-2xx status is a normal return, not an error — the backend
    /// reads the body to find out what the provider objected to. Only a
    /// failure to exchange anything at all is an `Err`.
    fn post(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, BackendError>;
}

/// The real one: blocking HTTPS.
///
/// Blocking is the point. A turn owns a worker thread from the first byte
/// of the request to the last byte of the stream, and the desktop's event
/// loop is never involved — it only drains the channel the sink writes
/// into.
pub struct HttpTransport {
    agent: ureq::Agent,
    /// Why the machine's certificates could not be read, if they could
    /// not. Kept rather than acted on at construction time, because a
    /// transport that refused to exist would leave the caller holding an
    /// error about TLS at a point where it was asking for a backend.
    trust_store: Option<String>,
}

impl HttpTransport {
    pub fn new() -> Self {
        HttpTransport::with_connect_timeout(CONNECT_TIMEOUT)
    }

    pub fn with_connect_timeout(connect: Duration) -> Self {
        // The operating system's trust store, not a CA bundle compiled
        // into the binary. A desktop should trust what the rest of the
        // desktop trusts — a corporate root the user installed works,
        // and a certificate the distribution withdrew stops working
        // without waiting for us to ship a release.
        let (roots, trust_store) = match system_roots() {
            Ok(roots) => (roots, None),
            Err(why) => (Vec::new(), Some(why)),
        };

        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::new_with_certs(&roots))
            .build();

        let config = ureq::Agent::config_builder()
            // A 4xx is an answer, and its body says what was wrong with
            // the request. Turning it into an error here would throw
            // that away before anyone read it.
            .http_status_as_error(false)
            .timeout_connect(Some(connect))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .user_agent(concat!("nacelle-ai/", env!("CARGO_PKG_VERSION")))
            .tls_config(tls)
            .build();

        HttpTransport {
            agent: ureq::Agent::new_with_config(config),
            trust_store,
        }
    }
}

/// The certificates this machine trusts.
///
/// An empty store is treated as a failure rather than as "trust
/// nothing": every request would fail with a certificate error, and the
/// user would have no way to tell that from the provider having a real
/// certificate problem. The distinction is worth the few lines — the fix
/// is usually one missing package.
fn system_roots() -> Result<Vec<ureq::tls::Certificate<'static>>, String> {
    let loaded = rustls_native_certs::load_native_certs();

    if loaded.certs.is_empty() {
        let why = loaded
            .errors
            .iter()
            .map(|err| err.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if why.is_empty() {
            "it holds no certificates".to_string()
        } else {
            why
        });
    }

    // Some certificates failing to parse while others load is ordinary —
    // a store accumulates odd files. What matters is that something
    // usable came out.
    Ok(loaded
        .certs
        .iter()
        .map(|der| ureq::tls::Certificate::from_der(der.as_ref()).to_owned())
        .collect())
}

impl Default for HttpTransport {
    fn default() -> Self {
        HttpTransport::new()
    }
}

impl Transport for HttpTransport {
    fn post(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, BackendError> {
        // Reported as a transport failure because that is what it is:
        // the request cannot leave the machine. Saying so plainly beats
        // the certificate error the handshake would produce, which reads
        // as if the provider were at fault.
        if let Some(why) = &self.trust_store {
            return Err(BackendError::Network(format!(
                "this machine's certificate store could not be used ({why}), \
                 so the provider's identity cannot be verified"
            )));
        }

        let mut request = self.agent.post(url);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }

        let response = request.send(body).map_err(failure)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(after);

        Ok(HttpResponse {
            status,
            retry_after,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

/// Anything that stops the exchange happening at all: DNS, TLS, connect,
/// reset, timeout. The provider never got to answer, so it is worth
/// asking again.
///
/// The message is built from the transport's own words and the URL, and
/// never from the request headers — an error is the most likely thing to
/// end up in a log or a bug report, and the credential is in a header.
fn failure(err: ureq::Error) -> BackendError {
    BackendError::Network(err.to_string())
}

/// `retry-after` as the provider most often sends it: whole seconds.
fn after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}
