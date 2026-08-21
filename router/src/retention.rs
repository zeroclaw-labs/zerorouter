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
//!
//! ## Why some pins hash only part of the page
//!
//! That trade has a limit, and `developers.openai.com` found it: a docs app
//! renders its entire site navigation into the page's visible text, so the
//! digest moves whenever the vendor publishes ANY unrelated document. It
//! churned three times in two days on deploys that touched nothing about
//! retention. Past some rate a false positive stops being a tolerable cost and
//! becomes the failure itself — an alarm that cries wolf daily is one a team
//! learns to clear without reading, which is worse than no alarm because it
//! still looks like one.
//!
//! The fix is to narrow the EVIDENCE, never to lower the bar: a pin may declare
//! [`crate::config::RetentionPin::source_extract_anchors`], and then the digest
//! is taken over bounded regions around those anchors instead of the whole
//! page. Inside a region nothing is relaxed — a reworded sentence moves the
//! digest exactly as before. Outside it, text that was never evidence stops
//! being able to raise an alarm.
//!
//! The one thing such a mechanism must never do is fail soft. An extractor that
//! loses its anchor and hashes an empty string would report UNCHANGED forever
//! against a page saying anything at all. So [`extract`] has no fallback: a
//! missing or ambiguous anchor is reported as `PAGE CHANGED`, with no observed
//! digest offered to copy.

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
    hash(&normalize(document))
}

/// Hex SHA-256 of an already-normalized (or already-extracted) string.
fn hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// How much normalized text one anchor selects, in CHARACTERS, counting from
/// the first character of the anchor itself.
///
/// Characters and not bytes, and that is not a detail: these are marketing and
/// legal pages, routinely carrying curly quotes and accented names, and slicing
/// a `str` at a byte offset that lands mid-codepoint panics. The same class of
/// bug is recorded in [`normalize`]'s own comment, which is why this file
/// counts what it can count safely.
///
/// One constant rather than a per-pin window, deliberately. A window is a
/// second number an author can get wrong in a direction that is invisible —
/// set it too small and the digest silently stops covering the sentence the
/// posture rests on, while every run still reports UNCHANGED. Anchors are the
/// tunable; a pin that needs to cover more ground adds another anchor, which
/// is a change whose effect is visible in the extract rather than hidden in a
/// length. 2,000 characters is roughly two screens of prose and was chosen by
/// measuring the two pages that needed it: OpenAI's three commitments span
/// ~1,800 characters from the section's opening sentence.
const EXTRACT_WINDOW_CHARS: usize = 2_000;

/// Separator between the windows of a multi-anchor extract.
///
/// Present so two adjacent windows cannot be forged into one another by text
/// moving across the boundary: without it, content shifting from the tail of
/// one window to the head of the next could leave the concatenation identical
/// while the page genuinely changed.
const EXTRACT_JOIN: &str = "\n\u{241f}\n";

