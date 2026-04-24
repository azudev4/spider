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
    // Auth / redirect params (never change page content, just redirect targets).
    // Missing any of these creates the infinite-recursion trap where the login
    // page links back to itself with the current URL embedded, and each hop
    // adds another %25 layer of encoding.
    "redirect_to", "redirect", "return", "return_to", "returnto",
    "next", "continue", "destination", "go", "target",
    "back", "back_url", "back_to", "backurl", "backto",
    "from", "from_url",
    "reauth", "loggedout", "action",
    // Full-text search queries — result pages are ephemeral and non-canonical.
    // Kept distinct from filter params (`category`, `filter`, `tag`) which
    // DO define canonical listing pages worth crawling.
    "q", "s", "search", "query",
    // View-state params: same content, different presentation. Sort order and
    // layout don't define new canonical pages.
    "sort", "order", "orderby",
    "view", "display", "layout",
];

/// Threshold for the "strip all unknowns" heuristic. When a URL has this many
/// or more params that are neither tracking nor allowlisted content, we assume
/// the URL is app-state noise and strip all non-allowlisted params.
///
/// Set aggressively low (2). If a real site uses custom param names as
/// legitimate content filters (e.g. `?region=eu&currency=gbp`), fix by adding
/// those names to `CONTENT_PARAMS` — don't raise this threshold, since that
/// affects every site globally and masks the actual problem.
const UNKNOWN_PARAM_COLLAPSE_THRESHOLD: usize = 2;

