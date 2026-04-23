/// Centralized URL normalization — mirrors crawler/src/utils/url_normalization.rs.
///
/// Keep this file byte-equivalent (minus the `use` line) with the crawler copy.
/// Divergence between the two copies will break link-graph matching: the crawler
/// re-normalizes URLs on DB insert (database/models.rs) and the two normalizations
/// must produce identical strings.
use url::Url;
use std::collections::HashMap;

/// Tracking/analytics query parameters that should be stripped during normalization.
/// These never change the page content - they're only used for attribution tracking.
const TRACKING_PARAMS: &[&str] = &[
    // Google Analytics / Ads
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
    "gclid", "gclsrc", "dclid", "gbraid", "wbraid",
    // Facebook / Meta
    "fbclid", "fb_action_ids", "fb_action_types", "fb_source", "fb_ref",
    // Microsoft / Bing
    "msclkid",
    // HubSpot
    "hsa_cam", "hsa_grp", "hsa_mt", "hsa_src", "hsa_ad", "hsa_acc",
    "hsa_net", "hsa_ver", "hsa_la", "hsa_ol", "hsa_kw",
    // Mailchimp
    "mc_cid", "mc_eid",
    // Generic tracking
    "_ga", "_gl", "_hsenc", "_hsmi", "_openstat",
    // Referral / session IDs (never change page content)
    "ref", "ref_src", "referer", "referrer",
    // jQuery / cache busters (never change page content)
    "_",
    // WordPress internals (cron triggers appended to page URLs)
    "doing_wp_cron",
    // Auth / redirect params (never change page content, just redirect targets)
    "redirect_to", "redirect", "return", "return_to", "returnto",
    "next", "continue", "destination", "go", "target",
    "reauth", "loggedout", "action",
];

/// Query parameters known to affect page content. When a URL has 3+ unknown params
/// (not in TRACKING_PARAMS and not in this list), all unknown params are stripped
/// to prevent app-state URLs from polluting crawl results.
const CONTENT_PARAMS: &[&str] = &[
    // Pagination
    "page", "p", "pg", "paged", "offset", "start", "per_page",
    // Filtering & search
    "category", "cat", "filter", "sort", "order", "orderby",
    "search", "q", "s", "query", "tag", "type",
    // Localization
    "lang", "language", "locale", "hl",
    // Content identifiers
    "id", "slug", "post_type",
    // Common CMS/ecommerce
    "product", "collection", "brand", "color", "size", "price",
    "min_price", "max_price", "rating", "stock", "availability",
    // View controls
    "view", "display", "layout", "tab", "section",
];

/// Stateful URL normalizer with an internal cache. Use when normalizing the
/// same URL repeatedly; for one-off calls prefer `UrlNormalizer::normalize_once`
/// or the free-standing `normalize_url_string` / `normalize_url_in_place`.
pub struct UrlNormalizer {
    /// Cache for normalized URLs to avoid repeated parsing
    cache: HashMap<String, String>,
}

impl UrlNormalizer {
    /// Create a new normalizer with an empty cache.
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Normalize URL for consistent comparison across all crawler components
    /// Removes: fragments, trailing slashes, tracking params, default ports
    /// Keeps: scheme, host, path, meaningful query params (sorted)
    pub fn normalize(&mut self, url: &str) -> String {
        // Check cache first
        if let Some(cached) = self.cache.get(url) {
            return cached.clone();
        }

        let normalized = self.normalize_internal(url);

        // Cache the result
        self.cache.insert(url.to_string(), normalized.clone());

        normalized
    }

    /// Get normalized URL without caching (for one-off use)
    pub fn normalize_once(url: &str) -> String {
        let normalizer = Self::new();
        normalizer.normalize_internal(url)
    }

