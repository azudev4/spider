/// Centralized URL normalization — the crawler re-exports `UrlNormalizer`
/// + `is_trivial_redirect` from here so there's one source of truth.
///
/// Behavior is now driven by a runtime [`URLFilterConfig`] — admins can
/// override the defaults per client via the "launch a crawl" admin UI.
/// Code-side defaults live in the `default_*_params()` functions below and
/// still apply when no config is provided (backward compatible).
use lazy_static::lazy_static;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use url::Url;

// ---------------------------------------------------------------------------
// Categorized defaults
// ---------------------------------------------------------------------------
//
// The strip list is split into named categories so the admin UI can toggle
// the whole set on/off via `use_defaults`. Each category is a pure function
// returning a static slice, not a const — stays source-of-truth for docs and
// lets us build `all_default_strip_params()` once.

/// Google/Facebook/Microsoft/HubSpot/Mailchimp tracking + referral params.
/// Attribution tracking that never changes page content.
pub fn default_tracking_analytics_params() -> &'static [&'static str] {
    &[
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
    ]
}

/// Search-query params. Result pages are ephemeral and non-canonical.
pub fn default_search_params() -> &'static [&'static str] {
    &["q", "s", "search", "query"]
}

/// Sort / view-state params. Same content, different presentation.
pub fn default_sort_view_params() -> &'static [&'static str] {
    &[
        "sort", "order", "orderby",
        "view", "display", "layout",
    ]
}

/// Auth-return URLs. Missing any of these creates the infinite-recursion trap
/// where login pages link back to themselves with the current URL embedded,
/// each hop adding another %25 layer of encoding.
pub fn default_auth_return_params() -> &'static [&'static str] {
    &[
        "redirect_to", "redirect", "return", "return_to", "returnto",
        "next", "continue", "destination", "go", "target",
        "back", "back_url", "back_to", "backurl", "backto",
        "from", "from_url",
    ]
}

/// WordPress internals and misc CMS junk. The jQuery `_` cache buster lives
/// here too — it's WP-adjacent noise.
pub fn default_wp_internals_params() -> &'static [&'static str] {
    &[
        "doing_wp_cron",
        "_",
        "reauth", "loggedout", "action",
    ]
}

/// Content-affecting params preserved by the normalizer. These define
/// canonical listing pages (pagination, filters, localization, etc.) and
/// must survive the strip pass.
pub fn default_content_params() -> &'static [&'static str] {
    &[
        // Pagination
        "page", "p", "pg", "paged", "offset", "start", "per_page",
        // Filtering (canonical listing-page dimensions; search + sort + view
        // params live in the tracking lists — they don't define new canonical pages)
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
    ]
}

/// Union of all default strip categories. Used when building a
/// [`ResolvedFilter`] from `URLFilterConfig { use_defaults: true }`.
fn all_default_strip_params() -> impl Iterator<Item = &'static str> {
    default_tracking_analytics_params().iter().copied()
        .chain(default_search_params().iter().copied())
        .chain(default_sort_view_params().iter().copied())
        .chain(default_auth_return_params().iter().copied())
        .chain(default_wp_internals_params().iter().copied())
}

/// Default for the "strip all unknowns when count >= N" heuristic.
/// Set aggressively low (2). If a real site uses custom param names as
/// legitimate content filters, surface those via a team's
/// `custom_keep_params` override rather than bumping this — the threshold
/// is a blunt instrument.
pub const DEFAULT_UNKNOWN_THRESHOLD: usize = 2;

// ---------------------------------------------------------------------------
// Runtime configuration (per-crawl, per-client)
// ---------------------------------------------------------------------------

/// Per-crawl URL filter configuration. Admin-editable via the "launch a crawl"
/// section of the admin UI; stored on `teams.settings` as the team default and
/// snapshotted onto `crawl_sessions.url_filter_config` for reprocess consistency.
///
/// An empty `URLFilterConfig::default()` produces the same normalization
/// behavior as the old compile-time consts — full backward compatibility.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct URLFilterConfig {
    /// When true or None (default), apply the full built-in strip ruleset
    /// (tracking, search, sort/view, auth-return, WP internals).
    /// When false, built-in defaults are skipped entirely and only
    /// `custom_strip_params` is used.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub use_defaults: Option<bool>,

    /// Additional param names to strip, on top of (or instead of) defaults.
    /// Site-specific junk the built-in lists don't cover.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub custom_strip_params: Option<Vec<String>>,

    /// Param names to preserve, overriding the strip list and the unknown-param
    /// threshold. Used when a client genuinely treats `?pa_brand=acme` as a
    /// canonical filter dimension.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub custom_keep_params: Option<Vec<String>>,

    /// Override the unknown-param collapse threshold. Clamped to `[1, 10]`.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub unknown_param_threshold: Option<usize>,
}