/// Query parameters known to affect page content. When a URL has
/// `UNKNOWN_PARAM_COLLAPSE_THRESHOLD`+ unknown params (not in TRACKING_PARAMS
/// and not in this list), all unknown params are stripped to prevent app-state
/// URLs from polluting crawl results.
const CONTENT_PARAMS: &[&str] = &[
    // Pagination
    "page", "p", "pg", "paged", "offset", "start", "per_page",
    // Filtering (canonical listing-page dimensions; search + sort + view
    // params live in TRACKING_PARAMS — they don't define new canonical pages)
    "category", "cat", "filter", "tag", "type",
    // Localization
    "lang", "language", "locale", "hl",
    // Content identifiers
    "id", "slug", "post_type",
    // Common CMS/ecommerce
    "product", "collection", "brand", "color", "size", "price",
    "min_price", "max_price", "rating", "stock", "availability",
    // Content sections (tab/section can point to genuinely different content
    // on some sites; keep until proven otherwise)
    "tab", "section",
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

    /// Normalize URL for consistent comparison across all crawler components.
    ///
    /// Removes: fragments, tracking params, default ports.
    /// Keeps: scheme, host, path (including trailing slash — the site's own
    /// linking decides the canonical form), meaningful query params (sorted).
    ///
    /// Trailing-slash variants (`/foo` vs `/foo/`) stay distinct on purpose:
    /// forcing a canonical form globally would make us fetch the non-canonical
    /// variant on every WordPress/Apache site, eating crawl budget on 301 rows.
    /// The trivial-redirect dedup in `is_trivial_redirect` + the skip-rule in
    /// `build_page_entry` handles the cleanup downstream.
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
            // Trailing slashes are preserved — the site's own links decide the
            // canonical form.
            let path = parsed.path().to_lowercase();

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
            // Fallback for unparseable URLs — return as-is (trailing slashes preserved)
            url.to_string()
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

        // Pass 2: count unknown params — if at/over threshold, strip them all
        let unknown_count = params.iter().filter(|(_, _, known)| !known).count();
        if unknown_count >= UNKNOWN_PARAM_COLLAPSE_THRESHOLD {
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

/// Belt-and-suspenders check: returns true if the URL still has unknown-param
/// count at/over `UNKNOWN_PARAM_COLLAPSE_THRESHOLD` after normalization. Should
/// never fire because the normalizer already strips them; keeping it explicit
/// means that if the allowlist ever drifts, we drop the URL rather than
/// silently re-adding noise back to the queue.
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
    unknown_count >= UNKNOWN_PARAM_COLLAPSE_THRESHOLD
}

/// Detect "trivial" redirects where the destination differs from the source
/// only by: trailing slash on path, `www.` subdomain prefix, scheme
/// (http/https), or path case. Any mix of those counts.
///
/// Used at page-insert time to skip recording 301 rows that carry no new
/// information. Query-string / fragment differences count as non-trivial
/// (normalization would have collapsed those upstream anyway).
///
/// Pure, idempotent, side-effect free. Returns false on any parse error.
pub fn is_trivial_redirect(src: &str, dest: &str) -> bool {
    let Ok(s) = Url::parse(src) else { return false };
    let Ok(d) = Url::parse(dest) else { return false };

    // Build a canonical key that erases all four "trivial" differences.
    let canon = |u: &Url| -> (String, String, Option<String>) {
        let host = u
            .host_str()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_lowercase();
        let path = u.path().trim_end_matches('/').to_lowercase();
        let query = u.query().map(|q| q.to_string());
        (host, path, query)
    };

    canon(&s) == canon(&d)
}

// ---------------------------------------------------------------------------
// Tests (keep in sync with crawler/src/utils/url_normalization.rs tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_preserves_trailing_slash() {
        // Trailing slashes are preserved — the site's own links decide canonical form.
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/page/"),
            "https://example.com/page/"
        );
        assert_eq!(
            normalizer.normalize("https://example.com/page"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_slash_and_non_slash_are_distinct() {
        // Critical: /foo and /foo/ must stay distinct so spider fetches the
        // form the site actually uses. Trivial-redirect handling cleans up
        // mixed-linking cases downstream (see is_trivial_redirect).
        let mut normalizer = UrlNormalizer::new();
        let with_slash = normalizer.normalize("https://example.com/foo/");
        let without = normalizer.normalize("https://example.com/foo");
        assert_ne!(with_slash, without);
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
    fn test_at_threshold_unknowns_stripped() {
        // 2 unknown params → collapsed under threshold=2
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/app?foo=1&bar=2"),
            "https://example.com/app"
        );
        // 3 unknowns also stripped
        assert_eq!(
            normalizer.normalize("https://example.com/app?foo=1&bar=2&baz=3"),
            "https://example.com/app"
        );
    }

    #[test]
    fn test_one_unknown_param_kept() {
        // Single custom param could be legit, not collapsed
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/page?custom=value"),
            "https://example.com/page?custom=value"
        );
    }

    #[test]
    fn test_allowlisted_params_survive_regardless_of_count() {
        // color + size are allowlisted, so they survive even when mixed with unknowns
        let mut normalizer = UrlNormalizer::new();
        assert_eq!(
            normalizer.normalize("https://example.com/products?color=red&size=large"),
            "https://example.com/products?color=red&size=large"
        );
        assert_eq!(
            normalizer.normalize("https://example.com/shop?color=red&foo=1&bar=2"),
            "https://example.com/shop?color=red"
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
        // Trailing slash preserved; tracking stripped; allowlisted params kept.
        let mut url = Url::parse("https://example.com/page/?utm_source=x&page=2").unwrap();
        normalize_url_in_place(&mut url);
        assert_eq!(url.as_str(), "https://example.com/page/?page=2");
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

    #[test]
    fn test_search_query_params_stripped() {
        // `q`, `s`, `search`, `query` are search-box inputs — result pages are
        // ephemeral and non-canonical, so they collapse to the search landing page.
        let mut n = UrlNormalizer::new();
        assert_eq!(
            n.normalize("https://example.com/search?q=pizza"),
            "https://example.com/search"
        );
        assert_eq!(
            n.normalize("https://example.com/?s=shoes"),
            "https://example.com/"
        );
        assert_eq!(
            n.normalize("https://example.com/find?search=hello&query=world"),
            "https://example.com/find"
        );
    }

    #[test]
    fn test_search_variants_collapse_to_same_url() {
        let mut n = UrlNormalizer::new();
        let a = n.normalize("https://example.com/search?q=shoes");
        let b = n.normalize("https://example.com/search?q=dress");
        let c = n.normalize("https://example.com/search?q=hat");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn test_filter_params_still_kept() {
        // Filter params define canonical listing pages and must survive.
        // (sort=price will be stripped — tested separately below.)
        let mut n = UrlNormalizer::new();
        assert_eq!(
            n.normalize("https://example.com/shop?category=shoes"),
            "https://example.com/shop?category=shoes"
        );
    }

    #[test]
    fn test_sort_and_order_stripped() {
        let mut n = UrlNormalizer::new();
        // Sort order is view state, not content
        assert_eq!(
            n.normalize("https://example.com/shop?sort=price&order=asc"),
            "https://example.com/shop"
        );
        assert_eq!(
            n.normalize("https://example.com/shop?orderby=date"),
            "https://example.com/shop"
        );
    }

    #[test]
    fn test_view_layout_stripped() {
        let mut n = UrlNormalizer::new();
        // Grid vs list is presentation, same content
        assert_eq!(
            n.normalize("https://example.com/shop?view=grid"),
            "https://example.com/shop"
        );
        assert_eq!(
            n.normalize("https://example.com/shop?display=list&layout=wide"),
            "https://example.com/shop"
        );
    }

    #[test]
    fn test_sort_variants_collapse_to_same_url() {
        let mut n = UrlNormalizer::new();
        let a = n.normalize("https://example.com/shop?category=shoes&sort=price&order=asc");
        let b = n.normalize("https://example.com/shop?category=shoes&sort=price&order=desc");
        let c = n.normalize("https://example.com/shop?category=shoes&sort=date");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(a, "https://example.com/shop?category=shoes");
    }

    #[test]
    fn test_back_param_stripped() {
        // Auth-return `back=` param — must be stripped to avoid the infinite
        // recursion trap where each hop URL-encodes the previous one.
        let mut n = UrlNormalizer::new();
        assert_eq!(
            n.normalize("https://example.com/login?back=https://example.com/home"),
            "https://example.com/login"
        );
        // Nested-encoded variant (what the trap produces)
        assert_eq!(
            n.normalize("https://example.com/login?back=https://example.com/login%253Fback%253Dhttps://example.com/home"),
            "https://example.com/login"
        );
    }

    #[test]
    fn test_all_auth_return_variants_collapse() {
        // Every URL below goes to the same canonical /login page
        let mut n = UrlNormalizer::new();
        let a = n.normalize("https://example.com/login?back=/a");
        let b = n.normalize("https://example.com/login?back_url=/b");
        let c = n.normalize("https://example.com/login?back_to=/c");
        let d = n.normalize("https://example.com/login?from=/d");
        let e = n.normalize("https://example.com/login?return_to=/e");
        assert_eq!(a, "https://example.com/login");
        assert_eq!(b, "https://example.com/login");
        assert_eq!(c, "https://example.com/login");
        assert_eq!(d, "https://example.com/login");
        assert_eq!(e, "https://example.com/login");
    }

    // --- is_trivial_redirect -----------------------------------------------

    #[test]
    fn test_is_trivial_redirect_slash_only() {
        assert!(is_trivial_redirect(
            "https://example.com/foo",
            "https://example.com/foo/"
        ));
        assert!(is_trivial_redirect(
            "https://example.com/foo/",
            "https://example.com/foo"
        ));
    }

    #[test]
    fn test_is_trivial_redirect_www_only() {
        assert!(is_trivial_redirect(
            "https://example.com/foo",
            "https://www.example.com/foo"
        ));
        assert!(is_trivial_redirect(
            "https://www.example.com/foo",
            "https://example.com/foo"
        ));
    }

    #[test]
    fn test_is_trivial_redirect_scheme_only() {
        assert!(is_trivial_redirect(
            "http://example.com/foo",
            "https://example.com/foo"
        ));
    }

    #[test]
    fn test_is_trivial_redirect_case_only() {
        assert!(is_trivial_redirect(
            "https://example.com/Foo",
            "https://example.com/foo"
        ));
    }

    #[test]
    fn test_is_trivial_redirect_combined() {
        // All four differences at once → still trivial.
        assert!(is_trivial_redirect(
            "http://example.com/Foo",
            "https://www.example.com/foo/"
        ));
    }

    #[test]
    fn test_is_not_trivial_different_path() {
        assert!(!is_trivial_redirect(
            "https://example.com/foo",
            "https://example.com/bar"
        ));
    }

    #[test]
    fn test_is_not_trivial_different_query() {
        // Two different query strings are meaningful — not a trivial redirect.
        assert!(!is_trivial_redirect(
            "https://example.com/foo?a=1",
            "https://example.com/foo?b=2"
        ));
    }

    #[test]
    fn test_is_not_trivial_different_host() {
        // example.com → other.com is a real redirect, not trivial.
        assert!(!is_trivial_redirect(
            "https://example.com/foo",
            "https://other.com/foo"
        ));
    }

    #[test]
    fn test_is_not_trivial_invalid_urls() {
        assert!(!is_trivial_redirect("not a url", "https://example.com/foo"));
        assert!(!is_trivial_redirect("https://example.com/foo", "not a url"));
    }
}
