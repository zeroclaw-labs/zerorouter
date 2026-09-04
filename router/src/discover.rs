//! Model-currency discovery: which models does the upstream world carry that
//! `tiers.toml` does not?
//!
//! [`crate::drift`] answers the reconciliation question in one direction — for
//! every lane WE carry, does the source still price it the way we recorded? —
//! and it exists because staleness there costs money silently. This module
//! answers the OTHER direction, and the cost it addresses is opportunity rather
//! than margin: a provider ships a new model, bumps a version (glm-5.2 →
//! glm-5.3), or lights the same model up on a second cloud we already dispatch
//! to (grok appears on Bedrock), and nobody notices until an operator stumbles
//! on it by hand. It reads the SAME parsed models.dev document `catalog-drift`
//! reads and diffs it against the catalog the other way round.
//!
//! **It reports; it never writes.** For the same reason `drift` never writes a
//! price, only more so: ADDING a lane is not a mechanical diff. It needs a
//! transcribed price, a retention basis, and a judgement — is this a hybrid
//! twin? is `mode:none` available on this account? is the published price the
//! one we will actually be invoiced? None of that is safe to automate, so the
//! output is a list of CANDIDATES a human turns into a pull request.
//!
//! Three questions are asked of every model the source lists under a provider
//! this deployment is credentialed for and that no catalog candidate already
//! carries. They are mutually exclusive, decided in the order below, because a
//! model is most usefully described by the strongest thing true of it:
//!
//! 1. [`Category::CrossCloud`] — the same model, by its bare name, is one we
//!    already carry on a DIFFERENT credentialed provider. This is the
//!    highest-value find: it is almost always a zero-retention-twin
//!    opportunity (the model we serve on a retaining cloud, available on one
//!    that does not retain), and it is checked first so a twin is never
//!    mislabelled as merely new.
//! 2. [`Category::VersionBump`] — we carry an EARLIER version in the same
//!    family under this same provider, and the source lists a higher one we do
//!    not. This is the glm-5.2 → glm-5.3 case.
//! 3. [`Category::New`] — everything else: a model under a provider we can
//!    dispatch on that we simply do not carry.
//!
//! Every heuristic here is tuned in ONE direction on purpose. A false New is
//! cheap — a human glances at a row and moves on — while a MISSED new model is
//! the whole failure this job exists to prevent, so wherever a judgement is
//! uncertain (is this a version bump? is this really a chat model?) the code
//! falls toward reporting, and toward the weaker category. A version comparison
//! it cannot make confidently degrades to New, never to silence.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::config::TierCatalog;

/// The default recency window for the `New` category: a model whose
/// `release_date` is older than this many days is dropped from New. Chosen so
/// the weekly report stays scannable — see [`filter_new_by_recency`] for why
/// only New is filtered and why an undated model is kept regardless.
pub const DEFAULT_NEW_SINCE_DAYS: i64 = 90;

/// Which of the three discovery questions a candidate answers. Ordered by the
/// precedence [`discover`] applies, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// A model we carry on another credentialed provider, offered here too.
    CrossCloud,
    /// A higher version of a family we already carry on this provider.
    VersionBump,
    /// A model under a provider we can dispatch on that we do not carry.
    New,
}

impl Category {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CrossCloud => "CROSS-CLOUD",
            Self::VersionBump => "VERSION BUMP",
            Self::New => "NEW",
        }
    }
}

/// One thing worth a human's attention: a model the catalog does not carry.
///
/// Deliberately the minimum a reviewer needs to decide whether to open a pull
/// request — the category, the provider whose account would serve it, the
/// source's own model string (so it can be pasted into a lane), and a sentence
/// of why it surfaced. No prices: discovery is about EXISTENCE, and a price
/// here would invite someone to trust a number this module took no care to
/// reconcile. That is `catalog-drift`'s job, on a lane a human has chosen to
/// add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub category: Category,
    /// The ZeroRouter provider key whose credential would dispatch this — the
    /// key an operator writes in `tiers.toml`, never the models.dev key.
    pub provider: String,
    /// The model string exactly as the source lists it. Left raw rather than
    /// canonicalized so a reviewer sees what the vendor actually calls it.
    pub model: String,
    pub note: String,
}

