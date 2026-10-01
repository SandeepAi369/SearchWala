// ============================================================================
// SearchWala v8.0 — Unified HTTP surface for search engines
//
// WHY THIS EXISTS
// v7.0 built a `wreq::Client` with BoringSSL TLS impersonation, passed it into
// `dispatch_engines` as `_stealth_client`, and never used it — the leading
// underscore was the entire bug. `SearchEngine::search()` only accepted
// `&reqwest::Client`, so the stealth client was unreachable by construction,
// and every WAF-protected engine got plain reqwest and was blocked.
//
// Measured on 2026-10-01, same queries, reqwest vs wreq:
//   brave       429 rate-limited   ->  200, 42 .snippet elements
//   duckduckgo  202 CAPTCHA        ->  200, 10 result__a + 40 uddg= links
//   bing        200 (homepage!)    ->  200, 10 li.b_algo results
//   google      200 enablejs       ->  200 enablejs (needs more than TLS)
//
// This enum unifies both clients behind a 2-method surface that RETURNS THE
// STATUS CODE, which also forces the status check engines previously skipped
// (they called `resp.text()` unconditionally and parsed CAPTCHA pages as
// SERPs).
// ============================================================================

use std::time::Duration;

/// Cap on a single SERP response. Engine result pages are tens of KB; anything
/// past this is a misrouted download or an attack.
const MAX_SERP_BYTES: usize = 3_000_000;

/// Hard per-request ceiling, independent of the client-level timeout.
///
/// Discovered in testing: the Marginalia public API *hangs* on some queries —
/// "cricket world cup" returned in 0.9s, "cricket world cup 2027" never
/// returned and hit a 25s timeout. With the client timeout at 16s and the
/// dispatch deadline at 2.5-6s, the task was aborted mid-flight, so the health
/// registry recorded an attempt with NO outcome: invisible. A tight per-request
/// timeout turns a hang into a recorded transport error.
fn request_timeout() -> Duration {
    Duration::from_millis(
        std::env::var("ENGINE_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6_000),
    )
}

/// Insert `extra` into a HeaderMap with REPLACE semantics.
fn set_headers(
    map: &mut reqwest::header::HeaderMap,
    extra: &[(&str, &str)],
) -> Result<(), String> {
    for (k, v) in extra {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| format!("bad header name {k}: {e}"))?;
        let value = reqwest::header::HeaderValue::from_str(v)
            .map_err(|e| format!("bad header value for {k}: {e}"))?;
        map.insert(name, value); // insert == replace
    }
    Ok(())
}

#[derive(Clone)]
pub enum Fetcher {
    /// Plain reqwest — fine for indie engines and JSON APIs that do not
    /// fingerprint TLS.
    Plain(reqwest::Client),
    /// wreq + BoringSSL — real browser JA4 + HTTP/2 SETTINGS fingerprint.
    Stealth(wreq::Client),
}

pub struct Fetched {
    pub status: u16,
    pub body: String,
    /// URL after redirects — needed so parsers can resolve relative hrefs and
    /// filter self-links correctly.
    pub final_url: String,
}

impl Fetched {
    /// 200 and not a bot challenge.
    pub fn is_usable(&self) -> bool {
        self.status == 200 && !crate::engines::health::looks_like_challenge(&self.body)
    }
}

impl Fetcher {
    pub fn is_stealth(&self) -> bool {
        matches!(self, Fetcher::Stealth(_))
    }