    /// Internal normalization logic
    fn normalize_internal(&self, url: &str) -> String {
        // Decode HTML entities before URL parsing so that e.g. &quot; and %22
        // normalize to the same representation (Url::parse encodes " as %22)
        let url = decode_html_entities(url);

        if let Ok(parsed) = Url::parse(&url) {
            // Lowercase path to match Spider's CaseInsensitiveString behaviour —
            // prevents BFS mismatches between /Monde/... and /monde/...
            let path = parsed.path().trim_end_matches('/').to_lowercase();

            // Build host with non-default port only
            let host_with_port = if let Some(port) = parsed.port() {
                let scheme = parsed.scheme();
                if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
                    parsed.host_str().unwrap_or("").to_string()
                } else {
                    format!("{}:{}", parsed.host_str().unwrap_or(""), port)
                }
            } else {
                parsed.host_str().unwrap_or("").to_string()
            };

            // Keep meaningful query params, strip tracking ones, sort for consistency
            let query_string = self.build_clean_query(&parsed);

            let base = format!("{}://{}{}",
                parsed.scheme(),
                host_with_port,
                if path.is_empty() { "" } else { &path }
            );

            if query_string.is_empty() {
                base
            } else {
                format!("{}?{}", base, query_string)
            }
        } else {
            // Fallback for unparseable URLs
            url.trim_end_matches('/').to_string()
        }
    }

    /// Build a clean query string with hybrid filtering:
    /// 1. Strip known tracking params (blocklist)
    /// 2. Strip params with UUID-like values (heuristic)
    /// 3. If 3+ unknown params remain, strip all non-allowlisted params
    fn build_clean_query(&self, parsed: &Url) -> String {
        let query = match parsed.query() {
            Some(q) if !q.is_empty() => q,
            _ => return String::new(),
        };

        // Pass 1: strip tracking params and UUID-valued params, normalize encoding
        let mut params: Vec<(String, String, bool)> = query.split('&')
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?;
                let value = parts.next().unwrap_or("");
                // Strip empty params (e.g. ?&_sfm_prix=... has an empty key before &)
                if key.is_empty() {
                    return None;
                }
                let decoded_key = decode_query_component(key);
                let key_lower = decoded_key.to_lowercase();
                // Strip known tracking params
                if TRACKING_PARAMS.iter().any(|&t| t == key_lower) {
                    return None;
                }
                let decoded_value = decode_query_component(value);
                // Strip params with UUID-like values
                if is_uuid_like(&decoded_value) {
                    return None;
                }
                let norm_key = encode_query_component(&decoded_key);
                let norm_value = encode_query_component(&decoded_value);
                let is_known = is_content_param(&key_lower);
                Some((norm_key, norm_value, is_known))
            })
            .collect();

        // Pass 2: count unknown params — if 3+, strip them all
        let unknown_count = params.iter().filter(|(_, _, known)| !known).count();
        if unknown_count >= 3 {
            params.retain(|(_, _, known)| *known);
        }

        // Sort by key for consistent ordering
        params.sort_by_key(|(k, _, _)| k.clone());

        // Rebuild query string
        let parts: Vec<String> = params.iter()
            .map(|(k, v, _)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{}={}", k, v)
                }
            })
            .collect();

        parts.join("&")
    }
}