/// Reduce normalized page text to the bounded regions `anchors` select, or say
/// why that could not be done.
///
/// **This is the narrowing that keeps the check readable, and its whole safety
/// argument is that it fails LOUDLY.** The failure it exists to prevent is not
/// a false alarm — those are merely annoying — it is a silent pass: an
/// extractor that cannot find its anchor, hashes the empty string, and reports
/// UNCHANGED forever while the policy page it was supposed to be watching says
/// whatever it likes. So there is no fallback path here. Every way this can
/// fail returns an error that the caller turns into `PAGE CHANGED`, which is
/// the same verdict a rewritten page gets, and it is the right one: an anchor
/// that has moved IS the page having changed in the region that matters.
///
/// Two conditions, both required, both reported rather than worked around:
///
/// - **Exactly zero** occurrences: the sentence the evidence was read from is
///   gone. Nothing to hash, and nothing legitimate to conclude.
/// - **More than one**: the anchor no longer identifies a unique place, so
///   which region gets hashed would depend on document order — and a later
///   edit could move the extract to a different part of the page while the
///   digest kept matching. An ambiguous anchor is not a bounded extract.
///
/// Matching is **case-sensitive**, which is a deliberate choice and not an
/// oversight. Vertex's page carries the heading `Training restriction` directly
/// above a quotation of the contractual term `"Training Restriction"`; matched
/// case-insensitively that anchor is ambiguous and the pin would be red on a
/// page nobody had edited. Case is also information on a legal page — a term
/// of art that stops being capitalized has stopped being a defined term — so
/// treating a case change as an edit worth a human's attention is correct on
/// its own merits.
///
/// The extract runs FORWARD from each anchor and never backward, which is what
/// makes it immune to the churn it was built for: navigation renders ahead of
/// the content, so an arbitrary amount of it can appear, disappear, or be
/// reordered above the anchor without moving the digest by one bit.
pub fn extract(normalized: &str, anchors: &[String]) -> Result<String, String> {
    let mut regions = Vec::with_capacity(anchors.len());
    for anchor in anchors {
        let mut found = normalized.match_indices(anchor.as_str());
        let Some((at, _)) = found.next() else {
            return Err(format!(
                "the extract anchor {anchor:?} no longer appears on the page, so the evidence \
                 this pin was read from cannot be located"
            ));
        };
        if found.next().is_some() {
            let total = normalized.matches(anchor.as_str()).count();
            return Err(format!(
                "the extract anchor {anchor:?} appears {total} times, so it no longer identifies \
                 one region; narrow it to a phrase unique to the retention section"
            ));
        }
        regions.push(
            normalized[at..]
                .chars()
                .take(EXTRACT_WINDOW_CHARS)
                .collect::<String>(),
        );
    }
    Ok(regions.join(EXTRACT_JOIN))
}

/// The text a pin's digest is taken over: the whole normalized page, or the
/// bounded extract its anchors select.
pub fn evidence(pin: &RetentionPin, document: &str) -> Result<String, String> {
    let normalized = normalize(document);
    if pin.source_extract_anchors.is_empty() {
        return Ok(normalized);
    }
    extract(&normalized, &pin.source_extract_anchors)
}

/// Compare one pinned claim against a document fetched or read for it.
#[must_use]
pub fn check(subject: &str, pin: &RetentionPin, document: &str) -> ProviderCheck {
    let observed = match evidence(pin, document) {
        Ok(evidence) => hash(&evidence),
        // An extract that could not be taken reports CHANGED with the reason
        // and NO observed digest. Withholding the digest is the point: every
        // other `Changed` row prints one so re-pinning is a copy-paste, and a
        // number printed here would be a digest of whatever the broken
        // extractor happened to produce. Copying it would pin the pin to
        // nothing and turn this check off permanently, which is precisely the
        // silent pass the anchor mechanism exists to make impossible.
        Err(error) => {
            return ProviderCheck {
                subject: subject.to_owned(),
                pin: pin.clone(),
                verdict: Verdict::Changed,
                observed_sha256: None,
                error: Some(error),
            };
        }
    };
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
// The live half of the Bedrock claim.
//
// Everything above answers "did the EVIDENCE move?" by hashing a page. That is
// the only question available for a pin whose posture rests on a vendor's
// published policy, because no API states what a contract says.
//
// `[retention.bedrock]` is the one pin where a second question is both askable
// and necessary, and the difference is worth stating plainly. Its posture does
// not rest on a contract; it rests on a SETTING on the operator's own AWS
// account, and settings change. Hashing AWS's docs page catches AWS rewording
// what `none` means. It cannot catch someone flipping the account to `default`,
// and after that flip every check above still passes while `/v1/models`
// continues telling customers their prompts are never stored. That is the
// failure this exists to close.
//
// It is OPT-IN (`--bedrock-live`) and that is not timidity. The daily CI job
// runs without AWS credentials by design, and a check that needs a secret must
// not be able to redden a job that cannot hold one. What it is NOT is advisory:
// unlike `--corroborate`, this reads ZeroRouter's OWN account rather than a
// third party's opinion, so when it is asked for it decides the exit code.
// Asking for it and not being able to run it is a failure too — a credential
// that rotated out must not read as a pass.
// ---------------------------------------------------------------------------

/// Bedrock's account-scoped retention setting, on the mantle control plane.
///
/// Its own constant rather than something derived from the provider entry's
/// `base_url`: that URL addresses the INFERENCE surface
/// (`/anthropic/v1/messages`) and this is a different API on the same host, so
/// deriving one from the other by string surgery would couple two contracts
/// that AWS versions separately. The region is the only shared part, and it is
/// read from the same environment variable the router dispatches with.
///
/// Note the underscore: the mantle plane spells this `/v1/data_retention` while
/// the classic control plane spells it `/data-retention`. Both are real; this
/// checks the mantle one because that is the plane ZeroRouter dispatches on.
pub const BEDROCK_RETENTION_URL_TEMPLATE: &str =
    "https://bedrock-mantle.{region}.api.aws/v1/data_retention";

/// The only mode value that backs a `zero` posture.
///
/// Note what is NOT here. The enum also has `inherit`, which is the DEFAULT for
/// a new account and means "defer to a broader scope" — so an account that has
/// never been configured answers `inherit`, not `none`, and reading anything
/// other than a literal `none` as zero-retention would turn an unconfigured
/// account into a zero-retention claim.
pub const BEDROCK_ZERO_RETENTION_MODE: &str = "none";

/// What the live check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveVerdict {
    /// The account reports the mode that backs the pin.
    Confirmed,
    /// The account reports some other mode. **The pinned label is now false**,
    /// which is a different and far more serious finding than a moved page.
    Contradicted { mode: String },
    /// The check was asked for and could not be run — no credential, no region,
    /// or the request failed. Actionable: a check that cannot run is not a check
    /// that passed, and treating it as one is how a rotated credential silently
    /// disables the only thing watching a live claim.
    Unavailable { detail: String },
}