/// Pre-computed hot-path form of a [`URLFilterConfig`]. Built once per crawl
/// and shared via `Arc` across the spider link frontier and the post-fetch
/// safety-net normalizers.
#[derive(Debug, Clone)]
pub struct ResolvedFilter {
    /// Lowercased param names the normalizer strips outright.
    strip: HashSet<String>,
    /// Lowercased param names the normalizer always preserves (overrides both
    /// strip and the unknown-count threshold).
    keep: HashSet<String>,
    /// If true, any param with a key starting with `_sfm_` is treated as
    /// content (WordPress Search-and-Filter plugin). Always true today.
    sfm_prefix: bool,
    /// `[1, 10]` — collapse URL to its base when this many unknown params remain.
    threshold: usize,
}

impl ResolvedFilter {
    fn is_stripped(&self, key_lower: &str) -> bool {
        // keep wins over strip — lets a team preserve a specific default param.
        !self.is_kept(key_lower) && self.strip.contains(key_lower)
    }

    fn is_kept(&self, key_lower: &str) -> bool {
        if self.keep.contains(key_lower) {
            return true;
        }
        if self.sfm_prefix && key_lower.starts_with("_sfm_") {
            return true;
        }
        false
    }

    fn threshold(&self) -> usize {
        self.threshold
    }
}

impl URLFilterConfig {
    /// Build the hot-path [`ResolvedFilter`] from this config + code defaults.
    pub fn resolve(&self) -> ResolvedFilter {
        let mut strip: HashSet<String> = HashSet::new();

        if self.use_defaults.unwrap_or(true) {
            for name in all_default_strip_params() {
                strip.insert(name.to_string());
            }
        }
        if let Some(extras) = &self.custom_strip_params {
            for name in extras {
                let lower = name.trim().to_lowercase();
                if !lower.is_empty() {
                    strip.insert(lower);
                }
            }
        }

        let mut keep: HashSet<String> = default_content_params()
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(extras) = &self.custom_keep_params {
            for name in extras {
                let lower = name.trim().to_lowercase();
                if !lower.is_empty() {
                    keep.insert(lower);
                }
            }
        }

        let threshold = self
            .unknown_param_threshold
            .unwrap_or(DEFAULT_UNKNOWN_THRESHOLD)
            .clamp(1, 10);

        ResolvedFilter {
            strip,
            keep,
            sfm_prefix: true,
            threshold,
        }
    }
}

lazy_static! {
    /// Lazily-initialized default filter that produces the same output the
    /// old compile-time consts did. Safe fallback when nothing else is installed.
    static ref DEFAULT_FILTER: Arc<ResolvedFilter> =
        Arc::new(URLFilterConfig::default().resolve());

    /// Process-level active filter, read by every stateless normalization
    /// helper. The crawler installs the session's filter via
    /// [`install_active_filter`] at crawl startup and resets to defaults when
    /// done. RwLock'd so multiple crawls in the same process (e.g. the GUI)
    /// can swap filters sequentially; readers just clone the Arc.
    static ref ACTIVE_FILTER: std::sync::RwLock<Arc<ResolvedFilter>> =
        std::sync::RwLock::new(Arc::clone(&DEFAULT_FILTER));
}

/// Get a shared default filter. Useful when constructing a `UrlNormalizer`
/// that should ignore any active filter installed on the process.
pub fn default_filter() -> Arc<ResolvedFilter> {
    Arc::clone(&DEFAULT_FILTER)
}

/// Return a cloned `Arc` of the currently-installed filter. All stateless
/// helpers call this, so swapping via `install_active_filter` is enough to
/// apply a custom config across the link frontier + post-fetch paths without
/// touching every call site's signature.
pub fn active_filter() -> Arc<ResolvedFilter> {
    match ACTIVE_FILTER.read() {
        Ok(guard) => Arc::clone(&*guard),
        // Poisoned lock — fall back to defaults rather than panicking.
        Err(_) => Arc::clone(&DEFAULT_FILTER),
    }
}

/// Install a new active filter for the current process. Subsequent
/// normalization calls pick it up. The crawler calls this once at crawl
/// startup after resolving the session's `URLFilterConfig`.
pub fn install_active_filter(filter: Arc<ResolvedFilter>) {
    if let Ok(mut guard) = ACTIVE_FILTER.write() {
        *guard = filter;
    }
}

/// Restore the active filter to defaults (e.g. between crawls in the GUI).
pub fn reset_active_filter() {
    install_active_filter(Arc::clone(&DEFAULT_FILTER))
}