/// The subset of one models.dev model this module reads. The source carries far
/// more; only the modality output is consulted, and only to filter obvious
/// non-chat noise (see [`is_non_chat`]).
#[derive(Debug, Deserialize)]
struct SourceModel {
    #[serde(default)]
    modalities: Option<SourceModalities>,
    /// When the model shipped, used ONLY to age out stale `New` findings (see
    /// [`filter_new_by_recency`]). The source spells it either as a full date
    /// (`2026-08-14`) or a year-month (`2026-01`); absent or unparseable is a
    /// model whose age is unknown, and an unknown age is never a reason to drop
    /// a candidate.
    #[serde(default)]
    release_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SourceModalities {
    /// What the model PRODUCES. A generator (image, audio, video) lists no
    /// `text` here; a chat model does. `None` means the source said nothing,
    /// which is treated as "could be chat" — silence never filters.
    #[serde(default)]
    output: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SourceProvider {
    #[serde(default)]
    models: BTreeMap<String, SourceModel>,
}

/// Providers whose models.dev key cannot be read from the inventory and must be
/// supplied for DISCOVERY ONLY — never for pricing.
///
/// `bedrock` is the whole reason this exists, and the split is deliberate.
/// [`crate::providers::provider_source_key`] returns `None` for it on purpose:
/// models.dev files Bedrock under `amazon-bedrock` and prices the bare model
/// ids at a SKU class ZeroRouter does not dial, so joining the two for
/// reconciliation would report a green margin over a basis ~10% under the real
/// invoice — the exact silent-margin failure `drift` refuses to risk. That
/// argument is about PRICE. Discovery asks a different question — does the
/// source merely LIST a model here? — and the answer is safe: no number is
/// trusted, only the fact that `amazon-bedrock` carries `xai.grok-4.6` at all.
///
/// So the mapping lives here, beside the code that only ever asks the existence
/// question, rather than in the shared inventory where a future reader might
/// reuse it for a price join and reopen the failure `provider_source_key` was
/// shaped to avoid. It is consulted only after the inventory's own declared key
/// and the identity key both fail to appear in the source (see
/// [`resolve_source_keys`]), so it can never override a provider that already
/// resolves.
const DISCOVERY_ONLY_SOURCE_KEYS: &[(&str, &str)] = &[("bedrock", "amazon-bedrock")];

/// Bare model tokens that name a class the source does not distinguish by
/// modality but that is never a chat model. See [`is_non_chat`].
///
/// Kept short and unambiguous on purpose. The cost of a token here that also
/// appears in a real chat name is a MISSED chat model — the one failure this
/// job exists to prevent — so a token earns its place only if no chat model
/// would ever carry it. `guard` covers Llama Guard and `gpt-oss-safeguard`;
/// `embed` covers `text-embedding-*` and `gemini-embedding-*`; `whisper` and
/// `tts` are speech, not chat.
const NON_CHAT_NAME_TOKENS: &[&str] = &["embed", "rerank", "moderation", "guard", "whisper", "tts"];

/// Whether a source model is obviously not a chat model, and so noise in a
/// report about chat lanes.
///
/// Two signals, both conservative — each errs toward KEEPING a model, because a
/// false "new chat model" wastes a glance while a dropped one defeats the job:
///
/// - **Modality.** A model whose declared output modalities exist and do not
///   include `text` produces images, audio, or video, not a chat completion.
///   Silence (no modalities at all) is never a filter — it is treated as "might
///   be chat".
/// - **Name.** Embeddings, rerankers, moderation/guard classifiers, and speech
///   models all output `text` in the source and so slip past the modality
///   check, but they are unmistakable by name ([`NON_CHAT_NAME_TOKENS`]).
fn is_non_chat(model: &str, entry: &SourceModel) -> bool {
    if let Some(output) = entry
        .modalities
        .as_ref()
        .and_then(|modalities| modalities.output.as_ref())
        && !output.is_empty()
        && !output.iter().any(|modality| modality == "text")
    {
        return true;
    }
    let lowered = model.to_ascii_lowercase();
    NON_CHAT_NAME_TOKENS
        .iter()
        .any(|token| lowered.contains(token))
}

/// A model string reduced to the comparable bare name two catalogs can be
/// checked against, whatever spelling conventions each vendor wraps it in.
///
/// The transforms, applied in order, and why each is needed by a real lane:
///
/// - **Last path segment.** Fireworks ships `accounts/fireworks/models/kimi-k3`
///   and Together ships `moonshotai/Kimi-K2.7-Code`; the routing prefix is not
///   part of the model's identity.
/// - **Lowercase.** Together writes `Qwen/Qwen3.6-Plus`, Fireworks writes
///   lowercase; the same model must compare equal across them.
/// - **Region and vendor dotted prefixes.** Bedrock writes
///   `us.anthropic.claude-sonnet-5` and `xai.grok-4.6`; the region a request is
///   billed in and the vendor namespace are not the model.
/// - **`@`-pinned version and trailing SKU markers.** Vertex writes
///   `claude-sonnet-5@default`, Bedrock `...-v1:0`, Vertex again `...-maas`;
///   these name a deployment of the model, not a different model.
/// - **Trailing 8-digit date.** Anthropic dispatches
///   `claude-haiku-4-5-20251001` while the source files it undated — the same
///   collapse [`crate::drift`]'s `undated` helper makes, for the same reason.
///
/// It is intentionally lossy: `claude-sonnet-5` and `us.anthropic.claude-sonnet-5`
/// collapse to one token so a model we serve on one cloud is recognised on
/// another, and so a provider's five regional spellings of one model do not
/// each become a separate finding.
fn canonical(model: &str) -> String {
    let mut bare = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();

    // Region prefixes AWS prepends to a geographic inference profile, then the
    // vendor namespace Bedrock files a model under. Stripped as a leading
    // dotted token so `us.anthropic.claude-sonnet-5` reduces to the model.
    for region in ["us.", "eu.", "au.", "apac.", "global."] {
        if let Some(rest) = bare.strip_prefix(region) {
            bare = rest.to_owned();
            break;
        }
    }
    for vendor in [
        "anthropic.",
        "xai.",
        "amazon.",
        "deepseek.",
        "meta.",
        "mistral.",
        "cohere.",
        "ai21.",
        "qwen.",
        "google.",
    ] {
        if let Some(rest) = bare.strip_prefix(vendor) {
            bare = rest.to_owned();
            break;
        }
    }

    // A Vertex `@`-pinned version (`claude-sonnet-5@default`) names a
    // deployment, not a distinct model.
    if let Some((head, _)) = bare.split_once('@') {
        bare = head.to_owned();
    }

    // Trailing SKU / deployment markers, longest first so `-v1:0` is tried
    // before `:0`.
    for marker in ["-v1:0", "-v1", ":0", "-maas", "-tput"] {
        if let Some(rest) = bare.strip_suffix(marker) {
            bare = rest.to_owned();
            break;
        }
    }

    // A trailing `-YYYYMMDD` snapshot date, exactly the shape `drift::undated`
    // strips: eight digits, so a four-digit build tag like `-0813` is left
    // alone.
    if let Some((head, tail)) = bare.rsplit_once('-')
        && tail.len() == 8
        && tail.bytes().all(|byte| byte.is_ascii_digit())
    {
        bare = head.to_owned();
    }

    // Fireworks encodes version dots as `p` (`glm-5p3`, `qwen3p8-max`) because
    // its ids live in URL paths. Fold digit-`p`-digit to the dot spelling so
    // one model canonicalizes the same everywhere — without this, a `glm-5.3`
    // appearing on a second provider never matches the fireworks lane and the
    // cross-cloud scan is blind to the highest-value find it exists for.
    // Digit-bounded on both sides, so names like `phi-4` are untouched.
    let bytes = bare.as_bytes();
    let mut folded = String::with_capacity(bare.len());
    for (index, &byte) in bytes.iter().enumerate() {
        let dotted = byte == b'p'
            && index > 0
            && bytes[index - 1].is_ascii_digit()
            && bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
        folded.push(if dotted { '.' } else { byte as char });
    }

    folded
}

/// Parse a models.dev `release_date`, or `None` when it is absent or in a shape
/// this does not recognise.
///
/// The source uses two spellings: a full `YYYY-MM-DD` and a `YYYY-MM`
/// year-month, the latter treated as the FIRST of that month — the most recent
/// interpretation, which keeps a coarsely-dated model in the window a day
/// longer rather than a day less, the conservative direction. `None` on
/// anything else is deliberate: the caller keeps an undated model, so a parse
/// that fails must look the same as an absent date, never drop the row.
fn parse_release_date(raw: &str) -> Option<NaiveDate> {
    let raw = raw.trim();
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .or_else(|| NaiveDate::parse_from_str(&format!("{raw}-01"), "%Y-%m-%d").ok())
}

/// A bare name split into the fixed family around a single release version, or
/// `None` when it carries no version this module is willing to compare.
///
/// The family is `(prefix, suffix)` — everything before and after the FIRST
/// version run — and two models are the same family iff both halves match
/// exactly. The version is that run parsed to integer components, so `5.2`,
/// `5p2` (Fireworks spells the dot `p`), and `4` all compare as numbers.
///
/// The one trap this guards is the size suffix. `gpt-oss-120b` and
/// `gpt-oss-20b` are different PARAMETER SIZES, not versions, and a naive first-
/// number split would call `120b` a bump over `20b`. So a numeric run
/// immediately followed by a size unit (`b`, `m`, `k`) is not treated as a
/// version at all — the function returns `None`, and the model falls through to
/// New rather than fabricating a version relationship. That is the conservative
/// direction: an unrecognised shape is reported as New, never silently dropped
/// and never mislabelled a bump.
fn split_version(bare: &str) -> Option<(String, Vec<u64>, String)> {
    let bytes = bare.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        // A maximal run of digits and interior `.`/`p` separators: `3`, `5.2`,
        // `2p6`. A separator only extends the run when a digit follows it.
        let mut end = index;
        while end < bytes.len() {
            let byte = bytes[end];
            let extends_run = byte.is_ascii_digit()
                || (matches!(byte, b'.' | b'p')
                    && end + 1 < bytes.len()
                    && bytes[end + 1].is_ascii_digit());
            if !extends_run {
                break;
            }
            end += 1;
        }
        // A size unit fused to the number (`120b`, `550m`) is a size, not a
        // version. Reject the whole name rather than guess.
        if end < bytes.len() && matches!(bytes[end], b'b' | b'm' | b'k') {
            let after = end + 1;
            let boundary = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
            if boundary {
                return None;
            }
        }
        let version: Vec<u64> = bare[start..end]
            .split(['.', 'p'])
            .filter_map(|part| part.parse().ok())
            .collect();
        if version.is_empty() {
            return None;
        }
        return Some((bare[..start].to_owned(), version, bare[end..].to_owned()));
    }
    None
}

/// What the catalog already carries, indexed the three ways the diff needs it.
struct Carried {
    /// Per provider, the canonical names carried on it — the "do we already
    /// have this exact model here?" question.
    by_provider: BTreeMap<String, BTreeSet<String>>,
    /// Every provider a canonical name is carried on — the cross-cloud lookup.
    providers_by_model: BTreeMap<String, BTreeSet<String>>,
    /// Per provider, the highest version seen in each `(prefix, suffix)`
    /// family — the version-bump comparison baseline.
    max_version: BTreeMap<(String, String, String), Vec<u64>>,
}

impl Carried {
    fn from_catalog(catalog: &TierCatalog) -> Self {
        let mut by_provider: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut providers_by_model: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut max_version: BTreeMap<(String, String, String), Vec<u64>> = BTreeMap::new();

        // Withheld tiers count as carried: a tier withheld for below-cost
        // pricing is still a lane in the file, and reporting its model as a
        // brand-new discovery would be wrong. Mirrors `drift::reconcile`.
        let definitions = catalog.tiers.values().chain(
            catalog
                .unavailable
                .values()
                .map(|withheld| &withheld.definition),
        );

        for definition in definitions {
            for candidate in &definition.candidates {
                let name = canonical(&candidate.model);
                by_provider
                    .entry(candidate.provider.clone())
                    .or_default()
                    .insert(name.clone());
                providers_by_model
                    .entry(name.clone())
                    .or_default()
                    .insert(candidate.provider.clone());
                if let Some((prefix, version, suffix)) = split_version(&name) {
                    let key = (candidate.provider.clone(), prefix, suffix);
                    let slot = max_version.entry(key).or_default();
                    if version > *slot {
                        *slot = version;
                    }
                }
            }
        }

        Self {
            by_provider,
            providers_by_model,
            max_version,
        }
    }
}

/// Discover the models the source lists that the catalog does not carry.
///
/// The testable core: `credentialed` is `(our provider key, models.dev key)`
/// for every provider a caller has decided this deployment can dispatch on,
/// supplied rather than read from the process-wide inventory so a test can pin
/// exactly which providers exist without installing a global. See
/// [`discover_from_inventory`] for the production wiring, and
/// [`crate::drift::reconcile_with`] for the same seam pattern.
///
/// `today` and `new_since_days` bound the `New` category only — see
/// [`filter_new_by_recency`]. `today` is a parameter rather than a call to the
/// clock so the recency window is deterministic under test.
#[must_use]
pub fn discover(
    catalog: &TierCatalog,
    source: &str,
    credentialed: &[(String, String)],
    today: NaiveDate,
    new_since_days: i64,
) -> Vec<Candidate> {
    let providers: BTreeMap<String, SourceProvider> =
        serde_json::from_str(source).unwrap_or_default();
    let carried = Carried::from_catalog(catalog);

    // Each candidate carries its parsed release date alongside, for the recency
    // pass below. The date never reaches the public [`Candidate`] — it is a
    // sort/filter key, not part of the report's contract.
    let mut scored: Vec<(Candidate, Option<NaiveDate>)> = Vec::new();
    // One finding per (provider, canonical model): a provider's five regional
    // spellings of one model, or its dated and undated forms, are one
    // opportunity, not five rows.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for (provider, source_key) in credentialed {
        let Some(source_provider) = providers.get(source_key) else {
            continue;
        };
        let carried_here = carried.by_provider.get(provider);

        for (model, entry) in &source_provider.models {
            let name = canonical(model);

            // Already ours on this provider, in any spelling — nothing to find.
            if carried_here.is_some_and(|names| names.contains(&name)) {
                continue;
            }
            if is_non_chat(model, entry) {
                continue;
            }
            if !seen.insert((provider.clone(), name.clone())) {
                continue;
            }

            let release = entry.release_date.as_deref().and_then(parse_release_date);
            let candidate = classify(provider, model, &name, &carried, release);
            scored.push((candidate, release));
        }
    }

