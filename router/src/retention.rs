//! Retention-posture drift: does the evidence behind each pinned label still
//! say what it said when a human read it?
//!
//! This is [`crate::drift`]'s sibling and follows the same constitution —
//! **read-only, database-free, never writes a value back** — but it answers a
//! different question, and the difference is worth stating because it decides
//! what the exit code means.
//!
//! `catalog-drift` reconciles two structured catalogs and can say *this number
//! is wrong*. Nothing here can say that. A provider's data-retention policy is
//! prose on a web page; no machine-readable source states "the operator's
//! account with Anthropic is zero-retention", because that fact lives in a
//! contract, not an API. So this command never compares postures at all. It
//! compares the PAGE against what the page said on `verified`, and a difference
//! means exactly one thing:
//!
//! > the evidence moved — a human must re-read it.
//!
//! A changed page is not a flipped posture. It is far more often a new nav bar.
//! That is why the failure text says re-verify rather than re-label, and why
//! nothing in this module can edit `tiers.toml`: the correct response to "the
//! policy page changed" is a person reading it, and an automated re-pin would
//! replace the one step that gives the label its meaning.
//!
//! ## Why the digest is over normalized text
//!
//! Hashing the raw response body would be useless: policy pages are served by
//! CMSes that embed build ids, CSRF tokens, and analytics nonces, so a raw
//! digest changes on almost every fetch and the check becomes noise a team
//! learns to ignore — worse than no check, because it looks like one.
//! [`normalize`] strips scripts, styles, comments, and tags, collapses
//! whitespace, and hashes the visible words. That is still not immune to a
//! layout change, and it is not meant to be: the design accepts a false
//! positive (a human reads a page that did not really change) to avoid a false
//! negative (a policy quietly moves and nobody looks).

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{RetentionPin, TierCatalog};

/// OpenRouter's provider directory, the corroboration source.
///
/// **Not on OpenRouter's documented `/api/v1` surface, deliberately chosen
/// anyway.** Their documented `/api/v1/providers` carries only
/// `privacy_policy_url` / `terms_of_service_url` — links, not claims — and
/// `/api/v1/models` and the per-model `/endpoints` route carry no data-policy
/// fields at all. The only machine-readable per-provider retention metadata
/// OpenRouter publishes is the `dataPolicy` object on this route, which is what
/// their own public "provider logging" docs page renders from.
///
/// Consuming an undocumented internal route would be indefensible for anything
/// load-bearing. It is defensible here for the same reason
/// [`crate::corroborate`] can consume a reseller's prices: this pass cannot
/// fail, cannot change the exit code, and cannot change a pin. If the route
/// disappears tomorrow, the command prints one SKIPPED line and finishes
/// exactly as it would have. It is JSON, so nothing here scrapes HTML for a
/// claim — the HTML fetch above is hashed, never interpreted.
pub const DEFAULT_CORROBORATION_URL: &str = "https://openrouter.ai/api/frontend/v1/all-providers";

const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What the re-fetch found for one pinned provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The page's normalized text still hashes to the pinned digest.
    Unchanged,
    /// The page still loads but no longer says what it said. **Actionable**:
    /// a human re-reads it, then bumps `verified` and `source_sha256`.
    Changed,
    /// The page could not be fetched or read.
    ///
    /// **Actionable, and that is a deliberate choice rather than an
    /// oversight.** A policy page that 404s is itself a signal — vendors
    /// reorganize their legal docs when the terms change — and a pin whose
    /// evidence cannot be reached has quietly lost its re-verification loop.
    /// Treating it as a pass would let a label assert itself forever against a
    /// URL that no longer exists. The cost is that a transient outage reddens
    /// the job; `--allow-drift` is the release valve, and unlike
    /// [`crate::corroborate`] this source is not a third party's opinion but
    /// the evidence the claim rests on.
    Unfetchable,
}

impl Verdict {
    /// Whether this verdict should set a non-zero exit code.
    #[must_use]
    pub const fn is_actionable(self) -> bool {
        match self {
            Self::Unchanged => false,
            Self::Changed | Self::Unfetchable => true,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unchanged => "UNCHANGED",
            Self::Changed => "PAGE CHANGED",
            Self::Unfetchable => "UNREACHABLE",
        }
    }
}