/// Stateful URL normalizer with an internal cache. Use when normalizing the
/// same URL repeatedly; for one-off calls prefer `UrlNormalizer::normalize_once`
/// or the free-standing `normalize_url_string` / `normalize_url_in_place`.
pub struct UrlNormalizer {
    /// Cache for normalized URLs to avoid repeated parsing
    cache: HashMap<String, String>,
    /// Filter rules — strip list, keep list, threshold.
    filter: Arc<ResolvedFilter>,
}

impl UrlNormalizer {
    /// Create a new normalizer backed by the process-active filter.
    /// Use [`UrlNormalizer::with_filter`] when you need a specific filter
    /// (e.g. per-call overrides). Use [`UrlNormalizer::with_defaults`] when
    /// you want to explicitly ignore any process-installed override.
    pub fn new() -> Self {
        Self::with_filter(active_filter())
    }

    /// Create a new normalizer with the code-level defaults, ignoring any
    /// filter installed on the process.
    pub fn with_defaults() -> Self {
        Self::with_filter(default_filter())
    }

    /// Create a new normalizer backed by a specific resolved filter. Use this
    /// when you have a per-crawl `URLFilterConfig` to apply.
    pub fn with_filter(filter: Arc<ResolvedFilter>) -> Self {
        Self {
            cache: HashMap::new(),
            filter,
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
    /// 1. Strip configured tracking params (blocklist)
    /// 2. Strip params with UUID-like values (heuristic)
    /// 3. If N+ unknown params remain (N = filter.threshold), strip all
    ///    non-allowlisted params
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
                // Strip params the filter marks as tracking — but `keep` wins so
                // a team can preserve a specific default name via custom_keep.
                if self.filter.is_stripped(&key_lower) {
                    return None;
                }
                let decoded_value = decode_query_component(value);
                // Strip params with UUID-like values (unless explicitly kept)
                if !self.filter.is_kept(&key_lower) && is_uuid_like(&decoded_value) {
                    return None;
                }
                let norm_key = encode_query_component(&decoded_key);
                let norm_value = encode_query_component(&decoded_value);
                let is_known = self.filter.is_kept(&key_lower);
                Some((norm_key, norm_value, is_known))
            })
            .collect();

        // Pass 2: count unknown params — if at/over threshold, strip them all
        let unknown_count = params.iter().filter(|(_, _, known)| !known).count();
        if unknown_count >= self.filter.threshold() {
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

// (is_content_param removed — the logic now lives on `ResolvedFilter`.)

// ---------------------------------------------------------------------------
// Spider-facing helpers (not in crawler copy — wrappers around the shared core)
// ---------------------------------------------------------------------------

/// Normalize a URL string using the process-active filter (defaults when none
/// installed). For ad-hoc custom filters use [`normalize_url_string_with`].
#[inline]
pub fn normalize_url_string(url: &str) -> String {
    normalize_url_string_with(url, &active_filter())
}

/// Normalize a URL string against a specific resolved filter.
pub fn normalize_url_string_with(url: &str, filter: &Arc<ResolvedFilter>) -> String {
    UrlNormalizer::with_filter(Arc::clone(filter)).normalize_internal(url)
}

/// Normalize a parsed `Url` in place using the default filter.
/// For custom filters use [`normalize_url_in_place_with`].
///
/// Used inside spider's `push_link` / `push_link_verify` / `push_link_check`
/// so that `links_visited` dedupes by canonical form and we never fetch the same
/// page twice via tracking-param variants.
///
/// Idempotent: normalizing an already-normalized URL is a no-op (aside from the
/// parse/format round-trip cost).
pub fn normalize_url_in_place(url: &mut Url) {
    normalize_url_in_place_with(url, &active_filter())
}

/// Normalize a parsed `Url` in place against a specific resolved filter.
pub fn normalize_url_in_place_with(url: &mut Url, filter: &Arc<ResolvedFilter>) {
    let normalized = normalize_url_string_with(url.as_str(), filter);
    if let Ok(parsed) = Url::parse(&normalized) {
        *url = parsed;
    }
    // On parse failure we leave `url` untouched — better to fetch a slightly
    // non-canonical URL than to lose it entirely.
}

/// Belt-and-suspenders check: returns true if the URL still has unknown-param
/// count at/over the default threshold after normalization. Should
/// never fire because the normalizer already strips them; keeping it explicit
/// means that if the allowlist ever drifts, we drop the URL rather than
/// silently re-adding noise back to the queue.
pub fn should_drop_url(url: &Url) -> bool {
    should_drop_url_with(url, &active_filter())
}

/// Same as [`should_drop_url`], against a specific resolved filter.
pub fn should_drop_url_with(url: &Url, filter: &Arc<ResolvedFilter>) -> bool {
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
            !filter.is_stripped(&kl) && !filter.is_kept(&kl)
        })
        .count();
    unknown_count >= filter.threshold()
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

    // --- URLFilterConfig / ResolvedFilter ----------------------------------