    filter_new_by_recency(scored, today, new_since_days)
}

/// Age out stale `New` findings, and only those.
///
/// `New` is the firehose: every model under a credentialed provider we do not
/// carry, which on the live catalog is hundreds of rows going back to
/// `gpt-3.5-turbo`. A weekly report nobody can read is a report nobody reads,
/// so a `New` model whose `release_date` is older than `new_since_days` before
/// `today` is dropped, and the survivors are sorted newest-first.
///
/// The rule is applied to `New` and to nothing else, on purpose:
/// [`Category::CrossCloud`] and [`Category::VersionBump`] are actionable at any
/// age — a twin of a lane we run, or a higher version of a family we carry, is
/// worth acting on whether the model shipped last week or last year — so they
/// pass through untouched, in their original order.
///
/// A model with no parseable `release_date` is KEPT, and placed after every
/// dated survivor. Dropping it would be guessing a model is old because the
/// source did not say how old it is, which is exactly the missed-new-model
/// failure the whole job exists to prevent — the same reason an uncomparable
/// version degrades to `New` rather than vanishing.
fn filter_new_by_recency(
    scored: Vec<(Candidate, Option<NaiveDate>)>,
    today: NaiveDate,
    new_since_days: i64,
) -> Vec<Candidate> {
    let cutoff = today - chrono::Duration::days(new_since_days.max(0));

    let (new, other): (Vec<_>, Vec<_>) = scored
        .into_iter()
        .partition(|(candidate, _)| candidate.category == Category::New);

    let mut fresh_new: Vec<(Candidate, Option<NaiveDate>)> = new
        .into_iter()
        // Keep an undated model (release is `None`), and any dated model on or
        // after the cutoff. `>=` makes the boundary inclusive.
        .filter(|(_, release)| release.is_none_or(|date| date >= cutoff))
        .collect();
    // Newest first. `Reverse` flips the ordering so a later date sorts earlier;
    // `None` is the smallest `Option`, so reversed it is the largest and undated
    // rows land last. `sort_by_key` is stable, leaving same-date rows in their
    // prior (name-sorted) order.
    fresh_new.sort_by_key(|(_, release)| std::cmp::Reverse(*release));

    // Cross-cloud and version bumps first, in the order they were found, then
    // the recency-ranked new models.
    other
        .into_iter()
        .chain(fresh_new)
        .map(|(candidate, _)| candidate)
        .collect()
}

