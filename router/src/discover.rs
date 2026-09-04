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

use serde::{Deserialize, Serialize};

use crate::config::TierCatalog;

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

    bare
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
#[must_use]
pub fn discover(
    catalog: &TierCatalog,
    source: &str,
    credentialed: &[(String, String)],
) -> Vec<Candidate> {
    let providers: BTreeMap<String, SourceProvider> =
        serde_json::from_str(source).unwrap_or_default();
    let carried = Carried::from_catalog(catalog);

    let mut out = Vec::new();
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

            let candidate = classify(provider, model, &name, &carried);
            out.push(candidate);
        }
    }

    out
}

/// Decide which category one uncarried source model falls into, strongest
/// first — the precedence documented on [`Category`].
fn classify(provider: &str, model: &str, name: &str, carried: &Carried) -> Candidate {
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

    Candidate {
        category: Category::New,
        provider: provider.to_owned(),
        model: model.to_owned(),
        note: "no catalog candidate carries this model".to_owned(),
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
#[must_use]
pub fn discover_from_inventory(catalog: &TierCatalog, source: &str) -> Vec<Candidate> {
    let inventory = crate::providers::inventory_source_keys();
    let credentialed = resolve_source_keys(source, &inventory);
    discover(catalog, source, &credentialed)
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

    #[test]
    fn a_new_chat_model_is_reported_and_an_embedding_is_filtered() {
        let catalog = catalog(&[
            ("xai", "grok-4.6"),
            ("fireworks", "accounts/fireworks/models/glm-5p2"),
        ]);
        let found = discover(&catalog, SOURCE, &credentialed());

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
        let found = discover(&catalog, SOURCE, &credentialed());
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
        let found = discover(&catalog, SOURCE, &credentialed());
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
        let found = discover(&catalog, SOURCE, &credentialed());
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
        let found = discover(&catalog, SOURCE, &credentialed());
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
        let found = discover(&catalog, SOURCE, &credentialed());
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
        let found = discover(&catalog, source, &[("xai".to_owned(), "xai".to_owned())]);
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
        let found = discover(&catalog, source, &[("groq".to_owned(), "groq".to_owned())]);
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
        let found = discover(&catalog, source, &[("xai".to_owned(), "xai".to_owned())]);
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
        let found = discover(&catalog, SOURCE, &credentialed());
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
}