/// One provider's pinned claim and what re-reading its evidence found.
#[derive(Clone, Debug)]
pub struct ProviderCheck {
    /// Provider key as a candidate names it (`anthropic`, `openai`, `google`),
    /// or `tier <id>` for a per-tier override.
    pub subject: String,
    pub pin: RetentionPin,
    pub verdict: Verdict,
    /// The digest observed this run. `None` when the page could not be read.
    /// Printed on a mismatch so re-pinning is a copy, not a second command.
    pub observed_sha256: Option<String>,
    /// Why the fetch failed, when it did.
    pub error: Option<String>,
}

/// Every retention claim the catalog makes, each named once.
///
/// Provider pins and per-tier overrides both land here, because both are claims
/// with evidence behind them and both need the same re-verification loop. A pin
/// no candidate currently uses is still checked — it is still in the file, and
/// a label that rots unnoticed is exactly what this command exists to prevent.
#[must_use]
pub fn claims(catalog: &TierCatalog) -> Vec<(String, RetentionPin)> {
    let mut out: Vec<(String, RetentionPin)> = catalog
        .retention
        .iter()
        .map(|(provider, pin)| (provider.clone(), pin.clone()))
        .collect();
    out.extend(
        catalog
            .tiers
            .iter()
            .filter_map(|(tier_id, definition)| {
                definition
                    .retention
                    .clone()
                    .map(|pin| (format!("tier {tier_id}"), pin))
            })
            .collect::<Vec<_>>(),
    );
    out
}

/// Reduce an HTML document to the visible text a human would have read.
///
/// Deliberately not an HTML parser. The goal is a digest that survives a
/// deployment but not an edit, and for that a conservative tag-stripper is both
/// sufficient and far easier to reason about than a DOM: there is no tree to
/// mis-walk and no parser version to change the answer between releases.
///
/// `<script>` and `<style>` bodies are removed WITH their contents — they are
/// where build hashes and nonces live, and keeping them would defeat the whole
/// exercise. Everything else keeps its text.
#[must_use]
pub fn normalize(document: &str) -> String {
    let mut text = String::with_capacity(document.len());
    let mut rest = document;
    // Byte offsets throughout, taken only from ASCII matches, so every slice
    // lands on a char boundary. An earlier draft lowercased the whole document
    // and indexed it with char positions; that panics on any policy page
    // containing a single non-ASCII character (a curly quote is enough), and
    // these are marketing pages.
    while let Some(open) = rest.find('<') {
        text.push_str(&rest[..open]);
        let tail = &rest[open..];
        if let Some(len) = block_len(tail, "script")
            .or_else(|| block_len(tail, "style"))
            .or_else(|| comment_len(tail))
        {
            rest = &tail[len..];
            text.push(' ');
            continue;
        }
        // An ordinary tag: drop it, but leave a space so `<p>a</p><p>b</p>`
        // does not read as one word.
        match tail.find('>') {
            Some(close) => {
                rest = &tail[close + 1..];
                text.push(' ');
            }
            // An unterminated `<` is the end of anything readable.
            None => {
                rest = "";
                break;
            }
        }
    }
    text.push_str(rest);
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Length in bytes of a whole `<name …> … </name>` element at the start of
/// `tail`, contents included. `None` when `tail` does not open that element.
///
/// An unclosed block swallows the remainder: a truncated document is not a
/// document to hash a legal claim against, and consuming the tail makes that
/// visible as a digest change rather than as text that happens to parse.
fn block_len(tail: &str, name: &str) -> Option<usize> {
    let after_lt = tail.strip_prefix('<')?;
    if !after_lt.get(..name.len())?.eq_ignore_ascii_case(name) {
        return None;
    }
    // Guard against `<scriptish>`: what follows the name must terminate it.
    let next = after_lt[name.len()..].chars().next()?;
    if !matches!(next, '>' | ' ' | '\t' | '\n' | '\r' | '/') {
        return None;
    }
    let close = format!("</{name}>");
    Some(find_ascii_ci(tail, &close).map_or(tail.len(), |at| at + close.len()))
}

/// Length in bytes of an HTML comment at the start of `tail`.
fn comment_len(tail: &str) -> Option<usize> {
    if !tail.starts_with("<!--") {
        return None;
    }
    Some(tail.find("-->").map_or(tail.len(), |at| at + "-->".len()))
}

/// Case-insensitive search for an ASCII `needle`, returning a byte offset.
///
/// Safe to slice at: every byte of a multi-byte UTF-8 sequence is >= 0x80 and
/// so can never equal an ASCII byte, meaning a match can only begin on a char
/// boundary.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    debug_assert!(needle.is_ascii());
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Hex SHA-256 of a document's [`normalize`]d text. This is the value pinned as
/// `source_sha256` and the value printed when it no longer matches.
#[must_use]
pub fn digest(document: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize(document).as_bytes());
    hex::encode(hasher.finalize())
}