/// Decide which category one uncarried source model falls into, strongest
/// first — the precedence documented on [`Category`].
///
/// `release` is threaded through only to date the `New` note; the categories
/// themselves do not depend on when a model shipped.
fn classify(
    provider: &str,
    model: &str,
    name: &str,
    carried: &Carried,
    release: Option<NaiveDate>,
) -> Candidate {
    // Cross-cloud: this exact model is carried on some OTHER provider. Checked
    // first because a twin is the most valuable thing it could be.
    if let Some(providers) = carried.providers_by_model.get(name) {
        let elsewhere: Vec<&str> = providers
            .iter()
            .filter(|carried_on| carried_on.as_str() != provider)
            .map(String::as_str)
            .collect();
        if !elsewhere.is_empty() {
            return Candidate {
                category: Category::CrossCloud,
                provider: provider.to_owned(),
                model: model.to_owned(),
                note: format!(
                    "carried on {}; the source also lists it under our {provider} lane — likely a \
                     zero-retention-twin opportunity",
                    elsewhere.join(", ")
                ),
            };
        }
    }

    // Version bump: we carry an earlier version of the same family here. Only
    // a source version strictly above the HIGHEST we carry counts — a version
    // we have already passed (we hold 4.6, the source lists 4.5) is not a bump
    // and degrades to New below.
    if let Some((prefix, version, suffix)) = split_version(name) {
        let key = (provider.to_owned(), prefix, suffix);
        if let Some(highest) = carried.max_version.get(&key)
            && version > *highest
        {
            return Candidate {
                category: Category::VersionBump,
                provider: provider.to_owned(),
                model: model.to_owned(),
                note: format!(
                    "we carry {} in this family; the source lists a higher {}",
                    render_version(highest),
                    render_version(&version)
                ),
            };
        }
    }

    // The release date rides in the note so a newest-first list reads as one,
    // and an undated model says so rather than looking like a missing field.
    let shipped = release.map_or_else(
        || "release date unknown".to_owned(),
        |date| format!("released {date}"),
    );
    Candidate {
        category: Category::New,
        provider: provider.to_owned(),
        model: model.to_owned(),
        note: format!("{shipped}; no catalog candidate carries this model"),
    }
}

/// A parsed version rendered back for the note, dot-joined (`[5, 2]` → `5.2`).
fn render_version(version: &[u64]) -> String {
    version
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// [`discover`], reading the credentialed-provider set from the process
/// inventory the router itself dispatches from.
///
/// For each provider in the inventory it resolves the ONE models.dev key to
/// scan, trying in order: the key the inventory declares
/// ([`crate::providers::provider_source_key`]), then a discovery-only override
/// ([`DISCOVERY_ONLY_SOURCE_KEYS`]), then the provider key itself. The first
/// candidate the source actually contains wins; a provider none of whose
/// candidate keys appear in the source is skipped, because the source cannot
/// tell us anything about a vendor it does not cover.
///
/// `today` (the caller's clock) and `new_since_days` bound the `New` category,
/// as in [`discover`].
#[must_use]
pub fn discover_from_inventory(
    catalog: &TierCatalog,
    source: &str,
    today: NaiveDate,
    new_since_days: i64,
) -> Vec<Candidate> {
    let inventory = crate::providers::inventory_source_keys();
    let credentialed = resolve_source_keys(source, &inventory);
    discover(catalog, source, &credentialed, today, new_since_days)
}

/// Resolve each inventory provider to the single models.dev key present in the
/// source, or drop it. Split out from [`discover_from_inventory`] so the
/// resolution rule is testable without a live document.
fn resolve_source_keys(
    source: &str,
    inventory: &[(String, Option<String>)],
) -> Vec<(String, String)> {
    let present: BTreeSet<String> =
        serde_json::from_str::<BTreeMap<String, serde::de::IgnoredAny>>(source)
            .map(|map| map.into_keys().collect())
            .unwrap_or_default();

    inventory
        .iter()
        .filter_map(|(provider, declared)| {
            let override_key = DISCOVERY_ONLY_SOURCE_KEYS
                .iter()
                .find(|(name, _)| *name == provider)
                .map(|(_, key)| (*key).to_owned());
            declared
                .clone()
                .into_iter()
                .chain(override_key)
                .chain(std::iter::once(provider.clone()))
                .find(|key| present.contains(key))
                .map(|key| (provider.clone(), key))
        })
        .collect()
}

/// models.dev keys that are themselves routers/aggregators rather than
/// first-party inference providers. ZeroRouter routing through another router
/// stacks margin and makes the retention story opaque (the aggregator's
/// upstream choice, not ours), so these are the wrong upstream leads.
///
/// TAGGED, NOT HIDDEN — the same philosophy as `is_non_chat`'s blocklist:
/// a wrong entry here costs a mislabelled row the reader can see, never a
/// silently missing one. The list is advisory; a human reading the report
/// decides. Keys observed in the live source on 2026-09-04, plus the two
/// gateways too well-known to omit.
const KNOWN_AGGREGATORS: &[&str] = &[
    "empiriolabs",
    "kilo",
    "llmgateway",
    "llmgateway-providers",
    "nano-gpt",
    "openrouter",
    "orcarouter",
    "pioneer",
    "vercel",
];

/// One uncredentialed provider that serves models the catalog already carries —
/// an upstream LEAD, ranked by how much of our catalog it could also serve.
///
/// This is the opposite question from [`Candidate`]: discovery asks "what do
/// our providers serve that we don't carry?"; this asks "who else serves what
/// we DO carry?". The answer feeds provider outreach — a second source for a
/// lane is price competition, redundancy, and sometimes a better retention
/// story — and, like every discovery product, it is a lead and never an action:
/// adding a provider needs wire support, a credential, a transcribed price
/// list, and a retention basis a human must supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCandidate {
    /// The models.dev provider key, exactly as the source spells it.
    pub key: String,
    /// How many of our carried models this provider also serves (distinct
    /// canonical names — five regional spellings of one model count once).
    pub overlap: usize,
    /// Up to five of the overlapping canonical names, sorted, so the report
    /// says WHICH lanes a second source exists for without flooding the row.
    pub overlap_examples: Vec<String>,
    /// Distinct canonical chat models the provider serves in total.
    pub chat_models: usize,
    /// Listed in [`KNOWN_AGGREGATORS`] — a router, not a first-party upstream.
    pub aggregator: bool,
    /// How many DISTINCT closed-frontier vendor families (claude, non-oss gpt,
    /// gemini, grok) the provider serves. No first-party host carries two
    /// competing labs' closed models — only aggregators and hyperscaler
    /// clouds do — so 2+ is the computed aggregator signal the const list
    /// cannot keep up with, while 0 marks the open-weight GPU hosts that are
    /// the prime outreach targets.
    pub frontier_families: usize,
}