    pub fn kind(&self) -> &'static str {
        if self.is_stealth() {
            "stealth"
        } else {
            "plain"
        }
    }

    pub async fn get(&self, url: &str, extra: &[(&str, &str)]) -> Result<Fetched, String> {
        // Space requests to the same registrable host. Engine aliases share
        // origins (brave + brave_news, 15 bing_* variants), and firing them
        // simultaneously is what earned the 429s.
        crate::host_gate::acquire(url).await;
        match self {
            Fetcher::Plain(c) => {
                let rb = crate::config::apply_browser_headers(c.get(url), url);
                // Build the Request, then REPLACE (not append) the extras.
                // reqwest's RequestBuilder::header appends, so setting `Accept`
                // twice sends two Accept lines — not what a browser does, and
                // some JSON APIs content-negotiate badly on it.
                let mut req = rb.build().map_err(|e| e.to_string())?;
                set_headers(req.headers_mut(), extra)?;
                let resp = with_timeout(c.execute(req)).await?.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
            Fetcher::Stealth(c) => {
                // IMPORTANT: wreq-util's Emulation already installs a
                // *consistent* User-Agent + Sec-CH-UA suite matched to the TLS
                // and HTTP/2 fingerprint. We deliberately do NOT override them.
                //
                // v7.0's hand-written profiles claimed Chrome 155 while
                // `Emulation::Chrome147` set a Chrome-147 TLS fingerprint. A
                // UA/TLS mismatch is a STRONGER bot signal than no spoofing at
                // all — it is exactly the inconsistency WAFs look for.
                let mut rb = c.get(url).header("Accept-Language", "en-US,en;q=0.9");
                for (k, v) in extra {
                    rb = rb.header(*k, *v);
                }
                let resp = with_timeout(rb.send()).await?.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                // wreq exposes `uri()`, not `url()`.
                let final_url = resp.uri().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
        }
    }

    pub async fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
        extra: &[(&str, &str)],
    ) -> Result<Fetched, String> {
        crate::host_gate::acquire(url).await;
        match self {
            Fetcher::Plain(c) => {
                let rb = crate::config::apply_browser_headers(c.post(url), url);
                let mut req = rb.form(form).build().map_err(|e| e.to_string())?;
                set_headers(req.headers_mut(), extra)?;
                let resp = with_timeout(c.execute(req)).await?.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
            Fetcher::Stealth(c) => {
                let mut rb = c.post(url).header("Accept-Language", "en-US,en;q=0.9");
                for (k, v) in extra {
                    rb = rb.header(*k, *v);
                }
                let resp = with_timeout(rb.form(form).send()).await?.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.uri().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
        }
    }

    /// Fetch and parse JSON. Used by the API-backed engines (Wikipedia, Wiby,
    /// Marginalia) where a status check still matters.
    pub async fn get_json(&self, url: &str) -> Result<(u16, serde_json::Value), String> {
        let page = self.get(url, &[("Accept", "application/json,text/plain,*/*")]).await?;
        if page.status != 200 {
            return Ok((page.status, serde_json::Value::Null));
        }
        let v = serde_json::from_str(&page.body).map_err(|e| format!("json: {e}"))?;
        Ok((page.status, v))
    }
}

/// Enforce `request_timeout()` around a send future.
async fn with_timeout<T>(fut: impl std::future::Future<Output = T>) -> Result<T, String> {
    tokio::time::timeout(request_timeout(), fut)
        .await
        .map_err(|_| format!("request timed out after {:?}", request_timeout()))
}

// ============================================================================
// Charset-aware body decoding
// ============================================================================

/// Decode a response body, honouring the document's declared charset.
///
/// v7.0 used `String::from_utf8_lossy` unconditionally, which turns every
/// windows-1252 / ISO-8859-1 / GBK / Shift_JIS page into replacement
/// characters. Many regional news sites still serve legacy encodings.
pub fn decode_body(bytes: &[u8]) -> String {
    let slice = &bytes[..bytes.len().min(MAX_SERP_BYTES)];

    // Byte-order marks win outright.
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(slice) {
        let (cow, _) = enc.decode_without_bom_handling(&slice[bom_len..]);
        return cow.into_owned();
    }

    // Try UTF-8 first — the overwhelming majority case.
    let (cow, _, had_errors) = encoding_rs::UTF_8.decode(slice);
    if !had_errors {
        return cow.into_owned();
    }

    // Malformed as UTF-8: honour the declared charset, else fall back to
    // windows-1252 (the HTML5 spec's default for unlabelled legacy content).
    if let Some(enc) = sniff_meta_charset(slice) {
        let (c2, _) = enc.decode_without_bom_handling(slice);
        return c2.into_owned();
    }
    let (c2, _, _) = encoding_rs::WINDOWS_1252.decode(slice);
    c2.into_owned()
}

/// Scan the document head for `<meta charset=..>` or
/// `<meta http-equiv="content-type" content="..; charset=..">`.
fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &bytes[..bytes.len().min(4096)];
    // Lossy is safe here: we only need the ASCII charset declaration.
    let ascii = String::from_utf8_lossy(head).to_lowercase();
    let i = ascii.find("charset")? + "charset".len();
    let name: String = ascii[i..]
        .trim_start_matches(['=', '"', '\'', ' '])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    encoding_rs::Encoding::for_label(name.as_bytes())
}

// ============================================================================
// Client builders
// ============================================================================

/// Plain reqwest client.
///
/// NOTE on compression: we enable gzip/brotli/deflate/zstd and let reqwest
/// emit `Accept-Encoding` itself. v7.0 hand-wrote the header with `zstd` in
/// two of three randomized variants while the client had no zstd support, so
/// zstd-serving origins (increasingly common behind Cloudflare) returned
/// bodies reqwest could not decode — silent, intermittent zero-result runs.
pub fn build_plain_client(proxy: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(16))
        .redirect(reqwest::redirect::Policy::limited(10))
        .pool_max_idle_per_host(10)
        .cookie_store(true)
        .user_agent(crate::config::random_user_agent())
        .tcp_nodelay(true)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .zstd(true)
        .https_only(false);

    if let Some(proxy_url) = proxy {
        match reqwest::Proxy::all(proxy_url) {
            Ok(cfg) => builder = builder.proxy(cfg),
            Err(err) => tracing::warn!("Invalid proxy {}: {}", proxy_url, err),
        }
    }

    builder.build().expect("Failed to build plain HTTP client")
}