    #[test]
    fn test_default_config_matches_legacy_behavior() {
        // URLFilterConfig::default() must produce the same output as the
        // previous hardcoded constants on a fixture of real-world URLs.
        let filter = Arc::new(URLFilterConfig::default().resolve());
        let mut n = UrlNormalizer::with_filter(filter);

        assert_eq!(
            n.normalize("https://example.com/page?utm_source=x&page=2"),
            "https://example.com/page?page=2"
        );
        assert_eq!(
            n.normalize("https://example.com/search?q=pizza"),
            "https://example.com/search"
        );
        assert_eq!(
            n.normalize("https://example.com/shop?sort=price"),
            "https://example.com/shop"
        );
        assert_eq!(
            n.normalize("https://example.com/login?back=/home"),
            "https://example.com/login"
        );
    }

    #[test]
    fn test_use_defaults_false_keeps_everything() {
        let cfg = URLFilterConfig {
            use_defaults: Some(false),
            ..Default::default()
        };
        let filter = Arc::new(cfg.resolve());
        let mut n = UrlNormalizer::with_filter(filter);
        // utm_source is a default-tracking param; with defaults off it survives.
        assert_eq!(
            n.normalize("https://example.com/page?utm_source=foo"),
            "https://example.com/page?utm_source=foo"
        );
    }

    #[test]
    fn test_custom_strip_params_add_to_strip() {
        let cfg = URLFilterConfig {
            custom_strip_params: Some(vec!["ref_id".to_string(), "SESSION_KEY".to_string()]),
            ..Default::default()
        };
        let filter = Arc::new(cfg.resolve());
        let mut n = UrlNormalizer::with_filter(filter);
        // case-insensitive match
        assert_eq!(
            n.normalize("https://example.com/page?ref_id=abc&session_key=xyz"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_custom_keep_wins_over_default_strip() {
        // utm_source is normally stripped; custom_keep should override.
        let cfg = URLFilterConfig {
            custom_keep_params: Some(vec!["utm_source".to_string()]),
            ..Default::default()
        };
        let filter = Arc::new(cfg.resolve());
        let mut n = UrlNormalizer::with_filter(filter);
        assert_eq!(
            n.normalize("https://example.com/page?utm_source=ab"),
            "https://example.com/page?utm_source=ab"
        );
    }

    #[test]
    fn test_custom_keep_survives_unknown_threshold() {
        // Normally 2+ unknowns collapse everything. With pa_brand explicitly
        // kept, it survives even when mixed with other unknowns.
        let cfg = URLFilterConfig {
            custom_keep_params: Some(vec!["pa_brand".to_string()]),
            ..Default::default()
        };
        let filter = Arc::new(cfg.resolve());
        let mut n = UrlNormalizer::with_filter(filter);
        assert_eq!(
            n.normalize("https://example.com/shop?pa_brand=acme&foo=1&bar=2"),
            "https://example.com/shop?pa_brand=acme"
        );
    }

    #[test]
    fn test_threshold_override() {
        let cfg = URLFilterConfig {
            unknown_param_threshold: Some(5),
            ..Default::default()
        };
        let filter = Arc::new(cfg.resolve());
        let mut n = UrlNormalizer::with_filter(filter);
        // With threshold=5, 4 unknowns survive as-is.
        let url = n.normalize("https://example.com/app?a=1&b=2&c=3&d=4");
        assert!(url.contains("a=1"));
        assert!(url.contains("b=2"));
        assert!(url.contains("c=3"));
        assert!(url.contains("d=4"));
    }

    #[test]
    fn test_threshold_clamped_to_one_ten() {
        let cfg_low = URLFilterConfig {
            unknown_param_threshold: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg_low.resolve().threshold(), 1);

        let cfg_high = URLFilterConfig {
            unknown_param_threshold: Some(9999),
            ..Default::default()
        };
        assert_eq!(cfg_high.resolve().threshold(), 10);
    }

    #[test]
    fn test_config_idempotent_normalization() {
        let cfg = URLFilterConfig {
            custom_strip_params: Some(vec!["weird".to_string()]),
            custom_keep_params: Some(vec!["pa_brand".to_string()]),
            unknown_param_threshold: Some(3),
            ..Default::default()
        };
        let filter = Arc::new(cfg.resolve());

        let fixtures = [
            "https://example.com/shop?pa_brand=acme",
            "https://example.com/page?weird=x&page=2",
            "https://example.com/foo/?utm_source=x&a=1",
            "https://example.com/search?q=hello",
        ];

        for raw in fixtures {
            let once = normalize_url_string_with(raw, &filter);
            let twice = normalize_url_string_with(&once, &filter);
            assert_eq!(once, twice, "non-idempotent for {raw}");
        }
    }

    // Serde round-trip is tested crawler-side where serde_json is a regular dep.
}