/// The provider scan's whole result. `zero_overlap_dropped` exists so the
/// report can say how many providers it declined to list rather than
/// truncating silently — a provider serving nothing we carry is not an
/// outreach lead, but "37 others had no overlap" is one honest line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderReport {
    pub candidates: Vec<ProviderCandidate>,
    pub zero_overlap_dropped: usize,
}

/// Scan the source for uncredentialed providers, ranked by catalog overlap.
///
/// Sort order is the read order: first-party providers before aggregators,
/// then overlap descending (the outreach signal), then breadth, then key for
/// determinism.
pub fn discover_providers(
    catalog: &TierCatalog,
    source: &str,
    credentialed: &[(String, String)],
) -> ProviderReport {
    let providers: BTreeMap<String, SourceProvider> =
        serde_json::from_str(source).unwrap_or_default();
    let carried = Carried::from_catalog(catalog);

    let credentialed_keys: BTreeSet<&str> = credentialed
        .iter()
        .map(|(_, source_key)| source_key.as_str())
        .collect();

    let mut candidates = Vec::new();
    let mut zero_overlap_dropped = 0usize;

    for (key, source_provider) in &providers {
        if credentialed_keys.contains(key.as_str()) {
            continue;
        }

        let mut chat: BTreeSet<String> = BTreeSet::new();
        for (model, entry) in &source_provider.models {
            if is_non_chat(model, entry) {
                continue;
            }
            chat.insert(canonical(model));
        }

        let overlapping: Vec<&String> = chat
            .iter()
            .filter(|name| carried.providers_by_model.contains_key(*name))
            .collect();
        if overlapping.is_empty() {
            // A provider serving nothing we carry may still matter one day,
            // but it is not an outreach lead; counted, not listed.
            if !chat.is_empty() {
                zero_overlap_dropped += 1;
            }
            continue;
        }

        let frontier_families = [
            |name: &str| name.starts_with("claude"),
            |name: &str| name.starts_with("gpt-") && !name.starts_with("gpt-oss"),
            |name: &str| name.starts_with("gemini"),
            |name: &str| name.starts_with("grok"),
        ]
        .iter()
        .filter(|family| chat.iter().any(|name| family(name)))
        .count();

        candidates.push(ProviderCandidate {
            key: key.clone(),
            overlap: overlapping.len(),
            overlap_examples: overlapping.iter().take(5).map(|s| (*s).clone()).collect(),
            chat_models: chat.len(),
            aggregator: KNOWN_AGGREGATORS.contains(&key.as_str()),
            frontier_families,
        });
    }

    // Read order = outreach order: const-tagged aggregators last of all, then
    // multi-frontier resellers (aggregator or hyperscaler — a human tells them
    // apart), and the open-weight hosts that are the actionable leads first;
    // overlap breaks ties.
    candidates.sort_by(|a, b| {
        a.aggregator
            .cmp(&b.aggregator)
            .then((a.frontier_families >= 2).cmp(&(b.frontier_families >= 2)))
            .then(b.overlap.cmp(&a.overlap))
            .then(b.chat_models.cmp(&a.chat_models))
            .then(a.key.cmp(&b.key))
    });

    ProviderReport {
        candidates,
        zero_overlap_dropped,
    }
}