/// Compare one pinned claim against a document fetched or read for it.
#[must_use]
pub fn check(subject: &str, pin: &RetentionPin, document: &str) -> ProviderCheck {
    let observed = digest(document);
    let verdict = if observed.eq_ignore_ascii_case(pin.source_sha256.trim()) {
        Verdict::Unchanged
    } else {
        Verdict::Changed
    };
    ProviderCheck {
        subject: subject.to_owned(),
        pin: pin.clone(),
        verdict,
        observed_sha256: Some(observed),
        error: None,
    }
}

/// A check that never got as far as comparing anything.
#[must_use]
pub fn unfetchable(subject: &str, pin: &RetentionPin, error: &str) -> ProviderCheck {
    ProviderCheck {
        subject: subject.to_owned(),
        pin: pin.clone(),
        verdict: Verdict::Unfetchable,
        observed_sha256: None,
        error: Some(error.to_owned()),
    }
}

/// Fetch a policy page as text.
pub async fn fetch(url: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .get(url)
        // Policy pages are served by ordinary marketing CDNs, several of which
        // answer a bare programmatic client with a challenge page. A named
        // agent gets the document and makes the fetch attributable.
        .header(
            reqwest::header::USER_AGENT,
            concat!("zerorouter-retention-drift/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("fetching the policy page from {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("policy page {url} answered HTTP {status}");
    }
    response
        .text()
        .await
        .with_context(|| format!("reading the policy page body from {url}"))
}

// ---------------------------------------------------------------------------
// Corroboration — advisory only, exactly as `crate::corroborate` is for prices.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProviderDirectory {
    data: Vec<DirectoryEntry>,
}

#[derive(Debug, Deserialize)]
struct DirectoryEntry {
    slug: String,
    #[serde(rename = "dataPolicy")]
    data_policy: Option<DataPolicy>,
}

/// OpenRouter's characterization of one provider's data handling.
///
/// Every field is optional because the source omits rather than nulls, and one
/// omission matters: `retentionDays` is absent for most providers, OpenAI
/// included. Absent means UNKNOWN — never zero. Reading a missing window as "no
/// retention" would turn the one field that could contradict a `zero` pin into
/// silent agreement with it.
#[derive(Debug, Deserialize)]
struct DataPolicy {
    #[serde(rename = "retainsPrompts")]
    retains_prompts: Option<bool>,
    #[serde(rename = "retentionDays")]
    retention_days: Option<u32>,
    training: Option<bool>,
}

/// What the second source says about one pinned provider.
#[derive(Debug)]
pub struct Corroboration {
    pub subject: String,
    pub slug: String,
    pub retains_prompts: Option<bool>,
    pub retention_days: Option<u32>,
    pub training: Option<bool>,
    /// Whether the second source *appears* to disagree with the pinned posture.
    ///
    /// Informational in the strongest sense: this is a third party's summary of
    /// someone else's terms, with no `as_of` date and no citation, and it
    /// describes OpenRouter's OWN account with that provider rather than
    /// ZeroRouter's. A `zero` pin backed by a signed ZDR agreement SHOULD look
    /// like a disagreement here, because OpenRouter has no way to know about it.
    /// Never wired to an exit code.
    pub appears_to_disagree: bool,
}

/// Cross-check pinned postures against OpenRouter's provider directory.
///
/// Joined on the pin's explicit `openrouter_slug`, never on the provider key.
/// The keys look joinable and are not: ZeroRouter's `google` lane is the Gemini
/// Developer API, which this directory calls `google-ai-studio`
/// (`retainsPrompts: true`), while its `google-vertex` entry is a different
/// product with a different answer (`retainsPrompts: false`). Guessing would
/// have corroborated a lane the operator does not run.
pub fn corroborate(
    claims: &[(String, RetentionPin)],
    document: &str,
) -> Result<Vec<Corroboration>> {
    let directory: ProviderDirectory = serde_json::from_str(document)
        .context("the provider directory was not the expected JSON")?;
    let mut out = Vec::new();
    for (subject, pin) in claims {
        let Some(slug) = pin.openrouter_slug.as_deref() else {
            continue;
        };
        let Some(entry) = directory.data.iter().find(|entry| entry.slug == slug) else {
            continue;
        };
        let policy = entry.data_policy.as_ref();
        let retains_prompts = policy.and_then(|policy| policy.retains_prompts);
        // Disagreement is only ever asserted in the direction the evidence
        // supports: a `zero` pin against a source that says prompts are
        // retained. The reverse — a `standard` pin against a source claiming no
        // retention — is not flagged, because erring toward "standard" is the
        // house rule and a conservative label is never a defect.
        let appears_to_disagree =
            pin.posture == crate::config::RetentionPosture::Zero && retains_prompts == Some(true);
        out.push(Corroboration {
            subject: subject.clone(),
            slug: slug.to_owned(),
            retains_prompts,
            retention_days: policy.and_then(|policy| policy.retention_days),
            training: policy.and_then(|policy| policy.training),
            appears_to_disagree,
        });
    }
    Ok(out)
}

/// Fetch the corroboration document.
pub async fn fetch_corroboration(url: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("fetching the provider directory from {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("provider directory {url} answered HTTP {status}");
    }
    response
        .text()
        .await
        .with_context(|| format!("reading the provider directory body from {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetentionPosture;

    fn pin(posture: RetentionPosture, sha: &str) -> RetentionPin {
        RetentionPin {
            posture,
            description: "test".to_owned(),
            source_url: "https://example.test/policy".to_owned(),
            verified: "2026-08-20".to_owned(),
            source_sha256: sha.to_owned(),
            openrouter_slug: None,
        }
    }

    #[test]
    fn normalize_strips_tags_scripts_and_styles() {
        let html = "<html><head><style>body{color:red}</style>\
                    <script>var buildId='abc123';</script></head>\
                    <body><p>We retain inputs for 30 days.</p></body></html>";
        assert_eq!(normalize(html), "We retain inputs for 30 days.");
    }

    /// The property the whole mechanism rests on: a redeploy that changes only
    /// a build id must NOT move the digest, or the check becomes noise and gets
    /// switched off.
    #[test]
    fn a_changed_build_id_does_not_move_the_digest() {
        let first = "<html><script>var build='1a2b3c';</script>\
                     <body><p>We retain inputs for 30 days.</p></body></html>";
        let second = "<html><script>var build='9z8y7x';</script>\
                      <body><p>We retain inputs for 30 days.</p></body></html>";
        assert_eq!(digest(first), digest(second));
    }

    /// ...and the property that makes it a check at all: an edit to the prose
    /// MUST move it.
    #[test]
    fn an_edited_sentence_moves_the_digest() {
        let before = "<body><p>We retain inputs for 30 days.</p></body>";
        let after = "<body><p>We retain inputs for 90 days.</p></body>";
        assert_ne!(digest(before), digest(after));
    }

    #[test]
    fn whitespace_and_comments_do_not_move_the_digest() {
        let before = "<body><p>We retain inputs.</p></body>";
        let after = "<body>\n  <!-- reworded 2026 -->\n  <p>We   retain\n inputs.</p>\n</body>";
        assert_eq!(digest(before), digest(after));
    }

    /// Policy pages are marketing pages: curly quotes, em dashes, and accented
    /// names are routine. Normalizing must not panic on them — an earlier draft
    /// mixed char positions with byte offsets and would have.
    #[test]
    fn a_page_containing_non_ascii_text_normalizes_without_panicking() {
        let html = "<body><p>Nous conservons — “inputs” — 30 jours. €10 · naïve</p></body>";
        let normalized = normalize(html);
        assert!(normalized.contains("“inputs”"), "{normalized}");
        assert!(normalized.contains("naïve"), "{normalized}");
        // And it still hashes, which is the operation that would have panicked.
        assert_eq!(digest(html).len(), 64);
    }

    /// `<scriptish>` is not `<script>`; its text must survive.
    #[test]
    fn a_tag_merely_prefixed_script_is_not_treated_as_a_script_block() {
        assert_eq!(normalize("<scriptish>kept</scriptish>"), "kept");
    }

    #[test]
    fn a_matching_digest_is_unchanged_and_a_different_one_is_actionable() {
        let document = "<body>policy</body>";
        let matching = check(
            "anthropic",
            &pin(RetentionPosture::Standard, &digest(document)),
            document,
        );
        assert_eq!(matching.verdict, Verdict::Unchanged);
        assert!(!matching.verdict.is_actionable());

        let stale = check(
            "anthropic",
            &pin(RetentionPosture::Standard, &"0".repeat(64)),
            document,
        );
        assert_eq!(stale.verdict, Verdict::Changed);
        assert!(stale.verdict.is_actionable());
        // The observed digest is reported so re-pinning is a copy-paste.
        assert_eq!(
            stale.observed_sha256.as_deref(),
            Some(digest(document).as_str())
        );
    }

    #[test]
    fn an_unreachable_page_is_actionable() {
        let check = unfetchable(
            "openai",
            &pin(RetentionPosture::Standard, &"a".repeat(64)),
            "404",
        );
        assert_eq!(check.verdict, Verdict::Unfetchable);
        assert!(check.verdict.is_actionable());
    }

    const DIRECTORY: &str = r#"{"data":[
        {"slug":"anthropic","dataPolicy":{"retainsPrompts":true,"retentionDays":30,"training":false}},
        {"slug":"google-ai-studio","dataPolicy":{"retainsPrompts":true,"retentionDays":55,"training":false}},
        {"slug":"google-vertex","dataPolicy":{"retainsPrompts":false,"training":false}}
    ]}"#;

    fn slugged(posture: RetentionPosture, slug: &str) -> RetentionPin {
        RetentionPin {
            openrouter_slug: Some(slug.to_owned()),
            ..pin(posture, &"b".repeat(64))
        }
    }

    #[test]
    fn corroboration_joins_on_the_declared_slug_not_the_provider_key() {
        let claims = vec![(
            "google".to_owned(),
            slugged(RetentionPosture::Standard, "google-ai-studio"),
        )];
        let report = corroborate(&claims, DIRECTORY).expect("directory parses");
        assert_eq!(report.len(), 1);
        // The AI Studio entry, NOT the Vertex one a provider-key join would have
        // found: they disagree, and only one is the lane ZeroRouter runs.
        assert_eq!(report[0].slug, "google-ai-studio");
        assert_eq!(report[0].retains_prompts, Some(true));
        assert_eq!(report[0].retention_days, Some(55));
    }

    #[test]
    fn a_pin_without_a_slug_is_simply_not_corroborated() {
        let claims = vec![(
            "local".to_owned(),
            pin(RetentionPosture::Standard, &"c".repeat(64)),
        )];
        assert!(corroborate(&claims, DIRECTORY).expect("parses").is_empty());
    }

    #[test]
    fn only_a_zero_pin_can_appear_to_disagree() {
        let standard = vec![(
            "anthropic".to_owned(),
            slugged(RetentionPosture::Standard, "anthropic"),
        )];
        assert!(!corroborate(&standard, DIRECTORY).expect("parses")[0].appears_to_disagree);

        let zero = vec![(
            "anthropic".to_owned(),
            slugged(RetentionPosture::Zero, "anthropic"),
        )];
        assert!(corroborate(&zero, DIRECTORY).expect("parses")[0].appears_to_disagree);
    }

    /// A `zero` pin against a provider the source says does NOT retain is the
    /// agreeing case; and a `standard` pin is never flagged in either direction.
    #[test]
    fn a_conservative_pin_is_never_flagged() {
        let claims = vec![(
            "google".to_owned(),
            slugged(RetentionPosture::Standard, "google-vertex"),
        )];
        assert!(!corroborate(&claims, DIRECTORY).expect("parses")[0].appears_to_disagree);
    }

    #[test]
    fn a_malformed_directory_is_an_error_rather_than_a_panic() {
        assert!(corroborate(&[], "not json").is_err());
    }
}