/// Build both fetchers once per search. Cloning a `Fetcher` is an Arc clone of
/// the inner client, so tasks share connection pools — v7.0 built a brand-new
/// `reqwest::Client` *inside each spawned task* when proxies were enabled,
/// defeating connection reuse entirely.
pub fn build_pair(proxy: Option<&str>) -> (Fetcher, Fetcher) {
    (
        Fetcher::Plain(build_plain_client(proxy)),
        Fetcher::Stealth(crate::config::build_stealth_client(proxy)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_utf8() {
        assert_eq!(decode_body("hello".as_bytes()), "hello");
        let hindi = "भारत और पाकिस्तान";
        assert_eq!(decode_body(hindi.as_bytes()), hindi);
    }

    #[test]
    fn decodes_windows_1252_when_utf8_fails() {
        // 0xE9 is 'é' in windows-1252 but invalid standalone UTF-8.
        let bytes = b"<html><head></head><body>caf\xE9</body></html>";
        let out = decode_body(bytes);
        assert!(out.contains("café"), "got {out:?}");
        assert!(!out.contains('\u{FFFD}'), "must not contain replacement chars");
    }

    #[test]
    fn honours_declared_meta_charset() {
        // ISO-8859-1 declared explicitly; 0xF6 is 'ö'.
        let mut bytes = Vec::from(
            &b"<html><head><meta charset=\"iso-8859-1\"></head><body>sch\xF6n"[..],
        );
        bytes.extend_from_slice(b"</body></html>");
        let out = decode_body(&bytes);
        assert!(out.contains("schön"), "got {out:?}");
    }

    #[test]
    fn handles_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("héllo".as_bytes());
        let out = decode_body(&bytes);
        assert_eq!(out, "héllo", "BOM must be stripped");
    }

    #[test]
    fn caps_oversized_bodies_without_panicking() {
        // Multibyte characters straddling the cap must not panic.
        let big: String = "क".repeat(MAX_SERP_BYTES); // 3x the cap in bytes
        let out = decode_body(big.as_bytes());
        assert!(out.len() <= MAX_SERP_BYTES);
        assert!(!out.is_empty());
    }

    #[test]
    fn sniffs_charset_label_variants() {
        assert!(sniff_meta_charset(b"<meta charset=utf-8>").is_some());
        assert!(sniff_meta_charset(b"<meta charset=\"UTF-8\">").is_some());
        assert!(sniff_meta_charset(b"<meta charset='shift_jis'>").is_some());
        assert!(sniff_meta_charset(
            b"<meta http-equiv=content-type content=\"text/html; charset=gbk\">"
        )
        .is_some());
        assert!(sniff_meta_charset(b"<html><body>no declaration").is_none());
    }

    #[test]
    fn is_usable_rejects_non_200_and_challenges() {
        let ok = Fetched {
            status: 200,
            body: "<div class=\"snippet\">real result</div>".into(),
            final_url: "https://x.test/".into(),
        };
        assert!(ok.is_usable());

        let rate_limited = Fetched {
            status: 429,
            body: "<div class=\"snippet\">real result</div>".into(),
            final_url: "https://x.test/".into(),
        };
        assert!(!rate_limited.is_usable(), "429 must not be parsed");

        let soft_block = Fetched {
            status: 200,
            body: "Select all squares containing a duck".into(),
            final_url: "https://x.test/".into(),
        };
        assert!(!soft_block.is_usable(), "200 CAPTCHA must not be parsed");
    }
}
