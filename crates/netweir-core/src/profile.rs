//! Browser profiles: everything a request needs to look like it came from
//! one particular browser. Profiles are data, in `profiles/*.toml` at the
//! repository root, compiled into the library.

use std::io::Write;

use serde::Deserialize;
use wreq::header::{HeaderMap, HeaderName, HeaderValue};
use wreq::http2::{
    Http2Options, PseudoId, PseudoOrder, SettingId, SettingsOrder, StreamDependency, StreamId,
};
use wreq::tls::compress::{CertificateCompressionAlgorithm, CertificateCompressor, Codec};
use wreq::tls::{AlpnProtocol, AlpsProtocol, ExtensionType, KeyShare, TlsOptions, TlsVersion};
use wreq::{Emulation, Group};

/// Profiles shipped with netweir, by name.
const BUILT_IN: &[(&str, &str)] = &[
    (
        "chrome-156-macos",
        include_str!("../../../profiles/chrome-156-macos.toml"),
    ),
    (
        "chrome-155-macos",
        include_str!("../../../profiles/chrome-155-macos.toml"),
    ),
    (
        "chrome-154-macos",
        include_str!("../../../profiles/chrome-154-macos.toml"),
    ),
    (
        "firefox-159-macos",
        include_str!("../../../profiles/firefox-159-macos.toml"),
    ),
    (
        "firefox-158-macos",
        include_str!("../../../profiles/firefox-158-macos.toml"),
    ),
    (
        "firefox-156-macos",
        include_str!("../../../profiles/firefox-156-macos.toml"),
    ),
    (
        "safari-27-macos",
        include_str!("../../../profiles/safari-27-macos.toml"),
    ),
];

/// What `profile="chrome"` means today.
pub const DEFAULT: &str = "chrome-156-macos";

/// The newest profile of each browser, by its short name.
const LATEST: &[(&str, &str)] = &[
    ("chrome", DEFAULT),
    ("firefox", "firefox-159-macos"),
    ("safari", "safari-27-macos"),
];