impl LiveVerdict {
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        !matches!(self, Self::Confirmed)
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Confirmed => "CONFIRMED",
            Self::Contradicted { .. } => "CONTRADICTED",
            Self::Unavailable { .. } => "COULD NOT CHECK",
        }
    }
}

/// The account's retention mode, as the control plane reports it.
///
/// Parsed permissively on purpose. AWS's own docs disagree with themselves
/// about the shape — the user guide prints `updated_at` where the API reference
/// types `updatedAt`, and the two planes differ in casing and in whether the
/// timestamp is an epoch integer or an ISO string. None of that is the field
/// this cares about, so everything except `mode` is ignored rather than modeled;
/// a struct that insisted on the rest would fail on a documentation-accurate
/// response for reasons that have nothing to do with retention.
#[derive(Debug, Deserialize)]
struct DataRetention {
    mode: Option<String>,
}

/// Read the retention mode out of a control-plane response body.
///
/// Split from the fetch so the one piece of judgment here — what counts as an
/// answer — is testable without a network or a credential.
pub fn parse_retention_mode(document: &str) -> Result<String> {
    let parsed: DataRetention = serde_json::from_str(document)
        .context("the data-retention response was not the expected JSON")?;
    // An absent or blank `mode` is an error rather than a default. This value
    // decides whether a zero-retention label keeps being published, and the one
    // reading it must never be able to conclude "zero" from a response that did
    // not say so.
    parsed
        .mode
        .map(|mode| mode.trim().to_owned())
        .filter(|mode| !mode.is_empty())
        .context("the data-retention response carried no mode")
}

/// Compare a control-plane response against the mode a `zero` pin requires.
#[must_use]
pub fn check_live_mode(document: &str) -> LiveVerdict {
    match parse_retention_mode(document) {
        Ok(mode) if mode.eq_ignore_ascii_case(BEDROCK_ZERO_RETENTION_MODE) => {
            LiveVerdict::Confirmed
        }
        Ok(mode) => LiveVerdict::Contradicted { mode },
        Err(error) => LiveVerdict::Unavailable {
            detail: format!("{error:#}"),
        },
    }
}