/// [`discover_providers`] against the shipped provider inventory — the same
/// resolution [`discover_from_inventory`] uses, so the two scans agree on what
/// "credentialed" means.
pub fn discover_providers_from_inventory(catalog: &TierCatalog, source: &str) -> ProviderReport {
    let inventory = crate::providers::inventory_source_keys();
    let credentialed = resolve_source_keys(source, &inventory);
    discover_providers(catalog, source, &credentialed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelMetadata, TierCandidate, TierDefinition};
    use crate::provider::{ModelRates, RateSchedule};

    fn flat(input: f64, output: f64) -> RateSchedule {
        ModelRates {
            input_per_mtok: Some(input),
            output_per_mtok: Some(output),
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
        }
        .into()
    }

    /// A catalog carrying the given `(provider, model)` candidates. Rates and
    /// metadata are irrelevant to discovery — it asks only what exists — so
    /// they are filled with a nominal value.
    fn catalog(candidates: &[(&str, &str)]) -> TierCatalog {
        let mut tiers = BTreeMap::new();
        for (index, (provider, model)) in candidates.iter().enumerate() {
            tiers.insert(
                format!("zero/tier-{index}"),
                TierDefinition {
                    rates: flat(1.0, 1.0),
                    retention: None,
                    candidates: vec![TierCandidate {
                        id: format!("{provider}/{model}"),
                        provider: (*provider).to_owned(),
                        model: (*model).to_owned(),
                        surface: None,
                        rates: flat(1.0, 1.0),
                        metadata: ModelMetadata::default(),
                    }],
                },
            );
        }
        TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
            unified: BTreeMap::new(),
        }
    }

    fn find<'a>(found: &'a [Candidate], model: &str) -> Option<&'a Candidate> {
        found.iter().find(|candidate| candidate.model == model)
    }

    // A fixture in the models.dev shape: a chat model, an embedding (text
    // output, non-chat by name), a TTS model (audio output, non-chat by
    // modality), and a higher version of a family the catalog carries.
    const SOURCE: &str = r#"{
      "xai": { "models": {
        "grok-4.3": { "modalities": { "input": ["text"], "output": ["text"] } },
        "grok-4.6": { "modalities": { "input": ["text"], "output": ["text"] } },
        "grok-4.7": { "modalities": { "input": ["text"], "output": ["text"] } },
        "grok-imagine-image": { "modalities": { "input": ["text"], "output": ["image"] } }
      } },
      "fireworks-ai": { "models": {
        "accounts/fireworks/models/glm-5p2": { "modalities": { "output": ["text"] } },
        "accounts/fireworks/models/glm-5p3": { "modalities": { "output": ["text"] } },
        "accounts/fireworks/models/text-embedding-v2": { "modalities": { "output": ["text"] } }
      } },
      "amazon-bedrock": { "models": {
        "xai.grok-4.6": { "modalities": { "input": ["text"], "output": ["text"] } }
      } },
      "cohere": { "models": {
        "command-r-plus": { "modalities": { "output": ["text"] } }
      } }
    }"#;

    /// The credentialed set the fixture is diffed against: xai (identity),
    /// fireworks (declared `fireworks-ai`), and bedrock (discovery override
    /// `amazon-bedrock`). `cohere` is present in the source but NOT here — a
    /// provider this deployment holds no credential for.
    fn credentialed() -> Vec<(String, String)> {
        vec![
            ("xai".to_owned(), "xai".to_owned()),
            ("fireworks".to_owned(), "fireworks-ai".to_owned()),
            ("bedrock".to_owned(), "amazon-bedrock".to_owned()),
        ]
    }

    /// A fixed "today" for the recency-agnostic tests.
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 20).expect("a valid date")
    }

    /// A window wide enough that the recency pass never removes a fixture model
    /// — used by every test not about recency. The models in [`SOURCE`] carry
    /// no `release_date` and so are kept regardless, but this makes the intent
    /// explicit.
    const WIDE_WINDOW: i64 = 3650;

    #[test]
    fn a_new_chat_model_is_reported_and_an_embedding_is_filtered() {
        let catalog = catalog(&[
            ("xai", "grok-4.6"),
            ("fireworks", "accounts/fireworks/models/glm-5p2"),
        ]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);

        // grok-4.7: a family we carry (4.6) with a higher version — reported.
        assert!(
            find(&found, "grok-4.7").is_some(),
            "a higher grok must surface"
        );
        // The embedding outputs text, so modality cannot catch it; the name
        // token must. It is noise and must not appear.
        assert!(
            find(&found, "accounts/fireworks/models/text-embedding-v2").is_none(),
            "an embedding is not a chat lane and must be filtered"
        );
        // The image generator is caught by output modality.
        assert!(
            find(&found, "grok-imagine-image").is_none(),
            "an image-only model must be filtered by its output modality"
        );
    }

    #[test]
    fn a_higher_version_in_a_carried_family_is_a_version_bump() {
        // glm-5.2 in the catalog (spelled glm-5p2), glm-5.3 in the source.
        let catalog = catalog(&[("fireworks", "accounts/fireworks/models/glm-5p2")]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);
        let bump =
            find(&found, "accounts/fireworks/models/glm-5p3").expect("glm-5.3 must be discovered");
        assert_eq!(bump.category, Category::VersionBump);
        assert_eq!(bump.provider, "fireworks");
    }

    #[test]
    fn the_same_model_on_another_cloud_is_cross_cloud() {
        // grok-4.6 carried on xai; the source lists it under amazon-bedrock,
        // which resolves to our bedrock lane.
        let catalog = catalog(&[("xai", "grok-4.6")]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);
        let twin = find(&found, "xai.grok-4.6").expect("grok on bedrock must surface");
        assert_eq!(twin.category, Category::CrossCloud);
        assert_eq!(twin.provider, "bedrock");
        assert!(
            twin.note.contains("xai"),
            "the note names the cloud we already carry it on"
        );
    }

    #[test]
    fn cross_cloud_wins_over_version_bump() {
        // On bedrock, source `xai.grok-4.6` is genuinely BOTH: we carry an
        // earlier grok-4.3 ON BEDROCK (a bump), and we carry grok-4.6 on xai (a
        // twin). Both categories apply, so this pins the precedence: the twin
        // wins.
        let catalog = catalog(&[("xai", "grok-4.6"), ("bedrock", "xai.grok-4.3")]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);
        let twin = find(&found, "xai.grok-4.6").expect("grok on bedrock must surface");
        assert_eq!(twin.category, Category::CrossCloud);
    }

    #[test]
    fn a_model_we_already_carry_is_not_reported() {
        // Both the exact xai lane and its bedrock twin are carried.
        let catalog = catalog(&[
            ("xai", "grok-4.6"),
            ("xai", "grok-4.3"),
            ("fireworks", "accounts/fireworks/models/glm-5p2"),
            ("fireworks", "accounts/fireworks/models/glm-5p3"),
        ]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);
        assert!(find(&found, "grok-4.3").is_none(), "grok-4.3 is carried");
        assert!(find(&found, "grok-4.6").is_none(), "grok-4.6 is carried");
        assert!(
            find(&found, "accounts/fireworks/models/glm-5p3").is_none(),
            "glm-5.3 is carried"
        );
    }

    #[test]
    fn a_provider_we_are_not_credentialed_for_is_not_reported() {
        // cohere is in the source but not in the credentialed set.
        let catalog = catalog(&[("xai", "grok-4.6")]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);
        assert!(
            find(&found, "command-r-plus").is_none(),
            "a provider we cannot dispatch on must never appear"
        );
    }

    #[test]
    fn a_version_we_have_already_passed_is_new_not_a_bump() {
        // We carry grok-4.6; the source lists grok-4.3 (lower). It is not a
        // bump — a bump is strictly above the highest we hold — and since no
        // lane carries grok-4.3, it degrades to New rather than vanishing.
        let source = r#"{ "xai": { "models": {
          "grok-4.3": { "modalities": { "output": ["text"] } },
          "grok-4.6": { "modalities": { "output": ["text"] } }
        } } }"#;
        let catalog = catalog(&[("xai", "grok-4.6")]);
        let found = discover(
            &catalog,
            source,
            &[("xai".to_owned(), "xai".to_owned())],
            today(),
            WIDE_WINDOW,
        );
        let lower = find(&found, "grok-4.3").expect("an uncarried lower version still surfaces");
        assert_eq!(
            lower.category,
            Category::New,
            "a passed version is New, never a bump and never dropped"
        );
    }

    #[test]
    fn a_size_variant_is_not_mistaken_for_a_version_bump() {
        // 120b is a bigger MODEL than 20b, not a newer VERSION of it. Carrying
        // gpt-oss-20b must not make gpt-oss-120b a bump; it is a distinct
        // model, so New.
        let source = r#"{ "groq": { "models": {
          "openai/gpt-oss-20b":  { "modalities": { "output": ["text"] } },
          "openai/gpt-oss-120b": { "modalities": { "output": ["text"] } }
        } } }"#;
        let catalog = catalog(&[("groq", "openai/gpt-oss-20b")]);
        let found = discover(
            &catalog,
            source,
            &[("groq".to_owned(), "groq".to_owned())],
            today(),
            WIDE_WINDOW,
        );
        let bigger = find(&found, "openai/gpt-oss-120b").expect("the 120b model surfaces");
        assert_eq!(
            bigger.category,
            Category::New,
            "a parameter-size difference is not a version bump"
        );
    }

    #[test]
    fn an_unparseable_version_degrades_to_new_rather_than_dropping() {
        // A family we carry (grok-4.6) and a source model in the same prefix
        // whose version cannot be compared (a build tag, not a release
        // number). It must still be reported — as New.
        let source = r#"{ "xai": { "models": {
          "grok-build-0.1": { "modalities": { "output": ["text"] } }
        } } }"#;
        let catalog = catalog(&[("xai", "grok-4.6")]);
        let found = discover(
            &catalog,
            source,
            &[("xai".to_owned(), "xai".to_owned())],
            today(),
            WIDE_WINDOW,
        );
        let built = find(&found, "grok-build-0.1").expect("an uncomparable model still surfaces");
        // grok-build-0.1 shares no (prefix, suffix) family with grok-4.6
        // (suffix differs), so it cannot be a bump; the point is that it is
        // reported at all rather than silently dropped.
        assert_eq!(built.category, Category::New);
    }

    #[test]
    fn regional_spellings_of_one_model_collapse_to_one_finding() {
        // Bedrock lists a model in several regional spellings. They are one
        // opportunity, not five rows.
        let source = r#"{ "amazon-bedrock": { "models": {
          "anthropic.claude-sonnet-5":        { "modalities": { "output": ["text"] } },
          "us.anthropic.claude-sonnet-5":     { "modalities": { "output": ["text"] } },
          "eu.anthropic.claude-sonnet-5":     { "modalities": { "output": ["text"] } },
          "global.anthropic.claude-sonnet-5": { "modalities": { "output": ["text"] } }
        } } }"#;
        let catalog = catalog(&[("anthropic", "claude-sonnet-5")]);
        let found = discover(
            &catalog,
            source,
            &[("bedrock".to_owned(), "amazon-bedrock".to_owned())],
            today(),
            WIDE_WINDOW,
        );
        let bedrock: Vec<_> = found.iter().filter(|c| c.provider == "bedrock").collect();
        assert_eq!(
            bedrock.len(),
            1,
            "the four regional spellings are one finding"
        );
        assert_eq!(bedrock[0].category, Category::CrossCloud);
    }

    #[test]
    fn resolve_source_keys_prefers_declared_then_override_then_identity() {
        let source = r#"{ "xai": {"models":{}}, "fireworks-ai": {"models":{}}, "amazon-bedrock": {"models":{}} }"#;
        let inventory = vec![
            ("xai".to_owned(), None),                                  // identity
            ("fireworks".to_owned(), Some("fireworks-ai".to_owned())), // declared
            ("bedrock".to_owned(), None),                              // discovery override
            ("openai".to_owned(), None), // absent from source → dropped
        ];
        let resolved = resolve_source_keys(source, &inventory);
        assert_eq!(
            resolved,
            vec![
                ("xai".to_owned(), "xai".to_owned()),
                ("fireworks".to_owned(), "fireworks-ai".to_owned()),
                ("bedrock".to_owned(), "amazon-bedrock".to_owned()),
            ],
            "openai has no key in this source and is dropped"
        );
    }

    #[test]
    fn the_json_shape_matches_the_candidate_fields() {
        let catalog = catalog(&[("xai", "grok-4.6")]);
        let found = discover(&catalog, SOURCE, &credentialed(), today(), WIDE_WINDOW);
        let twin = find(&found, "xai.grok-4.6").expect("cross-cloud grok");
        let value = serde_json::to_value(twin).expect("a candidate serializes");
        assert_eq!(value["category"], "cross-cloud");
        assert_eq!(value["provider"], "bedrock");
        assert_eq!(value["model"], "xai.grok-4.6");
        assert!(value["note"].is_string());
        // Exactly the four documented fields, no rates leaked in.
        let object = value.as_object().expect("an object");
        assert_eq!(object.len(), 4);
    }

    // ---- recency of the NEW category ----------------------------------------

    /// A single xai provider carrying nothing, so every source model is a NEW
    /// candidate the recency window can act on. Dates chosen against a `today`
    /// of 2026-08-20.
    const DATED_SOURCE: &str = r#"{
      "xai": { "models": {
        "aurora-ancient": { "modalities": { "output": ["text"] }, "release_date": "2023-01-01" },
        "aurora-fresh":   { "modalities": { "output": ["text"] }, "release_date": "2026-08-14" },
        "aurora-undated": { "modalities": { "output": ["text"] } }
      } }
    }"#;

    fn xai_only() -> Vec<(String, String)> {
        vec![("xai".to_owned(), "xai".to_owned())]
    }

    #[test]
    fn an_ancient_new_model_is_filtered_by_recency() {
        // 2023 is well outside a 90-day window ending 2026-08-20.
        let catalog = catalog(&[]);
        let found = discover(&catalog, DATED_SOURCE, &xai_only(), today(), 90);
        assert!(
            find(&found, "aurora-ancient").is_none(),
            "a model years old is not a currency finding"
        );
    }

    #[test]
    fn a_recent_new_model_is_kept() {
        // 2026-08-14 is six days before today — inside the window.
        let catalog = catalog(&[]);
        let found = discover(&catalog, DATED_SOURCE, &xai_only(), today(), 90);
        let fresh = find(&found, "aurora-fresh").expect("a recent model must surface");
        assert_eq!(fresh.category, Category::New);
    }

    #[test]
    fn an_undated_new_model_is_kept_regardless_of_window() {
        // Even at a one-day window, a model with no release_date survives:
        // unknown age is never a reason to drop.
        let catalog = catalog(&[]);
        let found = discover(&catalog, DATED_SOURCE, &xai_only(), today(), 1);
        assert!(
            find(&found, "aurora-undated").is_some(),
            "an undated model is kept — dropping it would be guessing it is old"
        );
        // ...while the dated-but-old ones are gone at this window.
        assert!(find(&found, "aurora-ancient").is_none());
        assert!(find(&found, "aurora-fresh").is_none());
    }

    #[test]
    fn the_recency_cutoff_boundary_is_exact_and_year_month_parses_as_the_first() {
        // today - 31 days = 2026-05-01 exactly, so the cutoff is 2026-05-01.
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let source = r#"{ "xai": { "models": {
          "on-cutoff":   { "modalities": { "output": ["text"] }, "release_date": "2026-05-01" },
          "day-before":  { "modalities": { "output": ["text"] }, "release_date": "2026-04-30" },
          "year-month":  { "modalities": { "output": ["text"] }, "release_date": "2026-05" }
        } } }"#;
        let catalog = catalog(&[]);
        let found = discover(&catalog, source, &xai_only(), today, 31);
        assert!(
            find(&found, "on-cutoff").is_some(),
            "a model dated exactly on the cutoff is kept (inclusive boundary)"
        );
        assert!(
            find(&found, "day-before").is_none(),
            "one day past the cutoff is dropped"
        );
        assert!(
            find(&found, "year-month").is_some(),
            "`2026-05` parses as 2026-05-01, which is on the cutoff and kept"
        );
    }

    #[test]
    fn cross_cloud_and_version_bump_ignore_the_recency_window() {
        // grok-4.6 carried on xai. The source offers, with ANCIENT dates: a
        // cross-cloud twin on bedrock and a version bump on xai. Even at a
        // one-day window both must survive — recency touches only NEW.
        let catalog = catalog(&[("xai", "grok-4.6")]);
        let source = r#"{
          "xai": { "models": {
            "grok-4.6": { "modalities": { "output": ["text"] }, "release_date": "2019-01-01" },
            "grok-4.7": { "modalities": { "output": ["text"] }, "release_date": "2019-01-01" }
          } },
          "amazon-bedrock": { "models": {
            "xai.grok-4.6": { "modalities": { "output": ["text"] }, "release_date": "2018-01-01" }
          } }
        }"#;
        let credentialed = vec![
            ("xai".to_owned(), "xai".to_owned()),
            ("bedrock".to_owned(), "amazon-bedrock".to_owned()),
        ];
        let found = discover(&catalog, source, &credentialed, today(), 1);
        let bump = find(&found, "grok-4.7").expect("an old version bump still surfaces");
        assert_eq!(bump.category, Category::VersionBump);
        let twin = find(&found, "xai.grok-4.6").expect("an old cross-cloud twin still surfaces");
        assert_eq!(twin.category, Category::CrossCloud);
    }

    #[test]
    fn new_is_sorted_newest_first_with_undated_last() {
        let source = r#"{ "xai": { "models": {
          "older":   { "modalities": { "output": ["text"] }, "release_date": "2026-07-01" },
          "newer":   { "modalities": { "output": ["text"] }, "release_date": "2026-08-01" },
          "undated": { "modalities": { "output": ["text"] } }
        } } }"#;
        let catalog = catalog(&[]);
        let found = discover(&catalog, source, &xai_only(), today(), 90);
        let order: Vec<&str> = found
            .iter()
            .filter(|candidate| candidate.category == Category::New)
            .map(|candidate| candidate.model.as_str())
            .collect();
        assert_eq!(
            order,
            vec!["newer", "older", "undated"],
            "newest first, undated last"
        );
    }

    // ---- provider discovery (the other direction) ---------------------------

    /// A source with two uncredentialed providers: `baseten` (first-party,
    /// serves two models we carry plus one we don't, plus an embedding) and
    /// `openrouter` (a known aggregator with a BIGGER overlap — the sort must
    /// still put the first-party row first). `cohere` serves nothing we carry
    /// and must be counted, not listed. Regional respellings of one model
    /// (`us.` prefix) must count once.
    const PROVIDER_SOURCE: &str = r#"{
      "xai": { "models": {
        "grok-4.6": { "modalities": { "output": ["text"] } }
      } },
      "baseten": { "models": {
        "zai-org/GLM-5.3": { "modalities": { "output": ["text"] } },
        "moonshotai/Kimi-K3": { "modalities": { "output": ["text"] } },
        "nvidia/Nemotron-120B-A12B": { "modalities": { "output": ["text"] } },
        "baseten/text-embedding": { "modalities": { "output": ["embedding"] } }
      } },
      "openrouter": { "models": {
        "z-ai/glm-5.3": { "modalities": { "output": ["text"] } },
        "moonshotai/kimi-k3": { "modalities": { "output": ["text"] } },
        "x-ai/grok-4.6": { "modalities": { "output": ["text"] } },
        "us.x-ai/grok-4.6": { "modalities": { "output": ["text"] } }
      } },
      "cohere": { "models": {
        "command-r-plus": { "modalities": { "output": ["text"] } }
      } }
    }"#;

    fn provider_catalog() -> TierCatalog {
        catalog(&[
            ("fireworks", "accounts/fireworks/models/glm-5p3"),
            ("moonshot", "kimi-k3"),
            ("xai", "grok-4.6"),
        ])
    }

    #[test]
    fn uncredentialed_providers_rank_by_overlap_with_first_party_before_aggregators() {
        let report = discover_providers(&provider_catalog(), PROVIDER_SOURCE, &xai_only());
        let keys: Vec<&str> = report
            .candidates
            .iter()
            .map(|candidate| candidate.key.as_str())
            .collect();
        // openrouter overlaps 3 (glm-5.3, kimi-k3, grok-4.6) to baseten's 2,
        // but baseten is first-party and leads anyway.
        assert_eq!(keys, vec!["baseten", "openrouter"]);
        assert!(!report.candidates[0].aggregator);
        assert!(report.candidates[1].aggregator, "openrouter is tagged");
        assert_eq!(report.candidates[0].overlap, 2, "glm-5.3 + kimi-k3");
        assert_eq!(report.candidates[1].overlap, 3);
    }

    #[test]
    fn credentialed_providers_are_excluded_from_the_provider_scan() {
        let report = discover_providers(&provider_catalog(), PROVIDER_SOURCE, &xai_only());
        assert!(
            report
                .candidates
                .iter()
                .all(|candidate| candidate.key != "xai"),
            "a provider we hold a credential for is not an outreach lead"
        );
    }

    #[test]
    fn zero_overlap_providers_are_counted_not_listed() {
        let report = discover_providers(&provider_catalog(), PROVIDER_SOURCE, &xai_only());
        assert!(
            report
                .candidates
                .iter()
                .all(|candidate| candidate.key != "cohere")
        );
        assert_eq!(
            report.zero_overlap_dropped, 1,
            "cohere: counted, not listed"
        );
    }

    #[test]
    fn regional_respellings_and_non_chat_models_do_not_inflate_the_counts() {
        let report = discover_providers(&provider_catalog(), PROVIDER_SOURCE, &xai_only());
        let openrouter = report
            .candidates
            .iter()
            .find(|candidate| candidate.key == "openrouter")
            .expect("openrouter is listed");
        // Four raw rows, but `x-ai/grok-4.6` and `us.x-ai/grok-4.6` are one
        // canonical model: 3 chat models, all overlapping.
        assert_eq!(openrouter.chat_models, 3);
        let baseten = report
            .candidates
            .iter()
            .find(|candidate| candidate.key == "baseten")
            .expect("baseten is listed");
        // The embedding is filtered by modality; three chat models remain.
        assert_eq!(baseten.chat_models, 3);
        assert_eq!(
            baseten.overlap_examples,
            vec!["glm-5.3", "kimi-k3"],
            "examples are the overlapping canonicals, sorted"
        );
    }

    /// No first-party host serves two competing labs' closed models, so 2+
    /// frontier families is the computed aggregator signal — sorted between
    /// the open-weight hosts (the actionable leads) and the const-tagged
    /// aggregators, because the reseller might still be a hyperscaler worth
    /// talking to.
    #[test]
    fn a_multi_frontier_reseller_is_tagged_and_sorts_after_open_weight_hosts() {
        let source = r#"{
          "gpuhost": { "models": {
            "zai-org/GLM-5.3": { "modalities": { "output": ["text"] } }
          } },
          "megagateway": { "models": {
            "claude-opus-5":    { "modalities": { "output": ["text"] } },
            "gpt-5.6-sol":      { "modalities": { "output": ["text"] } },
            "gemini-3.7-flash": { "modalities": { "output": ["text"] } }
          } },
          "openrouter": { "models": {
            "z-ai/glm-5.3": { "modalities": { "output": ["text"] } }
          } }
        }"#;
        let catalog = catalog(&[
            ("anthropic", "claude-opus-5"),
            ("openai", "gpt-5.6-sol"),
            ("google", "gemini-3.7-flash"),
            ("fireworks", "accounts/fireworks/models/glm-5p3"),
        ]);
        let report = discover_providers(&catalog, source, &xai_only());
        let keys: Vec<&str> = report
            .candidates
            .iter()
            .map(|candidate| candidate.key.as_str())
            .collect();
        // gpuhost overlaps only 1 to megagateway's 3, and still leads: an
        // open-weight host outranks any multi-frontier reseller.
        assert_eq!(keys, vec!["gpuhost", "megagateway", "openrouter"]);
        assert_eq!(report.candidates[1].frontier_families, 3);
        assert!(
            !report.candidates[1].aggregator,
            "computed, not const-listed"
        );
        // The p-notation fold makes fireworks' `glm-5p3` and zai's `GLM-5.3`
        // one canonical model — the overlap that motivated the fold.
        assert_eq!(report.candidates[0].overlap, 1);
    }
}