/// The profiles shipped with netweir, each with the short name that means
/// it today ("chrome" for the newest Chrome), if one does.
pub fn built_in() -> Vec<(&'static str, Option<&'static str>)> {
    BUILT_IN
        .iter()
        .map(|(name, _)| {
            let short = LATEST.iter().find(|(_, n)| n == name).map(|(s, _)| *s);
            (*name, short)
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub tls: Tls,
    pub http2: Http2,
    /// Every request, in order; the capitalisation is HTTP/1.1's. An empty
    /// value only reserves a position, for headers filled in per request
    /// (`Cookie`, from the jar).
    pub headers: Vec<(String, String)>,
    /// Sent after `headers`, over HTTP/2 only.
    #[serde(default)]
    pub http2_headers: Vec<(String, String)>,
    /// Header names in the order used after a redirect, when the browser
    /// orders them differently; empty means the same as `headers`.
    #[serde(default)]
    pub redirect_header_order: Vec<String>,
    /// Among cookies of equal path length, the newest goes first (Safari);
    /// otherwise the oldest, as Chrome and Firefox send them.
    #[serde(default)]
    pub cookies_newest_first: bool,
    /// Once an https origin has answered over HTTP/1.1, later connections
    /// to it offer only `http/1.1` in ALPN (Safari).
    #[serde(default)]
    pub remembers_http1: bool,
    /// Following a link from a page.
    #[serde(default)]
    pub link: Option<Shape>,
    /// Submitting a form: a navigation that sends a body.
    #[serde(default)]
    pub form: Option<Shape>,
    /// A page's own script sending a request with `fetch()`.
    #[serde(default)]
    pub fetch: Option<Shape>,
    /// An `<img>` on a page loading its picture.
    #[serde(default)]
    pub image: Option<Shape>,
    /// The GET after a form submission is answered with a redirect.
    #[serde(default)]
    pub after_form_redirect: Option<Shape>,
}

/// What the browser sends for one kind of request, captured as `headers`
/// and `http2_headers` are for typing an address. Empty values are filled
/// per request: Host, Content-Length, Content-Type, Origin, Referer,
/// Sec-Fetch-Site and Cookie.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub http2_headers: Vec<(String, String)>,
    /// The HTTP/2 HEADERS frame's priority, where this kind of request
    /// has one of its own.
    #[serde(default)]
    pub headers_priority: Option<HeadersPriority>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub ciphers: Vec<String>,
    pub groups: Vec<String>,
    pub key_shares: Vec<String>,
    pub signature_algorithms: Vec<String>,
    pub alpn: Vec<String>,
    pub alps: Vec<String>,
    pub alps_new_codepoint: bool,
    pub certificate_compression: Vec<String>,
    pub grease: bool,
    pub grease_signature_algorithms: bool,
    pub permute_extensions: bool,
    pub ech_grease: bool,
    pub ocsp_stapling: bool,
    pub signed_certificate_timestamps: bool,
    pub session_ticket: bool,
    pub pre_shared_key: bool,
    /// Leave session_ticket out of a handshake that resumes a session, as
    /// Firefox does; it still goes in a fresh one.
    #[serde(default)]
    pub resume_without_session_ticket: bool,
    pub trust_anchors: Option<String>,
    /// The exact order of the ClientHello's extensions, by number, for a
    /// browser that doesn't shuffle them. Empty: BoringSSL's order.
    #[serde(default)]
    pub extension_order: Vec<u16>,
    #[serde(default)]
    pub record_size_limit: Option<u16>,
    /// Signature schemes offered for delegated credentials; empty sends no
    /// delegated_credentials extension.
    #[serde(default)]
    pub delegated_credentials: Vec<String>,
    /// Send the TLS 1.3 ciphers in the order listed, rather than the order
    /// BoringSSL prefers.
    #[serde(default)]
    pub preserve_tls13_cipher_order: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http2 {
    pub settings: Vec<(u16, u32)>,
    pub window_update: u32,
    pub pseudo_header_order: Vec<String>,
    pub headers_priority: Option<HeadersPriority>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersPriority {
    pub exclusive: bool,
    pub depends_on: u32,
    pub weight: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError(pub String);

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProfileError {}

impl Profile {
    /// Gives the header `name` (any case) `value` wherever the profile
    /// sends it, keeping its place: for a browser saying something of its
    /// own, such as another language. A header it doesn't send stays
    /// unsent.
    pub fn set_header(&mut self, name: &str, value: &str) {
        let set = |headers: &mut Vec<(String, String)>| {
            for (k, v) in headers.iter_mut() {
                if k.eq_ignore_ascii_case(name) && !v.is_empty() {
                    *v = value.to_string();
                }
            }
        };
        set(&mut self.headers);
        set(&mut self.http2_headers);
        for shape in [
            &mut self.link,
            &mut self.form,
            &mut self.fetch,
            &mut self.image,
            &mut self.after_form_redirect,
        ]
        .into_iter()
        .flatten()
        {
            set(&mut shape.headers);
            set(&mut shape.http2_headers);
        }
    }

    /// Every header name a request can carry, in the order the browser
    /// sends them: the profile's headers (or their after-redirect order),
    /// then the HTTP/2-only ones.
    pub fn header_order(&self, redirected: bool) -> Vec<String> {
        let base: Vec<String> = if redirected && !self.redirect_header_order.is_empty() {
            self.redirect_header_order.clone()
        } else {
            self.headers.iter().map(|(k, _)| k.clone()).collect()
        };
        base.into_iter()
            .chain(self.http2_headers.iter().map(|(k, _)| k.clone()))
            .collect()
    }

    /// The built-in profile of Chrome `major` (such as "154"), if there is
    /// one.
    pub fn for_chrome(major: &str) -> Option<Profile> {
        Profile::for_browser("chrome", major)
    }

    /// The built-in profile of `browser` ("chrome" or "firefox") version
    /// `major`, if there is one.
    pub fn for_browser(browser: &str, major: &str) -> Option<Profile> {
        let prefix = format!("{browser}-{major}-");
        BUILT_IN
            .iter()
            .find(|(n, _)| n.starts_with(&prefix))
            .and_then(|(n, _)| Profile::named(n).ok())
    }

    /// The `Sec-CH-UA` value Google Chrome `major` sends, as captured for
    /// its profile, if netweir has one. Chrome for Testing leaves "Google
    /// Chrome" out of its own; given this, its pages present Chrome's.
    pub fn chrome_brands(major: &str) -> Option<String> {
        Profile::for_chrome(major)?
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("sec-ch-ua"))
            .map(|(_, v)| v.clone())
    }

    /// A built-in profile by name. `"chrome"` and `"firefox"` are the
    /// newest of each.
    pub fn named(name: &str) -> Result<Profile, ProfileError> {
        let name = LATEST
            .iter()
            .find(|(short, _)| *short == name)
            .map_or(name, |(_, full)| full);
        let (_, src) = BUILT_IN.iter().find(|(n, _)| *n == name).ok_or_else(|| {
            let known: Vec<&str> = LATEST
                .iter()
                .map(|(n, _)| *n)
                .chain(BUILT_IN.iter().map(|(n, _)| *n))
                .collect();
            ProfileError(format!(
                "unknown profile {name:?}; known: {}",
                known.join(", ")
            ))
        })?;
        Profile::from_toml(src)
    }

    pub fn from_toml(src: &str) -> Result<Profile, ProfileError> {
        toml::from_str(src).map_err(|e| ProfileError(format!("bad profile: {e}")))
    }

    pub(crate) fn emulation(&self) -> Result<Emulation, ProfileError> {
        self.emulation_with(None)
    }

    /// `emulation`, with `priority` for every request's HEADERS frame in
    /// place of the profile's.
    pub(crate) fn emulation_with(
        &self,
        priority: Option<&HeadersPriority>,
    ) -> Result<Emulation, ProfileError> {
        let bad = |what: &str, v: &str| {
            ProfileError(format!("profile {}: unknown {what} {v:?}", self.name))
        };
        let t = &self.tls;

        let key_shares = t
            .key_shares
            .iter()
            .map(|k| match k.as_str() {
                "X25519MLKEM768" => Ok(KeyShare::X25519_MLKEM768),
                "X25519" => Ok(KeyShare::X25519),
                "P-256" => Ok(KeyShare::P256),
                "P-384" => Ok(KeyShare::P384),
                "P-521" => Ok(KeyShare::P521),
                _ => Err(bad("key share", k)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let alpn = t
            .alpn
            .iter()
            .map(|p| match p.as_str() {
                "h2" => Ok(AlpnProtocol::HTTP2),
                "http/1.1" => Ok(AlpnProtocol::HTTP1),
                _ => Err(bad("ALPN protocol", p)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let alps = t
            .alps
            .iter()
            .map(|p| match p.as_str() {
                "h2" => Ok(AlpsProtocol::HTTP2),
                "http/1.1" => Ok(AlpsProtocol::HTTP1),
                _ => Err(bad("ALPS protocol", p)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let compressors = t
            .certificate_compression
            .iter()
            .map(|c| match c.as_str() {
                "brotli" => Ok(&BROTLI as &'static dyn CertificateCompressor),
                "zlib" => Ok(&ZLIB as &'static dyn CertificateCompressor),
                "zstd" => Ok(&ZSTD as &'static dyn CertificateCompressor),
                _ => Err(bad("certificate compression", c)),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut tls = TlsOptions::builder()
            .cipher_list(t.ciphers.join(":"))
            .curves_list(t.groups.join(":"))
            .key_shares(key_shares)
            .sigalgs_list(t.signature_algorithms.join(":"))
            .alpn_protocols(alpn)
            .alps_protocols(alps)
            .alps_use_new_codepoint(t.alps_new_codepoint)
            .certificate_compressors(compressors)
            .min_tls_version(TlsVersion::TLS_1_2)
            .max_tls_version(TlsVersion::TLS_1_3)
            .grease_enabled(t.grease)
            .grease_sigalgs_enabled(t.grease_signature_algorithms)
            .permute_extensions(t.permute_extensions)
            .enable_ech_grease(t.ech_grease)
            .enable_ocsp_stapling(t.ocsp_stapling)
            .enable_signed_cert_timestamps(t.signed_certificate_timestamps)
            .session_ticket(t.session_ticket)
            .pre_shared_key(t.pre_shared_key)
            .psk_skip_session_ticket(t.resume_without_session_ticket);
        if !t.extension_order.is_empty() {
            let order: Vec<ExtensionType> = t.extension_order.iter().map(|&e| e.into()).collect();
            tls = tls.extension_permutation(order);
        }
        if let Some(limit) = t.record_size_limit {
            tls = tls.record_size_limit(limit);
        }
        if !t.delegated_credentials.is_empty() {
            tls = tls.delegated_credentials(t.delegated_credentials.join(":"));
        }
        if t.preserve_tls13_cipher_order {
            tls = tls.preserve_tls13_cipher_list(true);
        }
        if let Some(hex) = &t.trust_anchors {
            tls = tls.trust_anchors(decode_hex(hex).ok_or_else(|| bad("trust anchor hex", hex))?);
        }

        let h = &self.http2;
        let mut order = SettingsOrder::builder();
        let mut http2 = Http2Options::builder();
        for &(id, value) in &h.settings {
            let (setting, builder) = match id {
                1 => (SettingId::HeaderTableSize, http2.header_table_size(value)),
                2 => (SettingId::EnablePush, http2.enable_push(value != 0)),
                3 => (
                    SettingId::MaxConcurrentStreams,
                    http2.max_concurrent_streams(value),
                ),
                4 => (
                    SettingId::InitialWindowSize,
                    http2.initial_window_size(value),
                ),
                5 => (SettingId::MaxFrameSize, http2.max_frame_size(value)),
                6 => (
                    SettingId::MaxHeaderListSize,
                    http2.max_header_list_size(value),
                ),
                8 => (
                    SettingId::EnableConnectProtocol,
                    http2.enable_connect_protocol(value != 0),
                ),
                9 => (
                    SettingId::NoRfc7540Priorities,
                    http2.no_rfc7540_priorities(value != 0),
                ),
                _ => return Err(bad("HTTP/2 setting", &id.to_string())),
            };
            order = order.push(setting);
            http2 = builder;
        }
        let pseudo = h
            .pseudo_header_order
            .iter()
            .map(|p| match p.as_str() {
                "method" => Ok(PseudoId::Method),
                "authority" => Ok(PseudoId::Authority),
                "scheme" => Ok(PseudoId::Scheme),
                "path" => Ok(PseudoId::Path),
                _ => Err(bad("pseudo-header", p)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The connection window starts at 65535; WINDOW_UPDATE adds to it.
        http2 = http2
            .initial_connection_window_size(65_535 + h.window_update)
            .settings_order(order.build())
            .headers_pseudo_order(PseudoOrder::builder().extend(pseudo).build());
        if let Some(p) = priority.or(h.headers_priority.as_ref()) {
            http2 = http2.headers_stream_dependency(StreamDependency::new(
                StreamId::from(p.depends_on),
                p.weight,
                p.exclusive,
            ));
        }

        // No header order: every request sets its own (Fetcher), and a
        // client-wide one would take the place of the hook that records
        // what a request was sent with.
        let mut headers = HeaderMap::new();
        for (k, v) in &self.headers {
            // Host comes from the URL; an empty value only holds a position.
            if k.eq_ignore_ascii_case("host") || v.is_empty() {
                continue;
            }
            let name = HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes())
                .map_err(|_| bad("header name", k))?;
            let value = HeaderValue::from_str(v).map_err(|_| bad("header value", v))?;
            headers.insert(name, value);
        }
        Ok(Emulation::builder()
            .tls_options(tls.build())
            .http2_options(http2.build())
            .headers(headers)
            .build(Group::new(self.name.clone())))
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Zlib certificate decompression (RFC 8879), which Firefox offers.
#[derive(Debug)]
struct Zlib;

static ZLIB: Zlib = Zlib;

impl CertificateCompressor for Zlib {
    fn compress(&self) -> Codec {
        Codec::Pointer(|input, out| {
            let mut w = flate2::write::ZlibEncoder::new(out, flate2::Compression::default());
            w.write_all(input)?;
            w.finish().map(|_| ())
        })
    }

    fn decompress(&self) -> Codec {
        Codec::Pointer(|input, out| {
            let mut r = flate2::read::ZlibDecoder::new(input);
            std::io::copy(&mut r, out).map(|_| ())
        })
    }

    fn algorithm(&self) -> CertificateCompressionAlgorithm {
        CertificateCompressionAlgorithm::ZLIB
    }
}

/// Zstandard certificate decompression (RFC 8879), which Firefox offers.
#[derive(Debug)]
struct Zstd;

static ZSTD: Zstd = Zstd;

impl CertificateCompressor for Zstd {
    fn compress(&self) -> Codec {
        Codec::Pointer(|input, out| {
            let data = zstd::stream::encode_all(input, 0)?;
            out.write_all(&data)
        })
    }

    fn decompress(&self) -> Codec {
        Codec::Pointer(|input, out| zstd::stream::copy_decode(input, out))
    }

    fn algorithm(&self) -> CertificateCompressionAlgorithm {
        CertificateCompressionAlgorithm::ZSTD
    }
}

/// Brotli certificate decompression (RFC 8879), which Chrome offers and
/// many CDNs use.
#[derive(Debug)]
struct Brotli;

static BROTLI: Brotli = Brotli;

impl CertificateCompressor for Brotli {
    fn compress(&self) -> Codec {
        Codec::Pointer(|input, out| {
            let mut w = brotli::CompressorWriter::new(out, 4096, 11, 22);
            w.write_all(input)?;
            w.flush()
        })
    }

    fn decompress(&self) -> Codec {
        Codec::Pointer(|input, out| {
            let mut r = brotli::Decompressor::new(input, 4096);
            std::io::copy(&mut r, out).map(|_| ())
        })
    }

    fn algorithm(&self) -> CertificateCompressionAlgorithm {
        CertificateCompressionAlgorithm::BROTLI
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_knows_every_kind_of_request() {
        for (name, _) in BUILT_IN {
            let p = Profile::named(name).unwrap();
            let shapes = [
                ("link", &p.link),
                ("form", &p.form),
                ("fetch", &p.fetch),
                ("image", &p.image),
                ("after_form_redirect", &p.after_form_redirect),
            ];
            for (kind, shape) in shapes {
                let shape = shape
                    .as_ref()
                    .unwrap_or_else(|| panic!("{name} has no [{kind}]"));
                // The values filled per request only hold a place.
                for (k, v) in &shape.headers {
                    if [
                        "referer",
                        "origin",
                        "sec-fetch-site",
                        "cookie",
                        "content-type",
                    ]
                    .contains(&k.to_ascii_lowercase().as_str())
                    {
                        assert!(v.is_empty(), "{name} [{kind}] {k} has a value");
                    }
                }
            }
        }
    }

    /// Where each browser puts Authorization, as recorded from Chrome 155
    /// and 156 and Firefox 158 and 159 (profiles/*.auth*.capture.json): Chrome straight
    /// after Host, Connection, Content-Length and Cache-Control, Firefox
    /// straight before Connection, in every kind of request. Earlier
    /// versions order their headers the same way.
    #[test]
    fn authorization_holds_the_place_its_browser_gives_it() {
        for (name, _) in BUILT_IN {
            let p = Profile::named(name).unwrap();
            let family = if name.starts_with("chrome") {
                "chrome"
            } else if name.starts_with("firefox") {
                "firefox"
            } else {
                continue;
            };
            let mut lists: Vec<(&str, Vec<String>)> = vec![
                ("headers", p.header_order(false)),
                ("redirect_header_order", p.header_order(true)),
            ];
            for (kind, shape) in [
                ("link", &p.link),
                ("form", &p.form),
                ("fetch", &p.fetch),
                ("image", &p.image),
                ("after_form_redirect", &p.after_form_redirect),
            ] {
                let shape = shape.as_ref().unwrap();
                lists.push((kind, shape.headers.iter().map(|(k, _)| k.clone()).collect()));
            }
            for (kind, names) in lists {
                let lower: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
                let at = lower
                    .iter()
                    .position(|n| n == "authorization")
                    .unwrap_or_else(|| panic!("{name} [{kind}] has no Authorization"));
                if family == "chrome" {
                    let before = ["host", "connection", "content-length", "cache-control"];
                    assert!(
                        lower[..at].iter().all(|n| before.contains(&n.as_str()))
                            && !before.contains(&lower[at + 1].as_str()),
                        "{name} [{kind}]: {names:?}"
                    );
                } else {
                    assert_eq!(lower[at + 1], "connection", "{name} [{kind}]: {names:?}");
                }
            }
        }
    }

    #[test]
    fn every_built_in_profile_loads_and_builds() {
        for (name, _) in BUILT_IN {
            let p = Profile::named(name).unwrap();
            assert_eq!(&p.name, name, "the file listed as {name} names itself");
            p.emulation().unwrap();
        }
        assert_eq!(Profile::named("chrome").unwrap().name, DEFAULT);
    }

    #[test]
    fn redirect_order_holds_the_same_headers_as_the_first_request() {
        for (name, _) in BUILT_IN {
            let p = Profile::named(name).unwrap();
            let mut first = p.header_order(false);
            let mut after = p.header_order(true);
            first.sort();
            after.sort();
            assert_eq!(first, after, "{name}");
        }
    }

    #[test]
    fn unknown_names_list_the_known_ones() {
        let err = Profile::named("netscape").unwrap_err();
        assert!(err.0.contains("chrome-156-macos"), "{err}");
    }

    #[test]
    fn typos_in_a_profile_are_errors() {
        let src = include_str!("../../../profiles/chrome-156-macos.toml").replace(
            r#"key_shares = ["X25519MLKEM768""#,
            r#"key_shares = ["X25519MLKEM769""#,
        );
        assert!(Profile::from_toml(&src).unwrap().emulation().is_err());
        assert!(Profile::from_toml("name = 1").is_err());
        let extra = format!(
            "{}
speed = 11
",
            include_str!("../../../profiles/chrome-156-macos.toml")
        );
        assert!(Profile::from_toml(&extra).is_err());
    }

    #[test]
    fn chrome_brands_come_from_the_profile_of_that_version() {
        let brands = Profile::chrome_brands("154").unwrap();
        assert!(brands.contains("\"Google Chrome\";v=\"154\""), "{brands}");
        let brands = Profile::chrome_brands("155").unwrap();
        assert!(brands.contains("\"Google Chrome\";v=\"155\""), "{brands}");
        let brands = Profile::chrome_brands("156").unwrap();
        assert!(brands.contains("\"Google Chrome\";v=\"156\""), "{brands}");
        assert_eq!(Profile::chrome_brands("1"), None);
    }
}