/// Ask Bedrock's mantle control plane what this account's retention mode is.
///
/// `x-api-key`, not `Authorization: Bearer`. Bedrock's auth header splits by API
/// surface rather than by host: the OpenAI-compatible `/v1/*` inference routes
/// take a bearer token, while the mantle control routes — this one included —
/// take `x-api-key`. Both carry the same Bedrock API key, so there is one
/// credential and two header spellings, and getting it wrong is a 403 that reads
/// like a permissions problem.
pub async fn fetch_bedrock_retention(region: &str, credential: &str) -> Result<String> {
    let url = BEDROCK_RETENTION_URL_TEMPLATE.replace("{region}", region);
    let response = reqwest::Client::new()
        .get(&url)
        .header("x-api-key", credential)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("fetching the Bedrock retention mode from {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Bedrock retention endpoint {url} answered HTTP {status}");
    }
    response
        .text()
        .await
        .with_context(|| format!("reading the Bedrock retention body from {url}"))
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
            source_extract_anchors: Vec::new(),
            openrouter_slug: None,
        }
    }

    /// `pin`, narrowed to the regions `anchors` select.
    fn anchored(sha: &str, anchors: &[&str]) -> RetentionPin {
        RetentionPin {
            source_extract_anchors: anchors.iter().map(|a| (*a).to_owned()).collect(),
            ..pin(RetentionPosture::Standard, sha)
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

    // -----------------------------------------------------------------------
    // The bounded extract. Everything here is about one property: the check may
    // become QUIETER about text that was never evidence, and must never become
    // quieter about text that is.
    // -----------------------------------------------------------------------

    /// A docs app renders its whole site navigation into the page's visible
    /// text, so the whole-page digest moves when the vendor publishes anything
    /// at all. This is the case the mechanism was built for, and both halves
    /// are asserted together on purpose: the value of the narrowing IS the
    /// difference between them.
    #[test]
    fn nav_churn_moves_the_whole_page_digest_but_not_the_extract() {
        let with_nav = |nav: &str| {
            format!(
                "<body><nav>{nav}</nav>\
                 <p>Your data is your data. Retained for up to 30 days.</p>\
                 <footer>unrelated</footer></body>"
            )
        };
        let monday = with_nav("Docs Guides Pricing");
        let tuesday = with_nav("Docs Guides Pricing Changelog NewProduct Blog");

        // Whole-page: the two disagree, which is the false alarm being fixed.
        assert_ne!(digest(&monday), digest(&tuesday));

        // Narrowed to the sentence the posture was read from: identical.
        let narrowed = anchored(&"0".repeat(64), &["Your data is your data."]);
        let monday_evidence = evidence(&narrowed, &monday).expect("the anchor is present");
        let tuesday_evidence = evidence(&narrowed, &tuesday).expect("the anchor is present");
        assert_eq!(monday_evidence, tuesday_evidence);
        assert!(monday_evidence.contains("Retained for up to 30 days."));
        assert!(
            !monday_evidence.contains("Docs Guides Pricing"),
            "the navigation is what the extract exists to exclude: {monday_evidence}"
        );
    }

    /// The narrowing must not become a way to stop noticing an edit. Text
    /// INSIDE the window still moves the digest exactly as it always did.
    #[test]
    fn an_edit_inside_the_extract_still_moves_the_digest() {
        let narrowed = anchored(&"0".repeat(64), &["Your data is your data."]);
        let before = "<body><nav>Docs</nav><p>Your data is your data. Retained 30 days.</p></body>";
        let after = "<body><nav>Docs</nav><p>Your data is your data. Retained 90 days.</p></body>";
        assert_ne!(
            evidence(&narrowed, before).expect("present"),
            evidence(&narrowed, after).expect("present"),
            "30 days becoming 90 days is the entire point of the check"
        );
    }

    /// **The failure this mechanism lives or dies on.** An extractor that
    /// cannot find its anchor and hashes the empty string would report
    /// UNCHANGED forever against a page saying anything at all — a check that
    /// is switched off while still looking like a check. It must go red.
    #[test]
    fn a_vanished_anchor_is_page_changed_and_never_a_silent_pass() {
        let narrowed = anchored(&"0".repeat(64), &["Your data is your data."]);
        let rewritten = "<body><nav>Docs</nav><p>We now retain everything forever.</p></body>";

        let error = evidence(&narrowed, rewritten).expect_err("the anchor is gone");
        assert!(error.contains("no longer appears"), "{error}");

        let found = check("openai", &narrowed, rewritten);
        assert_eq!(found.verdict, Verdict::Changed);
        assert!(found.verdict.is_actionable());
        // And no digest is offered, so the failure cannot be "fixed" by
        // copy-pasting a number that would pin this to nothing.
        assert_eq!(found.observed_sha256, None);
        assert!(found.error.is_some());
    }

    /// The subtler half: a digest of the empty extract must not be able to
    /// match a pin. Proven by pinning the hash of the empty string and showing
    /// a page with no anchor STILL goes red.
    #[test]
    fn the_digest_of_an_empty_extract_can_never_satisfy_a_pin() {
        let empty_hash = hash("");
        let narrowed = anchored(&empty_hash, &["Your data is your data."]);
        let found = check(
            "openai",
            &narrowed,
            "<body><p>nothing familiar here</p></body>",
        );
        assert_eq!(
            found.verdict,
            Verdict::Changed,
            "a pin holding the empty digest must not be satisfiable by a failed extract"
        );
        assert_eq!(found.observed_sha256, None);
    }

    /// An anchor that has stopped being unique cannot bound anything: which
    /// region got hashed would depend on document order, and a later edit could
    /// slide the extract onto different text while the digest kept matching.
    #[test]
    fn an_ambiguous_anchor_is_page_changed_rather_than_a_guess() {
        let narrowed = anchored(&"0".repeat(64), &["Retention policy"]);
        let twice = "<body><a>Retention policy</a><p>Retention policy: 30 days.</p></body>";

        let error = evidence(&narrowed, twice).expect_err("two occurrences");
        assert!(error.contains("appears 2 times"), "{error}");
        assert_eq!(check("openai", &narrowed, twice).verdict, Verdict::Changed);
    }

    /// Case-sensitivity, and the real page that forced the decision. Vertex's
    /// heading `Training restriction` sits directly above a quotation of the
    /// contractual term `"Training Restriction"`; matched case-insensitively
    /// the anchor is ambiguous and the pin reddens on an unedited page.
    #[test]
    fn anchors_match_case_sensitively_so_a_quoted_term_is_not_the_heading() {
        let page = "<body><h2>Training restriction</h2>\
                    <p>As outlined in \"Training Restriction\" in the Service Terms, \
                    Google won't use your data to train.</p></body>";
        let narrowed = anchored(&"0".repeat(64), &["Training restriction"]);
        let evidence = evidence(&narrowed, page).expect("exactly one case-sensitive match");
        assert!(evidence.starts_with("Training restriction"), "{evidence}");
    }

    /// Several anchors, because a page's retention facts are rarely contiguous
    /// — Vertex states training, abuse-monitoring, request-response logging and
    /// caching in four places thousands of characters apart.
    #[test]
    fn several_anchors_each_contribute_their_own_region() {
        let page = "<body><nav>lots of nav</nav>\
                    <p>Training restriction applies.</p>\
                    <p>filler</p>\
                    <p>In-memory data caching has a 24-hour TTL.</p></body>";
        let narrowed = anchored(
            &"0".repeat(64),
            &["Training restriction", "In-memory data caching"],
        );
        let evidence = evidence(&narrowed, page).expect("both anchors present");
        assert!(
            evidence.contains("Training restriction applies."),
            "{evidence}"
        );
        assert!(evidence.contains("24-hour TTL"), "{evidence}");
        assert!(!evidence.contains("lots of nav"), "{evidence}");
    }

    /// One missing anchor out of several is still a failure. A partial extract
    /// would quietly narrow the evidence further than the author declared.
    #[test]
    fn one_missing_anchor_fails_the_whole_extract() {
        let narrowed = anchored(&"0".repeat(64), &["present here", "gone from the page"]);
        let error = evidence(&narrowed, "<body><p>present here only</p></body>")
            .expect_err("the second anchor is absent");
        assert!(error.contains("gone from the page"), "{error}");
    }

    /// A pin that declares no anchors keeps hashing the whole page, exactly as
    /// every pin did before the field existed. Pinned so that adding the
    /// mechanism cannot quietly change the pins that never opted in.
    #[test]
    fn a_pin_without_anchors_still_hashes_the_whole_page() {
        let page = "<body><nav>Docs</nav><p>We retain inputs for 30 days.</p></body>";
        let whole = pin(RetentionPosture::Standard, &digest(page));
        assert!(whole.source_extract_anchors.is_empty());
        assert_eq!(evidence(&whole, page).expect("no anchors"), normalize(page));
        assert_eq!(check("anthropic", &whole, page).verdict, Verdict::Unchanged);
    }

    /// The window is bounded, and bounded in CHARACTERS — these are marketing
    /// pages, and slicing a `str` at a byte offset landing mid-codepoint
    /// panics. The same class of bug is recorded in `normalize`'s own comment.
    #[test]
    fn the_window_is_bounded_and_counts_characters_not_bytes() {
        let long = format!("<body><p>ANCHOR {}</p></body>", "é".repeat(4_000));
        let narrowed = anchored(&"0".repeat(64), &["ANCHOR"]);
        let evidence = evidence(&narrowed, &long).expect("the anchor is present");
        assert_eq!(evidence.chars().count(), EXTRACT_WINDOW_CHARS);
        // Text beyond the window is excluded, which is what makes the extract
        // bounded rather than "the rest of the page".
        assert!(evidence.chars().count() < normalize(&long).chars().count());
    }

    /// The extract runs FORWARD only. Anything above the anchor — which is
    /// where navigation renders — can change without limit.
    #[test]
    fn text_before_the_anchor_never_reaches_the_extract() {
        let narrowed = anchored(&"0".repeat(64), &["ANCHOR"]);
        let a = evidence(
            &narrowed,
            "<body><p>alpha beta gamma ANCHOR evidence</p></body>",
        );
        let b = evidence(
            &narrowed,
            "<body><p>totally different preamble ANCHOR evidence</p></body>",
        );
        assert_eq!(a.expect("present"), b.expect("present"));
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

    // -----------------------------------------------------------------------
    // The live Bedrock check. The judgement here is entirely in what counts as
    // an answer, which is why the parse is split from the fetch.
    // -----------------------------------------------------------------------

    #[test]
    fn only_a_literal_none_confirms_the_zero_posture() {
        // AWS's documented PUT response shape, with the mode that backs the pin.
        assert_eq!(
            check_live_mode(r#"{"mode":"none","updated_at":1733529600}"#),
            LiveVerdict::Confirmed
        );
        // Casing is the wire's business, not a reason to fail a true answer.
        assert_eq!(
            check_live_mode(r#"{"mode":"NONE"}"#),
            LiveVerdict::Confirmed
        );
    }

    #[test]
    fn every_other_mode_contradicts_the_published_label() {
        // `inherit` is the one to get right, and it is the DEFAULT for a new
        // account: it means "no opinion at this scope", not "retains nothing".
        // Reading it as zero would let an account nobody ever configured
        // publish a zero-retention claim to customers.
        for mode in ["default", "provider_data_share", "inherit"] {
            let verdict = check_live_mode(&format!(r#"{{"mode":"{mode}"}}"#));
            assert_eq!(
                verdict,
                LiveVerdict::Contradicted {
                    mode: mode.to_owned()
                },
                "{mode} must not read as zero retention"
            );
            assert!(verdict.is_actionable());
        }
    }

    #[test]
    fn a_response_that_states_no_mode_is_unverified_never_confirmed() {
        // The failure direction that matters. A body this cannot read must
        // never resolve to "zero" — the value decides whether a legal-adjacent
        // claim keeps being published, so silence has to be an error.
        for body in ["{}", r#"{"mode":null}"#, r#"{"mode":"  "}"#, "not json", ""] {
            let verdict = check_live_mode(body);
            assert!(
                matches!(verdict, LiveVerdict::Unavailable { .. }),
                "{body:?} must not resolve to a verdict about the account"
            );
            assert!(verdict.is_actionable());
        }
    }

    #[test]
    fn the_retention_url_is_the_mantle_plane_and_interpolates_its_region() {
        // Two spellings exist and they are not interchangeable: the mantle
        // plane serves `/v1/data_retention` with an underscore, the classic
        // control plane `/data-retention` with a hyphen. ZeroRouter dispatches
        // on mantle, so mantle is the plane whose setting governs its traffic.
        let url = BEDROCK_RETENTION_URL_TEMPLATE.replace("{region}", "us-east-1");
        assert_eq!(
            url,
            "https://bedrock-mantle.us-east-1.api.aws/v1/data_retention"
        );
        assert!(!url.contains("{region}"), "{url}");
    }
}