impl Default for UrlNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode a query component (key or value) for normalization.
/// Treats `+` as space (form-urlencoded convention), decodes %XX sequences.
fn decode_query_component(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            result.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                result.push(hi * 16 + lo);
                i += 3;
            } else {
                result.push(bytes[i]);
                i += 1;
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// Re-encode a decoded query component with consistent percent-encoding.
/// Spaces become %20, only unsafe characters are encoded.
fn encode_query_component(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'.' | b'_' | b'~'  // unreserved (RFC 3986)
            | b'!' | b'*' | b'\'' | b'(' | b')'  // sub-delims safe in query
            | b':' | b'@' | b'/' | b',' | b';' => {
                result.push(b as char);
            }
            _ => {
                // Percent-encode everything else (spaces, +, &, =, etc.)
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode common HTML entities that may appear in URLs extracted from HTML attributes.
/// e.g. &quot; → " which Url::parse then percent-encodes as %22
fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
     .replace("&quot;", "\"")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&#39;", "'")
     .replace("&apos;", "'")
}

/// Check if a value looks like a UUID (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)
/// or a long hex string (32+ hex chars). These are app-state identifiers, not content params.
fn is_uuid_like(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    // Standard UUID: 8-4-4-4-12 hex with dashes
    if value.len() == 36 {
        let parts: Vec<&str> = value.split('-').collect();
        if parts.len() == 5
            && parts[0].len() == 8
            && parts[1].len() == 4
            && parts[2].len() == 4
            && parts[3].len() == 4
            && parts[4].len() == 12
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return true;
        }
    }
    // Long hex string (32+ chars, no dashes) — hash-like identifiers
    if value.len() >= 32 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    false
}

/// Check if a query param key is a known content-affecting parameter.
/// Matches exact names from CONTENT_PARAMS and the _sfm_ prefix (SearchAndFilter plugin).
fn is_content_param(key_lower: &str) -> bool {
    if key_lower.starts_with("_sfm_") {
        return true;
    }
    CONTENT_PARAMS.iter().any(|&p| p == key_lower)
}

// ---------------------------------------------------------------------------
// Spider-facing helpers (not in crawler copy — wrappers around the shared core)
// ---------------------------------------------------------------------------

/// Normalize a URL string. Convenience wrapper for `UrlNormalizer::normalize_once`.
#[inline]
pub fn normalize_url_string(url: &str) -> String {
    UrlNormalizer::normalize_once(url)
}

/// Normalize a parsed `Url` in place.
///
/// Used inside spider's `push_link` / `push_link_verify` / `push_link_check`
/// so that `links_visited` dedupes by canonical form and we never fetch the same
/// page twice via tracking-param variants.
///
/// Idempotent: normalizing an already-normalized URL is a no-op (aside from the
/// parse/format round-trip cost).
pub fn normalize_url_in_place(url: &mut Url) {
    let normalized = normalize_url_string(url.as_str());
    if let Ok(parsed) = Url::parse(&normalized) {
        *url = parsed;
    }
    // On parse failure we leave `url` untouched — better to fetch a slightly
    // non-canonical URL than to lose it entirely.
}

/// Belt-and-suspenders check: returns true if the URL still has 3+ unknown
/// query params after normalization. This should never fire because the
/// normalizer already strips 3+ unknowns; keeping it explicit means that if
/// the allowlist ever drifts, we drop the URL rather than silently re-adding
/// noise back to the queue.
pub fn should_drop_url(url: &Url) -> bool {
    let query = match url.query() {
        Some(q) if !q.is_empty() => q,
        _ => return false,
    };
    let unknown_count = query
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .filter(|k| !k.is_empty())
        .filter(|k| {
            let kl = k.to_lowercase();
            !TRACKING_PARAMS.iter().any(|&t| t == kl) && !is_content_param(&kl)
        })
        .count();
    unknown_count >= 3
}

// ---------------------------------------------------------------------------
// Tests (keep in sync with crawler/src/utils/url_normalization.rs tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_trailing_slash() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/page/"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_normalize_keeps_meaningful_query_params() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/biens?page=2"),
            "https://example.com/biens?page=2"
        );
    }

    #[test]
    fn test_normalize_strips_tracking_params() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/page?utm_source=google&utm_medium=cpc"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_pagination_pages_are_distinct() {
        let mut normalizer = UrlNormalizer::new();
        let page1 = normalizer.normalize("https://example.com/biens?page=1");
        let page2 = normalizer.normalize("https://example.com/biens?page=2");
        let page3 = normalizer.normalize("https://example.com/biens?page=3");
        assert_ne!(page1, page2);
        assert_ne!(page2, page3);
        assert_ne!(page1, page3);
    }

    #[test]
    fn test_three_plus_unknown_params_stripped() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/app?foo=1&bar=2&baz=3"),
            "https://example.com/app"
        );
    }

    #[test]
    fn test_two_unknown_params_kept() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/products?color=red&size=large"),
            "https://example.com/products?color=red&size=large"
        );
    }

    #[test]
    fn test_strips_uuid_param_values() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/page?itemUid=f7d895be-22d8-4c2c-bf38-ce09e8d1f4a0"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_sfm_prefix_is_allowlisted() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/vente?_sfm_prix=0+1000000&foo=1&bar=2&baz=3"),
            "https://example.com/vente?_sfm_prix=0%201000000"
        );
    }

    #[test]
    fn test_normalize_case_insensitive_path() {
        let mut normalizer = UrlNormalizer::new();
        let upper = normalizer.normalize("https://article-1.eu/Monde/relations-ue");
        let lower = normalizer.normalize("https://article-1.eu/monde/relations-ue");
        assert_eq!(upper, lower);
    }

    #[test]
    fn test_normalize_url_in_place() {
        let mut url = Url::parse("https://example.com/page/?utm_source=x&page=2").unwrap();
        normalize_url_in_place(&mut url);
        assert_eq!(url.as_str(), "https://example.com/page?page=2");
    }

    #[test]
    fn test_normalize_url_in_place_idempotent() {
        let mut url = Url::parse("https://example.com/page?page=2").unwrap();
        normalize_url_in_place(&mut url);
        let first = url.clone();
        normalize_url_in_place(&mut url);
        assert_eq!(url, first);
    }

    #[test]
    fn test_should_drop_url_only_triggers_on_excess_unknowns() {
        // After normalization the 3+ unknowns case is already stripped, so this
        // is the defensive fallback.
        let clean = Url::parse("https://example.com/page?page=2").unwrap();
        assert!(!should_drop_url(&clean));

        let raw = Url::parse("https://example.com/page?foo=1&bar=2&baz=3").unwrap();
        assert!(should_drop_url(&raw));
    }

    #[test]
    fn test_normalize_fragment_stripped() {
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/page#section"),
            "https://example.com/page"
        );
    }
}
