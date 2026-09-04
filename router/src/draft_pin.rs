//! Draft a `tiers.toml` lane from a research agent's "facts dossier" — phase 2
//! of the currency loop [`crate::discover`] opens.
//!
//! [`crate::discover`] answers *which* models the upstream world carries that
//! the catalog does not, and stops there on purpose: adding a lane is not a
//! mechanical diff, it needs a transcribed price, a retention basis, and a
//! judgement no diff can make. This module is the next step and NOT the last
//! one. It takes a dossier a human (or a research agent) assembled about ONE
//! discovered candidate and turns it into a PROPOSED stanza for `tiers.toml` —
//! the exact TOML to paste, plus the evidence behind every claim in it. A human
//! still reads it and still opens the pull request. **The bot never merges.**
//!
//! Two claims in that stanza are load-bearing in a way that shapes every rule
//! here:
//!
//! - **Price is the money path.** A drafted price becomes what a customer is
//!   billed, so a number with no traceable source is refused rather than
//!   guessed, exactly as [`crate::drift`] refuses to write one.
//! - **A retention label is a claim to a customer about their own data.** A
//!   wrong `standard` costs a little marketing; a wrong `zero` is a false
//!   statement of the kind a regulator or a plaintiff reads literally. So the
//!   posture resolver fails **safe** — to `standard` — on every uncertainty,
//!   and reaches `zero` only on the narrow, mechanically-checkable grounds
//!   `docs/DEPLOY.md` spells out.
//!
//! The automation boundary is the crux. `docs/DEPLOY.md` permits a `zero` label
//! on exactly three bases: (1) a signed/confirmed arrangement, (2) an enforced
//! account setting, or (3) the provider's published default for all customers.
//! Bases 1 and 2 are facts about the OPERATOR'S OWN account or contract — a
//! web-research agent cannot verify them from public pages. Only basis 3 is
//! publicly verifiable. Therefore this drafter may auto-assign `zero` on basis 3
//! alone (and only with a real published-default citation), while a basis-1/2
//! `zero` is honoured only when the dossier carries `human_attested: true` — a
//! flag the drafter reads but NEVER sets for itself. Anything else is
//! `standard`.
//!
//! The strongest guarantee is the last one: whatever this module emits, it then
//! runs back through the router's own [`crate::config::load_tier_catalog`]. A
//! draft the router could not load is never produced. That check reuses the real
//! loader rather than a reimplementation of it, so the day the loader learns a
//! new rule the drafter inherits it for free.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::config::{RetentionPosture, TierCatalog, TierConfigError};

// ---------------------------------------------------------------------------
// The facts dossier — the research agent's output, and this module's input.
// ---------------------------------------------------------------------------

/// One discovered candidate, plus everything a human/agent transcribed about
/// it, as a JSON document handed to `admin draft-pin --facts <path>`.
///
/// Field names are snake_case, matching `tiers.toml`'s own price keys
/// (`input_per_mtok`, `source_url`) so a transcriber reads the same spelling on
/// both sides; only [`BasisKind`] carries a kebab-case value (`published-default`)
/// and the `category` string echoes [`crate::discover::Category`]'s kebab
/// spelling.
///
/// **Unknown fields are TOLERATED, not rejected.** The dossier is produced by an
/// LLM research agent (see `docs/research-skill.md`) that decorates it with
/// descriptive fields a strict schema never anticipated — a `label`/`note` on a
/// price band, a per-field `note`, other running commentary. `deny_unknown_fields`
/// would let a single stray key kill the whole draft, which for an unattended
/// weekly bot means silent no-output. Lenient parsing is SAFE here precisely
/// because every downstream invariant already fails toward a conservative
/// outcome: a mistyped REQUIRED price deserialises as absent and REFUSES
/// (invariants 2/3), and a mistyped `covers_this_model`/`human_attested`
/// deserialises as `false` and yields `standard` (invariant 4). Lenience can
/// only ever cause an over-refuse or a `standard` — never a false `zero` or a
/// wrong price.
#[derive(Debug, Clone, Deserialize)]
pub struct FactsDossier {
    /// The phase-1 [`crate::discover::Candidate`] this dossier expands, by
    /// value. Modelled locally rather than importing that type so the phase-1
    /// module keeps its narrow `Serialize`-only surface; the wire shape is
    /// identical.
    pub candidate: CandidateFacts,
    /// The `tiers.toml` lane KEY to emit, e.g. `anthropic/claude-opus-4-8`. It
    /// becomes the tier id AND the single candidate's id, which is what lets an
    /// OpenRouter client address the lane unchanged (see
    /// [`crate::config`]'s tier-id rule: a vendor pin is keyed by one of its own
    /// candidate ids).
    pub display_id: String,
    pub prices: Prices,
    pub retention: RetentionFacts,
    /// The researcher's STANDARD retention receipt: the evidence that the honest
    /// posture is `standard` (not that it is `zero`). Consumed two ways — it is
    /// shown in the dossier as the retention receipt, and when the lane resolves
    /// `standard` it is the evidence used to pin `standard` EXPLICITLY on a
    /// provider whose live pin is `zero` (see [`resolve_posture`] / FIX 3),
    /// rather than letting the lane silently inherit that `zero`.
    #[serde(default)]
    pub standard_evidence: Option<StandardEvidence>,
    /// Free-text caveats the research agent flagged for a human — surfaced
    /// verbatim in the dossier so nothing the researcher was unsure about is lost.
    #[serde(default)]
    pub gaps: Vec<String>,
    /// Whether a LIVE invoke actually succeeded. `true` (the boolean) is the
    /// only value that clears [`refuse_if_not_invokable`]; `false` or the string
    /// `"unknown"` refuse. This is the Bedrock GPT-5.6 / Grok trap: an account
    /// can be AUTHORIZED for a model and still answer `AccessDenied — contact
    /// Sales`, which looks available and is not.
    pub invokable: Invokable,
    #[serde(default)]
    pub invoke_evidence: String,
    /// The `verified` date to stamp on every emitted retention pin, as
    /// `YYYY-MM-DD`. Optional here because `--verified` may supply it; one of
    /// the two must be present. Never the system clock — a draft must be
    /// byte-reproducible from its inputs.
    #[serde(default)]
    pub verified: Option<String>,
}

/// A retention receipt for a `standard` posture: the same four evidence fields a
/// [`Basis`] carries, but asserting the honest default rather than a `zero`
/// claim. It never on its own produces a `zero` label.
#[derive(Debug, Clone, Deserialize)]
pub struct StandardEvidence {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub source_sha256: String,
    #[serde(default)]
    pub source_extract: String,
}

impl StandardEvidence {
    /// Whether this receipt can be built into a real retention pin: all four
    /// fields present, and the URL/digest well-formed enough that the loader
    /// will accept them.
    fn is_usable(&self) -> bool {
        !self.description.trim().is_empty()
            && !self.source_extract.trim().is_empty()
            && looks_http(&self.source_url)
            && looks_sha256(&self.source_sha256)
    }
}

/// The phase-1 candidate fields, in the shape [`crate::discover::Candidate`]
/// serialises to. `category` is a free string here rather than a typed enum: it
/// is echoed into the dossier for a human and never drives a money or retention
/// decision, so validating it would add a refusal path for no safety.
#[derive(Debug, Clone, Deserialize)]
pub struct CandidateFacts {
    pub category: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub note: String,
}

/// Whether a live invoke was proven. A bool or the literal string `"unknown"`,
/// so the dossier can say "I did not check" distinctly from "I checked and it
/// failed" — both of which refuse, but a reader deserves to know which.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Invokable {
    Proven(bool),
    Unknown(String),
}

impl Invokable {
    /// The one true state. Anything else — `false`, `"unknown"`, any other
    /// string — is unproven and refuses.
    #[must_use]
    pub fn is_proven(&self) -> bool {
        matches!(self, Self::Proven(true))
    }

    fn describe(&self) -> String {
        match self {
            Self::Proven(value) => value.to_string(),
            Self::Unknown(text) => format!("\"{text}\""),
        }
    }
}

/// The price facts, in per-million-token USD, with the provenance that makes
/// each number traceable. `input_per_mtok` and `output_per_mtok` are optional
/// in the type only so a missing one becomes a clean refusal
/// ([`refuse_if_prices_incomplete`]) rather than a serde error.
#[derive(Debug, Clone, Deserialize)]
pub struct Prices {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Conditional rate bands. Only a band keyed on `min_prompt_tokens` — the
    /// OpenAI 272k repricing shape — is a lane rate this drafter models. Research
    /// agents overload this list with cache-TTL, batch, region, and time
    /// variants that carry no `min_prompt_tokens`; those are NOT lane rates and
    /// are ignored (see [`context_bands`]). Optional and commonly empty.
    #[serde(default)]
    pub conditional: Vec<ConditionalPrice>,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub source_sha256: String,
    #[serde(default)]
    pub source_extract: String,
}

/// One `conditional` entry. Every field is optional because the research agent
/// emits entries of many shapes here; only an entry with `min_prompt_tokens`
/// present is a context-tier lane rate (see [`context_bands`]), and such an
/// entry still needs `input`/`output` to load, which the loader enforces.
#[derive(Debug, Clone, Deserialize)]
pub struct ConditionalPrice {
    #[serde(default)]
    pub min_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_per_mtok: Option<f64>,
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

/// The retention facts: whether the model is open-weight, and the ONE basis (if
/// any) the dossier offers for a `zero` claim.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionFacts {
    /// Whether the model is open-weight. Load-bearing for basis 3: a closed-
    /// weight model may not ride a published-default `zero` pin, because the
    /// common published default (Fireworks) is scoped to open models — the
    /// `fireworks/qwen3.8-max` case.
    pub weight_open: bool,
    #[serde(default)]
    pub basis: Option<Basis>,
}

/// A single retention basis, exactly as `docs/DEPLOY.md` frames the three.
#[derive(Debug, Clone, Deserialize)]
pub struct Basis {
    pub kind: BasisKind,
    /// What the basis asserts the posture is. `standard` here never yields a
    /// `zero` draft; only `zero` can, and only when the rest of the conditions
    /// hold.
    pub posture: RetentionPosture,
    /// Whether the cited evidence's SCOPE actually reaches this model. A false
    /// here is the honest transcription of "the page says zero, but not about
    /// this surface/model".
    pub covers_this_model: bool,
    /// Only meaningful for `signed`/`enforced`. The drafter never sets it — it
    /// only honours it. Absent (the default `false`) downgrades a basis-1/2
    /// `zero` to `standard`.
    #[serde(default)]
    pub human_attested: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub source_sha256: String,
    #[serde(default)]
    pub source_extract: String,
}

impl Basis {
    /// Whether the basis carries the three evidence fields a claim needs to be
    /// checkable. Non-emptiness only — well-formedness is [`Self::is_pin_usable`].
    fn has_provenance(&self) -> bool {
        !self.source_url.trim().is_empty()
            && !self.source_sha256.trim().is_empty()
            && !self.source_extract.trim().is_empty()
    }

    /// Whether this basis can be built into a real retention pin the loader will
    /// accept: a well-formed URL and digest. The description is synthesised when
    /// blank ([`pin_from_basis`]), so it is not required here.
    fn is_pin_usable(&self) -> bool {
        looks_http(&self.source_url) && looks_sha256(&self.source_sha256)
    }
}

/// Which of `docs/DEPLOY.md`'s three bases a [`Basis`] claims.
///
/// The split that matters is `PublishedDefault` (basis 3, publicly verifiable)
/// versus the other two (bases 1 and 2, facts about the operator's own account
/// that a research agent cannot verify). See [`resolve_posture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BasisKind {
    Signed,
    Enforced,
    PublishedDefault,
}

impl BasisKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Signed => "signed",
            Self::Enforced => "enforced",
            Self::PublishedDefault => "published-default",
        }
    }

    /// Whether this basis is one a web-research agent CANNOT verify from public
    /// pages — a fact about the operator's own account or contract. True for
    /// `signed` and `enforced`; false for the publicly-verifiable published
    /// default. These are the bases that require `human_attested`.
    const fn is_account_private(self) -> bool {
        matches!(self, Self::Signed | Self::Enforced)
    }
}

// ---------------------------------------------------------------------------
// The decision and the draft.
// ---------------------------------------------------------------------------

/// The pure result of [`draft`] before the load-validation step.
#[derive(Debug, Clone)]
pub enum Decision {
    /// The dossier failed a fail-safe gate; nothing is drafted.
    Refused { reason: String },
    /// A stanza was produced. It still must survive [`load_fragment`] before it
    /// is shown as a draft — see [`draft_and_validate`].
    Drafted(Box<Draft>),
}

/// A produced stanza and the reasoning behind it. Every byte here is a pure
/// function of the dossier and the `verified` date — no clock, no environment —
/// so the same input yields the same draft.
#[derive(Debug, Clone)]
pub struct Draft {
    pub display_id: String,
    pub provider: String,
    pub posture: RetentionPosture,
    /// Whether an explicit `[tiers."<id>".retention]` override is part of
    /// `stanza_toml` (the closed-weight-on-a-zero-provider case).
    pub override_emitted: bool,
    /// The exact TOML to paste into `tiers.toml`.
    pub stanza_toml: String,
    pub dossier_markdown: String,
    pub flags: Vec<String>,
    /// Which retention pin the lane relies on, for the dossier.
    pub inherit_note: String,
    /// A well-formed provider pin used ONLY to make the fragment loadable when
    /// no override is emitted; never part of `stanza_toml`. `None` when an
    /// override is emitted (the override labels the lane by itself).
    scaffold_pin: Option<PinFields>,
}

impl Draft {
    /// Assemble the self-contained catalog fragment that [`load_fragment`] runs
    /// through the real loader.
    ///
    /// The emitted stanza only becomes loadable once the lane is LABELLED with a
    /// retention posture. When an override is emitted the stanza labels itself;
    /// otherwise a provider pin ([`Self::scaffold_pin`]) is prepended as
    /// scaffolding. The scaffold never appears in the draft a human sees — it
    /// exists so the loader can exercise the stanza's structure, rates, and
    /// margins, and (for a `zero` lane) the well-formedness of the cited
    /// evidence.
    #[must_use]
    pub fn validation_fragment(&self) -> String {
        let mut fragment = String::from("schema_version = 1\n\n");
        if let Some(pin) = &self.scaffold_pin {
            let _ = writeln!(fragment, "[retention.{}]", self.provider);
            fragment.push_str(&render_pin_body(pin));
            fragment.push('\n');
        }
        fragment.push_str(&self.stanza_toml);
        fragment
    }
}

// ---------------------------------------------------------------------------
// The pure drafting core.
// ---------------------------------------------------------------------------

/// Draft a lane from a dossier, applying every fail-safe gate. Pure: no IO, no
/// clock. `verified` is the already-resolved, already-validated ISO date;
/// `provider_posture` is the candidate provider's LIVE `[retention.<provider>]`
/// posture read from the operator's `tiers.toml` (`None` if the provider has no
/// pin there yet) — the fact that closes the false-zero fail-open (FIX 3).
///
/// The load-validation gate (invariant 5) is NOT here — it needs the loader,
/// which is async and file-backed — so a bare `draft` result is "would draft,
/// pending the load check". [`draft_and_validate`] is the whole story.
#[must_use]
pub fn draft(
    facts: &FactsDossier,
    verified: &str,
    provider_posture: Option<RetentionPosture>,
) -> Decision {
    // 1. Not proven invokable ⇒ refuse. A draft asserts "ready to serve".
    if let Some(reason) = refuse_if_not_invokable(facts) {
        return Decision::Refused { reason };
    }
    // 2/3. Prices must be present and traceable.
    if let Some(reason) = refuse_if_prices_incomplete(&facts.display_id, &facts.prices) {
        return Decision::Refused { reason };
    }
    let input = facts
        .prices
        .input_per_mtok
        .expect("input price presence was just checked");
    let output = facts
        .prices
        .output_per_mtok
        .expect("output price presence was just checked");

    // 4 + FIX 3. Posture, fail-safe to standard, reconciled against the live
    // provider pin. This is the only gate that can REFUSE on retention grounds:
    // a `standard` lane on a `zero` provider with no receipt to pin standard.
    let resolution = match resolve_posture(
        &facts.candidate.provider,
        &facts.retention,
        facts.standard_evidence.as_ref(),
        provider_posture,
        verified,
    ) {
        Ok(resolution) => resolution,
        Err(reason) => return Decision::Refused { reason },
    };

    // 6. Sanity FLAGS (warn, never refuse).
    let mut flags = price_sanity_flags(&facts.prices, input, output);
    flags.extend(resolution.flags.clone());

    let posture = resolution.posture;
    let reasoning = resolution.reasoning;
    let inherit_note = resolution.inherit_note;

    // Emit the stanza. Consume the placement once: an override goes INTO the
    // stanza; a scaffold is kept aside for load-validation only.
    let (override_pin, scaffold_pin) = match resolution.placement {
        PinPlacement::TierOverride(pin) => (Some(pin), None),
        PinPlacement::ProviderScaffold(pin) => (None, Some(pin)),
    };
    let override_emitted = override_pin.is_some();
    let stanza_toml = render_stanza(
        &facts.display_id,
        &facts.candidate,
        &facts.prices,
        input,
        output,
        override_pin.as_ref(),
    );

    let dossier_markdown = render_dossier(
        facts,
        input,
        output,
        posture,
        provider_posture,
        &reasoning,
        &inherit_note,
        &stanza_toml,
        &flags,
    );

    Decision::Drafted(Box::new(Draft {
        display_id: facts.display_id.clone(),
        provider: facts.candidate.provider.clone(),
        posture,
        override_emitted,
        stanza_toml,
        dossier_markdown,
        flags,
        inherit_note,
        scaffold_pin,
    }))
}

/// Invariant 1. Refuse a lane whose live availability is unproven, echoing the
/// dossier's `invoke_evidence` so a reader sees what was (or was not) checked.
fn refuse_if_not_invokable(facts: &FactsDossier) -> Option<String> {
    if facts.invokable.is_proven() {
        return None;
    }
    Some(format!(
        "REFUSED: {id} is not proven invokable (invokable = {state}). A draft asserts the lane is \
         ready to serve, and \"authorized\" is not \"serving\": a Bedrock/Sales-gated model answers \
         AccessDenied — contact Sales, and a preview waitlist authenticates and then refuses. Prove \
         a LIVE invoke succeeded before drafting. invoke_evidence: {evidence}",
        id = facts.display_id,
        state = facts.invokable.describe(),
        evidence = quote_or_none(&facts.invoke_evidence),
    ))
}

/// Invariants 2 and 3. Refuse a lane with no cost basis, or a price no human
/// could trace back to its source.
fn refuse_if_prices_incomplete(display_id: &str, prices: &Prices) -> Option<String> {
    let mut missing = Vec::new();
    if prices.input_per_mtok.is_none() {
        missing.push("input_per_mtok");
    }
    if prices.output_per_mtok.is_none() {
        missing.push("output_per_mtok");
    }
    if !missing.is_empty() {
        return Some(format!(
            "REFUSED: {display_id} is missing required price(s): {}. A lane with no cost basis \
             cannot be drafted — the drafted price is what a customer is billed.",
            missing.join(", "),
        ));
    }
    // Any price present at all demands full provenance. Input and output are
    // present by the check above, so provenance is always required here; the
    // conditional/cached dimensions only widen what "a price" means.
    let mut missing_provenance = Vec::new();
    if prices.source_url.trim().is_empty() {
        missing_provenance.push("prices.source_url");
    }
    if prices.source_sha256.trim().is_empty() {
        missing_provenance.push("prices.source_sha256");
    }
    if prices.source_extract.trim().is_empty() {
        missing_provenance.push("prices.source_extract");
    }
    if !missing_provenance.is_empty() {
        return Some(format!(
            "REFUSED: {display_id} carries prices without full provenance (missing {}). Never emit \
             a number a human cannot trace: source_url, source_sha256, and a verbatim source_extract \
             are all required.",
            missing_provenance.join(", "),
        ));
    }
    None
}

/// Where a resolved posture's evidence goes.
enum PinPlacement {
    /// Emit an explicit `[tiers."<id>".retention]` override in the stanza —
    /// whenever the resolved lane posture DIFFERS from the live provider pin, so
    /// the lane publishes what it earned rather than inheriting the provider's
    /// (the `fireworks/qwen3.8-max` shape, and its mirror when a lane earns
    /// `zero` on a provider whose pin is not `zero`).
    TierOverride(PinFields),
    /// Rely on the provider pin (the resolved posture equals it, or the provider
    /// is not `zero` so there is no unearned `zero` to inherit). The carried
    /// [`PinFields`] is scaffolding for load-validation only and is never emitted.
    ProviderScaffold(PinFields),
}

struct PostureResolution {
    posture: RetentionPosture,
    placement: PinPlacement,
    reasoning: String,
    flags: Vec<String>,
    inherit_note: String,
}

/// Invariant 4 (the posture core) reconciled with the live provider pin (FIX 3).
///
/// A lane earns `zero` only when EVERY condition holds:
///
/// - a basis is present with all three of `source_url`/`source_sha256`/
///   `source_extract` non-empty (a `zero` claim with no evidence is not a claim);
/// - `basis.posture == zero` and `basis.covers_this_model == true`;
/// - the kind is `published-default` (publicly verifiable, basis 3), OR it is
///   `signed`/`enforced` (bases 1/2, private to the account) AND
///   `human_attested == true`;
/// - the weight guard: open-weight, OR an account-private basis. A **published
///   default never zeros a closed-weight model**.
///
/// Every other case is `standard`. Then the resolved posture is reconciled with
/// the LIVE provider pin (`provider_posture`), which is what closes the
/// false-zero fail-open:
///
/// - resolved posture == provider pin → a plain lane inherits the pin;
/// - resolved `zero`, provider pin not `zero` → explicit `zero` override (also
///   the only way a lane can be labelled when the provider has no pin yet);
/// - resolved `standard`, provider pin `zero` → explicit `standard` override
///   built from a receipt (`standard_evidence`, or the basis), and if no receipt
///   exists, **REFUSE** rather than let the lane inherit an unearned `zero`.
fn resolve_posture(
    provider: &str,
    retention: &RetentionFacts,
    standard_evidence: Option<&StandardEvidence>,
    provider_posture: Option<RetentionPosture>,
    verified: &str,
) -> Result<PostureResolution, String> {
    let provider_is_zero = provider_posture == Some(RetentionPosture::Zero);
    let provider_note = describe_provider_pin(provider_posture);

    // Does the basis earn a `zero` label?
    if let Some(basis) = &retention.basis {
        let kind_ok = !basis.kind.is_account_private() || basis.human_attested;
        let weight_ok = retention.weight_open || basis.kind.is_account_private();
        let zero_ok = basis.has_provenance()
            && basis.posture == RetentionPosture::Zero
            && basis.covers_this_model
            && kind_ok
            && weight_ok;

        if zero_ok {
            let base = format!(
                "zero: basis kind={kind}, posture=zero, covers_this_model=true, provenance present{attest}.",
                kind = basis.kind.label(),
                attest = if basis.kind.is_account_private() {
                    ", human_attested=true"
                } else {
                    " (published default — publicly verifiable)"
                },
            );
            if provider_is_zero {
                // The lane and the provider agree: a plain lane inherits the pin.
                return Ok(PostureResolution {
                    posture: RetentionPosture::Zero,
                    placement: PinPlacement::ProviderScaffold(pin_from_basis(
                        basis,
                        RetentionPosture::Zero,
                        verified,
                    )),
                    reasoning: base,
                    flags: Vec::new(),
                    inherit_note: format!(
                        "inherits [retention.{provider}] = zero ({provider_note})"
                    ),
                });
            }
            // The lane earned zero but the provider pin is standard/absent:
            // publish zero EXPLICITLY (and this is the only way the lane is
            // labelled at all when the provider has no pin).
            return Ok(PostureResolution {
                posture: RetentionPosture::Zero,
                placement: PinPlacement::TierOverride(pin_from_basis(
                    basis,
                    RetentionPosture::Zero,
                    verified,
                )),
                reasoning: format!(
                    "{base} Provider pin is {provider_note}, so zero is pinned explicitly."
                ),
                flags: vec![AUTO_DESCRIPTION.to_owned(), ZERO_OVERRIDE_NOTE.to_owned()],
                inherit_note: "explicit [tiers.\"<id>\".retention] override → zero".to_owned(),
            });
        }
    }

    // Standard — the fail-safe. Say why (for the dossier).
    let reason = standard_reason(retention);

    // Provider pin is NOT zero: a plain standard lane cannot inherit an unearned
    // zero, so it is safe. Flag thin evidence unless a receipt is available.
    if !provider_is_zero {
        let (scaffold, needs_evidence) =
            standard_scaffold_any(retention.basis.as_ref(), standard_evidence, verified);
        let mut flags = Vec::new();
        if needs_evidence {
            flags.push(NEEDS_RETENTION_EVIDENCE.to_owned());
        }
        return Ok(PostureResolution {
            posture: RetentionPosture::Standard,
            placement: PinPlacement::ProviderScaffold(scaffold),
            reasoning: format!("standard: {reason}"),
            flags,
            inherit_note: format!("inherits [retention.{provider}] ({provider_note})"),
        });
    }

    // Provider pin IS zero and the lane is standard: pin standard EXPLICITLY, or
    // refuse. Never let the lane inherit an unearned zero — a false claim.
    if let Some(pin) = build_standard_override(
        retention.basis.as_ref(),
        standard_evidence,
        retention.weight_open,
        verified,
    ) {
        return Ok(PostureResolution {
            posture: RetentionPosture::Standard,
            placement: PinPlacement::TierOverride(pin),
            reasoning: format!(
                "standard (explicit override — live provider pin is zero): {reason}"
            ),
            flags: vec![AUTO_DESCRIPTION.to_owned()],
            inherit_note: "explicit [tiers.\"<id>\".retention] override → standard (does NOT \
                           inherit the provider's zero pin)"
                .to_owned(),
        });
    }
    Err(format!(
        "REFUSED: drafting `standard` on provider `{provider}`, whose live [retention.{provider}] \
         pin is `zero`, would make the lane inherit an unearned zero — a false zero-retention claim \
         to a customer, the one thing this drafter must never emit. Supply `standard_evidence` \
         (description + source_url + source_sha256 + source_extract) to pin standard explicitly, or \
         a valid zero basis to earn the zero. ({reason})"
    ))
}

/// A one-line description of the live provider pin, for reasoning/inherit notes.
fn describe_provider_pin(posture: Option<RetentionPosture>) -> String {
    match posture {
        Some(RetentionPosture::Zero) => "live provider pin = zero".to_owned(),
        Some(RetentionPosture::Standard) => "live provider pin = standard".to_owned(),
        None => "no live provider pin found — add [retention.<provider>] before merge".to_owned(),
    }
}

/// Why the lane fell to `standard`, for the dossier. `None` basis is its own
/// reason; otherwise the dominant unmet `zero` condition.
fn standard_reason(retention: &RetentionFacts) -> String {
    match &retention.basis {
        None => "no retention basis supplied.".to_owned(),
        Some(basis) => {
            let kind_ok = !basis.kind.is_account_private() || basis.human_attested;
            let weight_ok = retention.weight_open || basis.kind.is_account_private();
            downgrade_reason(
                basis,
                retention.weight_open,
                basis.has_provenance(),
                kind_ok,
                weight_ok,
            )
        }
    }
}

/// Build the `standard` override pin from the best available receipt, or `None`
/// when neither a usable `standard_evidence` nor a usable basis exists (→ the
/// caller REFUSES). `standard_evidence` is preferred: it is the dedicated
/// positive receipt for the standard posture; a downgraded-`zero` basis is the
/// fallback (its description explains why zero does not reach this lane).
fn build_standard_override(
    basis: Option<&Basis>,
    standard_evidence: Option<&StandardEvidence>,
    weight_open: bool,
    verified: &str,
) -> Option<PinFields> {
    if let Some(evidence) = standard_evidence.filter(|evidence| evidence.is_usable()) {
        return Some(PinFields {
            posture: RetentionPosture::Standard,
            description: evidence.description.clone(),
            source_url: evidence.source_url.clone(),
            verified: verified.to_owned(),
            source_sha256: evidence.source_sha256.clone(),
        });
    }
    if let Some(basis) = basis.filter(|basis| basis.is_pin_usable()) {
        let kind_ok = !basis.kind.is_account_private() || basis.human_attested;
        let description = if basis.posture == RetentionPosture::Zero {
            override_description(basis, weight_open, kind_ok)
        } else {
            nonblank(
                &basis.description,
                "retention basis (see source); description not supplied in the dossier",
            )
        };
        return Some(PinFields {
            posture: RetentionPosture::Standard,
            description,
            source_url: basis.source_url.clone(),
            verified: verified.to_owned(),
            source_sha256: basis.source_sha256.clone(),
        });
    }
    None
}

/// The scaffold for a plain `standard` lane, plus whether the evidence was too
/// thin to count as a receipt (which flags NEEDS RETENTION EVIDENCE). Prefers a
/// usable `standard_evidence`, then a usable basis, then a placeholder; a
/// placeholder means no receipt exists and the flag fires. Never refuses —
/// invariant 4 emits-and-flags a thin `standard` lane rather than refusing it.
fn standard_scaffold_any(
    basis: Option<&Basis>,
    standard_evidence: Option<&StandardEvidence>,
    verified: &str,
) -> (PinFields, bool) {
    if let Some(evidence) = standard_evidence.filter(|evidence| evidence.is_usable()) {
        return (
            PinFields {
                posture: RetentionPosture::Standard,
                description: evidence.description.clone(),
                source_url: evidence.source_url.clone(),
                verified: verified.to_owned(),
                source_sha256: evidence.source_sha256.clone(),
            },
            false,
        );
    }
    if let Some(basis) = basis.filter(|basis| basis.is_pin_usable()) {
        return (
            pin_from_basis(basis, RetentionPosture::Standard, verified),
            false,
        );
    }
    (placeholder_pin(RetentionPosture::Standard, verified), true)
}

const NEEDS_RETENTION_EVIDENCE: &str = "NEEDS RETENTION EVIDENCE: the dossier does not supply a \
     usable retention basis. The lane is drafted `standard`; confirm the provider pin is `standard` \
     (or add a basis / an override) before merge. A `zero` label is NEVER emitted to fill this gap.";

const AUTO_DESCRIPTION: &str = "AUTO-GENERATED override description — a human must read the cited \
     page and rewrite this description before merge; the drafter only templated it.";

const ZERO_OVERRIDE_NOTE: &str = "ZERO PINNED EXPLICITLY on a provider whose live pin is not `zero` \
     — the lane earned zero from its basis, but confirm the basis truly reaches this model before \
     merge; a zero label is a claim to a customer about their data.";

/// Spell out why a `zero`-claiming basis was downgraded, dominant reason first.
fn downgrade_reason(
    basis: &Basis,
    weight_open: bool,
    has_provenance: bool,
    kind_ok: bool,
    weight_ok: bool,
) -> String {
    if basis.posture != RetentionPosture::Zero {
        return format!("basis posture is {}, not zero.", basis.posture.wire_token());
    }
    if !has_provenance {
        return "the basis claims zero but is missing source_url/source_sha256/source_extract — a \
                zero claim with no evidence is not honoured."
            .to_owned();
    }
    if !basis.covers_this_model {
        return "the cited evidence does not, by its own scope, reach this model \
                (covers_this_model=false)."
            .to_owned();
    }
    if !weight_ok {
        // Only reachable for a published-default basis on a closed-weight model.
        return "the basis is a provider PUBLISHED DEFAULT and this model is closed-weight; a \
                published default (commonly scoped to open models) is not extended to a closed-weight \
                lane automatically."
            .to_owned();
    }
    if !kind_ok {
        return format!(
            "the basis kind is `{}` — a fact about the operator's own account/contract that a \
             research agent cannot verify from public pages — and it is not human_attested.",
            basis.kind.label(),
        );
    }
    // Defensive: some other unmet condition.
    let _ = weight_open;
    "the basis does not satisfy the drafter's conditions for an automatic zero label.".to_owned()
}

/// Build the honest, specific `standard`-override description for the emitted
/// pin. It is a customer-facing claim, so it says what the evidence does NOT
/// establish rather than asserting anything about the provider.
fn override_description(basis: &Basis, weight_open: bool, kind_ok: bool) -> String {
    let core = if basis.kind == BasisKind::PublishedDefault && !weight_open {
        "The cited zero-retention basis is a provider published default, and this model is \
         closed-weight. A published default scoped to open models does not extend to a closed-weight \
         lane, and this could not be established from a public page, so ZeroRouter makes no \
         zero-retention claim for this lane."
    } else if basis.kind.is_account_private() && !kind_ok {
        "The cited zero-retention basis rests on the operator's own arrangement or account setting, \
         which a research agent cannot verify from public pages; absent human attestation, \
         ZeroRouter makes no zero-retention claim for this lane."
    } else if !basis.covers_this_model {
        "The cited zero-retention evidence does not, by its own scope, reach this model, so \
         ZeroRouter makes no zero-retention claim for this lane."
    } else {
        "The cited zero-retention basis does not satisfy the conditions for a zero label, so \
         ZeroRouter makes no zero-retention claim for this lane."
    };
    format!("{core} (Auto-drafted; verify and rewrite before merge.)")
}

// ---------------------------------------------------------------------------
// Retention pin fields and their well-formedness (for the scaffold decision).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PinFields {
    posture: RetentionPosture,
    description: String,
    source_url: String,
    verified: String,
    source_sha256: String,
}

fn pin_from_basis(basis: &Basis, posture: RetentionPosture, verified: &str) -> PinFields {
    PinFields {
        posture,
        // A zero basis is required to have provenance but not necessarily a
        // description; synthesise one rather than emit a blank the loader would
        // reject, so a thin description never spuriously refuses the draft.
        description: nonblank(
            &basis.description,
            "retention basis (see source); description not supplied in the dossier",
        ),
        source_url: basis.source_url.clone(),
        verified: verified.to_owned(),
        source_sha256: basis.source_sha256.clone(),
    }
}

/// A structurally-valid placeholder pin for load-validation scaffolding only —
/// never emitted. It exists so a lane with no usable retention evidence still
/// produces a loadable fragment, letting the loader check the tier's STRUCTURE
/// while the real posture claim lives in the provider pin of the live file.
fn placeholder_pin(posture: RetentionPosture, verified: &str) -> PinFields {
    PinFields {
        posture,
        description: "load-validation scaffold (never emitted); the live [retention.<provider>] \
                      pin carries the real claim"
            .to_owned(),
        source_url: "https://retention-scaffold.invalid/".to_owned(),
        verified: verified.to_owned(),
        source_sha256: "0".repeat(64),
    }
}

fn looks_http(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://") || url.starts_with("http://")
}

fn looks_sha256(digest: &str) -> bool {
    let digest = digest.trim();
    digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())
}

fn nonblank(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Conditional bands: only `min_prompt_tokens` context tiers are lane rates.
// ---------------------------------------------------------------------------

/// The conditional entries that are genuine lane rates: those keyed on
/// `min_prompt_tokens`. Research agents overload the `conditional` list with
/// cache-TTL, batch, region, and time variants that carry no `min_prompt_tokens`
/// — those are not lane rates this drafter models, so they are dropped here
/// rather than emitted as (invalid) conditional rates.
fn context_bands(prices: &Prices) -> Vec<&ConditionalPrice> {
    prices
        .conditional
        .iter()
        .filter(|band| band.min_prompt_tokens.is_some())
        .collect()
}

/// How many `conditional` entries were dropped for carrying no
/// `min_prompt_tokens`.
fn ignored_conditional_count(prices: &Prices) -> usize {
    prices.conditional.len() - context_bands(prices).len()
}

// ---------------------------------------------------------------------------
// Invariant 6 — price sanity flags (warn, never refuse).
// ---------------------------------------------------------------------------

fn price_sanity_flags(prices: &Prices, input: f64, output: f64) -> Vec<String> {
    let mut flags = Vec::new();
    let ignored = ignored_conditional_count(prices);
    if ignored > 0 {
        flags.push(format!(
            "{ignored} conditional pricing variant(s) were IGNORED for carrying no \
             min_prompt_tokens (cache-TTL / batch / region / time tiers are not lane rates); only \
             a min_prompt_tokens context tier becomes a conditional lane rate. Review them by hand \
             if any should shape the lane."
        ));
    }
    if let Some(cached) = prices.cached_input_per_mtok
        && input > 0.0
        && cached < input / 50.0
    {
        flags.push(format!(
            "suspicious cache price: cached_input {cached} is below input/50 ({input}/50); \
             reconcile against an invoice before trusting it."
        ));
    }
    if output < input {
        flags.push(format!(
            "output_per_mtok ({output}) is cheaper than input_per_mtok ({input}) — unusual; verify \
             the two were not transposed."
        ));
    }
    if prices.cache_write_per_mtok.is_some() && prices.cached_input_per_mtok.is_none() {
        flags.push(
            "cache_write_per_mtok is present without cached_input_per_mtok — a lane that sells cache \
             writes normally sells cache reads too; verify."
                .to_owned(),
        );
    }
    if prices.context_window.is_none() {
        flags.push(
            "context_window is absent — the drafted lane declares no window, so clients fall back to \
             a conservative default; supply it if known."
                .to_owned(),
        );
    }
    flags
}

// ---------------------------------------------------------------------------
// TOML rendering — matched to the shapes in config/tiers.toml.
// ---------------------------------------------------------------------------

/// Render the lane stanza: the tier header, the tier `rates` (with any
/// conditional bands), the single candidate with its id/provider/model, its
/// metadata (when a context window is known) and its own `rates` mirroring the
/// tier — the pass-through, basis == sell shape every lane in the file carries —
/// and, when needed, the explicit retention override.
fn render_stanza(
    display_id: &str,
    candidate: &CandidateFacts,
    prices: &Prices,
    input: f64,
    output: f64,
    override_pin: Option<&PinFields>,
) -> String {
    let key = toml_basic_string(display_id);
    let mut out = String::new();

    let _ = writeln!(out, "[tiers.{key}]\n");

    // Tier sell schedule.
    out.push_str(&render_rates_block(
        &format!("tiers.{key}.rates"),
        prices,
        input,
        output,
    ));
    out.push('\n');

    // The single candidate.
    let _ = writeln!(out, "[[tiers.{key}.candidates]]");
    let _ = writeln!(out, "id = {}", toml_basic_string(display_id));
    let _ = writeln!(out, "provider = {}", toml_basic_string(&candidate.provider));
    let _ = writeln!(out, "model = {}", toml_basic_string(&candidate.model));

    // Candidate metadata: only context_window is known from the dossier.
    if let Some(window) = prices.context_window {
        out.push('\n');
        let _ = writeln!(out, "[tiers.{key}.candidates.metadata]");
        let _ = writeln!(out, "context_window = {window}");
    }

    // Candidate cost basis mirrors the tier sell schedule (basis == sell).
    out.push('\n');
    out.push_str(&render_rates_block(
        &format!("tiers.{key}.candidates.rates"),
        prices,
        input,
        output,
    ));

    // The retention override, when the lane must publish standard on a provider
    // whose basis said zero.
    if let Some(pin) = override_pin {
        out.push('\n');
        let _ = writeln!(out, "[tiers.{key}.retention]");
        out.push_str(&render_pin_body(pin));
    }

    out
}

/// Render one `rates` table (and its conditional bands) under `base_header`,
/// keys in the order the file uses: input, cached input, cache write, output.
fn render_rates_block(base_header: &str, prices: &Prices, input: f64, output: f64) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "[{base_header}]");
    let _ = writeln!(out, "input_per_mtok = {}", fmt_price(input));
    if let Some(cached) = prices.cached_input_per_mtok {
        let _ = writeln!(out, "cached_input_per_mtok = {}", fmt_price(cached));
    }
    if let Some(write_rate) = prices.cache_write_per_mtok {
        let _ = writeln!(out, "cache_write_per_mtok = {}", fmt_price(write_rate));
    }
    let _ = writeln!(out, "output_per_mtok = {}", fmt_price(output));

    // Only genuine `min_prompt_tokens` context tiers become conditional lane
    // rates. A context tier missing input/output emits a table the loader then
    // refuses (invariant 5) — the fail-safe: a real repricing tier with a hole
    // in it must refuse, never silently under-price.
    for band in context_bands(prices) {
        let Some(min_prompt_tokens) = band.min_prompt_tokens else {
            continue;
        };
        out.push('\n');
        let _ = writeln!(out, "[[{base_header}.conditional]]");
        let _ = writeln!(out, "min_prompt_tokens = {min_prompt_tokens}");
        if let Some(band_input) = band.input_per_mtok {
            let _ = writeln!(out, "input_per_mtok = {}", fmt_price(band_input));
        }
        if let Some(cached) = band.cached_input_per_mtok {
            let _ = writeln!(out, "cached_input_per_mtok = {}", fmt_price(cached));
        }
        if let Some(write_rate) = band.cache_write_per_mtok {
            let _ = writeln!(out, "cache_write_per_mtok = {}", fmt_price(write_rate));
        }
        if let Some(band_output) = band.output_per_mtok {
            let _ = writeln!(out, "output_per_mtok = {}", fmt_price(band_output));
        }
    }
    out
}

fn render_pin_body(pin: &PinFields) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "posture = \"{}\"", pin.posture.wire_token());
    let _ = writeln!(out, "description = {}", toml_basic_string(&pin.description));
    let _ = writeln!(out, "source_url = {}", toml_basic_string(&pin.source_url));
    let _ = writeln!(out, "verified = {}", toml_basic_string(&pin.verified));
    let _ = writeln!(
        out,
        "source_sha256 = {}",
        toml_basic_string(&pin.source_sha256)
    );
    out
}

/// Format a price as a TOML float, value-preserving and deterministic.
///
/// The shortest round-tripping representation, padded to at least two
/// fractional digits so `2.0` reads as `2.00` like the file's convention — but
/// NEVER rounded, so `0.007` stays `0.007`. This does not byte-match a vendor's
/// own decimal padding for every number, only its value; equality under the
/// TOML parser (and thus load-validation) is exact.
fn fmt_price(value: f64) -> String {
    let shortest = format!("{value}");
    let (integer, fraction) = match shortest.split_once('.') {
        Some((integer, fraction)) => (integer.to_owned(), fraction.to_owned()),
        None => (shortest, String::new()),
    };
    // Right-pad the fractional part with zeros to width 2 (value-preserving).
    let fraction = if fraction.len() < 2 {
        format!("{fraction:0<2}")
    } else {
        fraction
    };
    format!("{integer}.{fraction}")
}

/// A TOML basic string: double-quoted, with the escapes the spec requires.
fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn quote_or_none(value: &str) -> String {
    if value.trim().is_empty() {
        "(none supplied)".to_owned()
    } else {
        format!("\"{value}\"")
    }
}

// ---------------------------------------------------------------------------
// The human-readable draft dossier.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_dossier(
    facts: &FactsDossier,
    input: f64,
    output: f64,
    posture: RetentionPosture,
    provider_posture: Option<RetentionPosture>,
    reasoning: &str,
    inherit_note: &str,
    stanza_toml: &str,
    flags: &[String],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# DRAFT tiers.toml lane — {}\n", facts.display_id);
    out.push_str(
        "> DRAFT — proposed automatically from a facts dossier. Every claim below needs human \
         verification before merge. This bot proposes; it never merges.\n\n",
    );

    out.push_str("## Proposed stanza\n\n```toml\n");
    out.push_str(stanza_toml);
    if !stanza_toml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n\n");

    out.push_str("## Posture decision\n\n");
    let _ = writeln!(out, "- Resolved posture: **{}**", posture.wire_token());
    let _ = writeln!(out, "- Reasoning: {reasoning}");
    let _ = writeln!(
        out,
        "- Live provider pin: {}",
        describe_provider_pin(provider_posture)
    );
    let _ = writeln!(out, "- Pin placement: {inherit_note}");
    match &facts.retention.basis {
        None => out.push_str("- Basis: none supplied.\n"),
        Some(basis) => {
            let _ = writeln!(
                out,
                "- Basis: kind={}, posture={}, covers_this_model={}, human_attested={}, \
                 weight_open={}.",
                basis.kind.label(),
                basis.posture.wire_token(),
                basis.covers_this_model,
                basis.human_attested,
                facts.retention.weight_open,
            );
            let _ = writeln!(out, "  - source_url: {}", quote_or_none(&basis.source_url));
            let _ = writeln!(
                out,
                "  - source_sha256: {}",
                quote_or_none(&basis.source_sha256)
            );
            let _ = writeln!(
                out,
                "  - source_extract: {}",
                quote_or_none(&basis.source_extract)
            );
        }
    }
    match &facts.standard_evidence {
        None => out.push_str("- Standard evidence (retention receipt): none supplied.\n"),
        Some(evidence) => {
            out.push_str("- Standard evidence (retention receipt):\n");
            let _ = writeln!(
                out,
                "  - description: {}",
                quote_or_none(&evidence.description)
            );
            let _ = writeln!(
                out,
                "  - source_url: {}",
                quote_or_none(&evidence.source_url)
            );
            let _ = writeln!(
                out,
                "  - source_sha256: {}",
                quote_or_none(&evidence.source_sha256)
            );
            let _ = writeln!(
                out,
                "  - source_extract: {}",
                quote_or_none(&evidence.source_extract)
            );
        }
    }
    out.push('\n');

    out.push_str("## Prices (each traceable)\n\n");
    let _ = writeln!(out, "- input_per_mtok = {}", fmt_price(input));
    if let Some(cached) = facts.prices.cached_input_per_mtok {
        let _ = writeln!(out, "- cached_input_per_mtok = {}", fmt_price(cached));
    }
    if let Some(write_rate) = facts.prices.cache_write_per_mtok {
        let _ = writeln!(out, "- cache_write_per_mtok = {}", fmt_price(write_rate));
    }
    let _ = writeln!(out, "- output_per_mtok = {}", fmt_price(output));
    match facts.prices.context_window {
        Some(window) => {
            let _ = writeln!(out, "- context_window = {window}");
        }
        None => out.push_str("- context_window = (absent)\n"),
    }
    for band in context_bands(&facts.prices) {
        let min = band.min_prompt_tokens.unwrap_or_default();
        let _ = writeln!(
            out,
            "- conditional above {min} tokens: input {}, output {}",
            band.input_per_mtok
                .map_or("(missing)".to_owned(), fmt_price),
            band.output_per_mtok
                .map_or("(missing)".to_owned(), fmt_price),
        );
    }
    let ignored = ignored_conditional_count(&facts.prices);
    if ignored > 0 {
        let _ = writeln!(
            out,
            "- ({ignored} non-min_prompt_tokens pricing variant(s) ignored — see Flags)"
        );
    }
    let _ = writeln!(
        out,
        "- source_url: {}",
        quote_or_none(&facts.prices.source_url)
    );
    let _ = writeln!(
        out,
        "- source_sha256: {}",
        quote_or_none(&facts.prices.source_sha256)
    );
    let _ = writeln!(
        out,
        "- source_extract: {}\n",
        quote_or_none(&facts.prices.source_extract)
    );

    out.push_str("## Availability\n\n");
    let _ = writeln!(
        out,
        "- invokable: {} — invoke_evidence: {}\n",
        facts.invokable.describe(),
        quote_or_none(&facts.invoke_evidence),
    );

    if !facts.gaps.is_empty() {
        out.push_str("## Researcher-flagged gaps\n\n");
        for gap in &facts.gaps {
            let _ = writeln!(out, "- {gap}");
        }
        out.push('\n');
    }

    out.push_str("## Flags\n\n");
    if flags.is_empty() {
        out.push_str("- none\n");
    } else {
        for flag in flags {
            let _ = writeln!(out, "- {flag}");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Invariant 5 — validate the draft by running it through the real loader.
// ---------------------------------------------------------------------------

/// Why a fragment did not load: a genuine config fault (which REFUSES the draft
/// — invariant 5) versus an environment failure writing the temp file (which is
/// the drafter's own problem, not the dossier's).
#[derive(Debug)]
pub enum FragmentError {
    Io(std::io::Error),
    Config(TierConfigError),
}

impl std::fmt::Display for FragmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "writing the validation fragment: {error}"),
            Self::Config(error) => write!(formatter, "{error}"),
        }
    }
}

/// Run a catalog fragment through the router's own
/// [`crate::config::load_tier_catalog`] — the SAME loader the server uses at
/// every request — by writing it to a temp file and loading it.
///
/// Going through the file-backed loader rather than a string-parse-plus-manual-
/// validate is deliberate: it is the one path guaranteed to apply every rule
/// the router applies (schema, tier-id, provider support, rates, margins,
/// retention-pin well-formedness, withholding, unified synthesis). A drafter
/// that reimplemented "does this load?" would be a second definition to drift.
pub async fn load_fragment(fragment: &str) -> Result<TierCatalog, FragmentError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "zerorouter-draft-pin-{}-{unique}.toml",
        std::process::id()
    ));

    tokio::fs::write(&path, fragment)
        .await
        .map_err(FragmentError::Io)?;
    let result = crate::config::load_tier_catalog(&path).await;
    // Best-effort cleanup; a leftover temp file is not a reason to fail a draft.
    let _ = tokio::fs::remove_file(&path).await;
    result.map_err(FragmentError::Config)
}

// ---------------------------------------------------------------------------
// The whole story: decide, then prove it loads.
// ---------------------------------------------------------------------------

/// The final, load-validated outcome the command renders.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub status: Status,
    pub refuse_reason: Option<String>,
    pub display_id: String,
    pub posture: Option<RetentionPosture>,
    pub stanza_toml: Option<String>,
    pub dossier_markdown: Option<String>,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Draft,
    Refused,
}

impl Outcome {
    fn refused(display_id: &str, reason: String, flags: Vec<String>) -> Self {
        Self {
            status: Status::Refused,
            refuse_reason: Some(reason),
            display_id: display_id.to_owned(),
            posture: None,
            stanza_toml: None,
            dossier_markdown: None,
            flags,
        }
    }

    /// The `--json` view, exactly the documented shape: `{ status,
    /// refuse_reason?, display_id, posture, stanza_toml, dossier_markdown,
    /// flags }`.
    #[must_use]
    pub fn to_json(&self) -> OutcomeJson {
        OutcomeJson {
            status: match self.status {
                Status::Draft => "draft",
                Status::Refused => "refused",
            },
            refuse_reason: self.refuse_reason.clone(),
            display_id: self.display_id.clone(),
            posture: self.posture,
            stanza_toml: self.stanza_toml.clone(),
            dossier_markdown: self.dossier_markdown.clone(),
            flags: self.flags.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OutcomeJson {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refuse_reason: Option<String>,
    pub display_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posture: Option<RetentionPosture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stanza_toml: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dossier_markdown: Option<String>,
    pub flags: Vec<String>,
}

/// Draft, then prove the draft loads. `provider_posture` is the candidate
/// provider's live `[retention.<provider>]` posture (see [`draft`]). `Ok(Outcome)`
/// covers both a draft and a refusal (including a load-validation refusal and the
/// FIX-3 false-zero refusal); `Err` is reserved for an environment failure
/// writing the temp fragment, which is not the dossier's fault.
///
/// # Errors
///
/// Returns the underlying IO error if the temp validation fragment cannot be
/// written.
pub async fn draft_and_validate(
    facts: &FactsDossier,
    verified: &str,
    provider_posture: Option<RetentionPosture>,
) -> Result<Outcome, std::io::Error> {
    let draft = match draft(facts, verified, provider_posture) {
        Decision::Refused { reason } => {
            return Ok(Outcome::refused(&facts.display_id, reason, Vec::new()));
        }
        Decision::Drafted(draft) => draft,
    };

    let fragment = draft.validation_fragment();
    match load_fragment(&fragment).await {
        Ok(catalog) => {
            // A tier that loaded but was WITHHELD for below-cost pricing is not
            // a servable draft. Basis == sell means this should never happen,
            // but a draft that would be withheld must be refused, not shown.
            if let Some(withheld) = catalog.unavailable.get(&draft.display_id) {
                return Ok(Outcome::refused(
                    &draft.display_id,
                    format!(
                        "REFUSED: the drafted lane loaded but was withheld as below-cost: {}",
                        withheld.reason
                    ),
                    draft.flags,
                ));
            }
            if !catalog.tiers.contains_key(&draft.display_id) {
                return Ok(Outcome::refused(
                    &draft.display_id,
                    "REFUSED: the drafted lane did not survive load-validation as a servable tier."
                        .to_owned(),
                    draft.flags,
                ));
            }
            Ok(Outcome {
                status: Status::Draft,
                refuse_reason: None,
                display_id: draft.display_id.clone(),
                posture: Some(draft.posture),
                stanza_toml: Some(draft.stanza_toml.clone()),
                dossier_markdown: Some(draft.dossier_markdown.clone()),
                flags: draft.flags.clone(),
            })
        }
        Err(FragmentError::Config(error)) => Ok(Outcome::refused(
            &draft.display_id,
            format!(
                "REFUSED: the drafted stanza did not load through the router's own tier loader — a \
                 draft the router could not serve must never be produced. Loader error: {error}"
            ),
            draft.flags,
        )),
        Err(FragmentError::Io(error)) => Err(error),
    }
}

/// Resolve the `verified` date: `--verified` wins, else the dossier's own
/// field, and it must be a real ISO date — the drafter never reads the clock.
///
/// # Errors
///
/// Returns an error when neither source supplies a date, or the supplied date
/// is not `YYYY-MM-DD`.
pub fn resolve_verified(
    flag: Option<&str>,
    dossier: Option<&str>,
) -> Result<String, VerifiedError> {
    let candidate = flag
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| dossier.map(str::trim).filter(|value| !value.is_empty()));
    let Some(value) = candidate else {
        return Err(VerifiedError::Missing);
    };
    if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() {
        return Err(VerifiedError::Malformed(value.to_owned()));
    }
    Ok(value.to_owned())
}

#[derive(Debug)]
pub enum VerifiedError {
    Missing,
    Malformed(String),
}

impl std::fmt::Display for VerifiedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                formatter,
                "no verified date: pass --verified YYYY-MM-DD or set \"verified\" in the facts \
                 dossier — the drafter never reads the system clock, so the date must be an input"
            ),
            Self::Malformed(value) => write!(
                formatter,
                "verified date {value:?} is not an ISO calendar date (YYYY-MM-DD)"
            ),
        }
    }
}

impl std::error::Error for VerifiedError {}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid Anthropic (standard provider) dossier: proven invokable, priced
    // with provenance, a standard basis. Tests mutate a clone of this.
    fn baseline() -> FactsDossier {
        FactsDossier {
            candidate: CandidateFacts {
                category: "version-bump".to_owned(),
                provider: "anthropic".to_owned(),
                model: "claude-opus-4-8".to_owned(),
                note: "test".to_owned(),
            },
            display_id: "anthropic/claude-opus-4-8".to_owned(),
            prices: Prices {
                input_per_mtok: Some(5.0),
                output_per_mtok: Some(25.0),
                cached_input_per_mtok: Some(0.5),
                cache_write_per_mtok: Some(6.25),
                context_window: Some(200_000),
                conditional: Vec::new(),
                source_url: "https://www.anthropic.com/pricing".to_owned(),
                source_sha256: "a".repeat(64),
                source_extract: "Claude Opus 4.8: $5 / $25 per Mtok".to_owned(),
            },
            retention: RetentionFacts {
                weight_open: false,
                basis: Some(Basis {
                    kind: BasisKind::PublishedDefault,
                    posture: RetentionPosture::Standard,
                    covers_this_model: true,
                    human_attested: false,
                    description: "Anthropic retains API data up to 30 days.".to_owned(),
                    source_url: "https://privacy.claude.com/".to_owned(),
                    source_sha256: "b".repeat(64),
                    source_extract: "deletes API inputs and outputs within 30 days".to_owned(),
                }),
            },
            standard_evidence: None,
            gaps: Vec::new(),
            invokable: Invokable::Proven(true),
            invoke_evidence: "200 OK from messages.create".to_owned(),
            verified: None,
        }
    }

    const VERIFIED: &str = "2026-09-04";

    /// The live `[retention.<provider>]` posture a test's fixture provider would
    /// carry, mirroring the shipped `config/tiers.toml`: the zero-retention
    /// providers pin `zero`, the ordinary API accounts pin `standard`, and an
    /// unknown provider has no pin. Deriving it from the provider means the
    /// arity-1 helpers below need no per-call posture.
    fn test_provider_pin(provider: &str) -> Option<RetentionPosture> {
        match provider {
            "fireworks" | "vertex" | "bedrock" | "xai" | "groq" | "together" => {
                Some(RetentionPosture::Zero)
            }
            "anthropic" | "openai" | "google" | "azure" => Some(RetentionPosture::Standard),
            _ => None,
        }
    }

    fn drafted(facts: &FactsDossier) -> Draft {
        match draft(
            facts,
            VERIFIED,
            test_provider_pin(&facts.candidate.provider),
        ) {
            Decision::Drafted(draft) => *draft,
            Decision::Refused { reason } => panic!("expected a draft, got refusal: {reason}"),
        }
    }

    fn refusal(facts: &FactsDossier) -> String {
        match draft(
            facts,
            VERIFIED,
            test_provider_pin(&facts.candidate.provider),
        ) {
            Decision::Refused { reason } => reason,
            Decision::Drafted(_) => panic!("expected a refusal, got a draft"),
        }
    }

    async fn validate(facts: &FactsDossier) -> Outcome {
        draft_and_validate(
            facts,
            VERIFIED,
            test_provider_pin(&facts.candidate.provider),
        )
        .await
        .expect("no io error")
    }

    /// A usable standard retention receipt for FIX-2 / FIX-3 tests.
    fn standard_evidence() -> StandardEvidence {
        StandardEvidence {
            description: "Provider retains API inputs and outputs up to 30 days; not zero."
                .to_owned(),
            source_url: "https://example.com/data-retention".to_owned(),
            source_sha256: "d".repeat(64),
            source_extract: "we retain inputs and outputs for up to 30 days".to_owned(),
        }
    }

    // ---- Invariant 1: not invokable ⇒ refuse -------------------------------

    #[test]
    fn invokable_true_is_accepted() {
        // MUTATION: if `is_proven` returned true for non-`true` values, the
        // `false`/`unknown` tests below would stop refusing.
        assert!(matches!(
            draft(&baseline(), VERIFIED, test_provider_pin("anthropic")),
            Decision::Drafted(_)
        ));
    }

    #[test]
    fn invokable_false_refuses() {
        let mut facts = baseline();
        facts.invokable = Invokable::Proven(false);
        let reason = refusal(&facts);
        assert!(reason.contains("not proven invokable"));
        assert!(reason.contains("Sales"), "names the Sales-gate trap");
        assert!(
            reason.contains("200 OK from messages.create"),
            "echoes invoke_evidence"
        );
    }

    #[test]
    fn invokable_unknown_refuses() {
        let mut facts = baseline();
        facts.invokable = Invokable::Unknown("unknown".to_owned());
        assert!(refusal(&facts).contains("not proven invokable"));
    }

    // ---- Invariant 2: missing required prices ⇒ refuse ---------------------

    #[test]
    fn missing_input_price_refuses() {
        let mut facts = baseline();
        facts.prices.input_per_mtok = None;
        assert!(refusal(&facts).contains("input_per_mtok"));
    }

    #[test]
    fn missing_output_price_refuses() {
        let mut facts = baseline();
        facts.prices.output_per_mtok = None;
        assert!(refusal(&facts).contains("output_per_mtok"));
    }

    // ---- Invariant 3: price without provenance ⇒ refuse --------------------

    #[test]
    fn price_without_source_url_refuses() {
        let mut facts = baseline();
        facts.prices.source_url = String::new();
        assert!(refusal(&facts).contains("source_url"));
    }

    #[test]
    fn price_without_source_sha_refuses() {
        let mut facts = baseline();
        facts.prices.source_sha256 = "   ".to_owned();
        assert!(refusal(&facts).contains("source_sha256"));
    }

    #[test]
    fn price_without_extract_refuses() {
        let mut facts = baseline();
        facts.prices.source_extract = String::new();
        assert!(refusal(&facts).contains("source_extract"));
    }

    // ---- Invariant 4: posture resolution (fail-safe to standard) -----------

    fn zero_published_default_open() -> FactsDossier {
        // An OPEN-weight model with a published-default zero basis: the one
        // shape the drafter may auto-zero.
        let mut facts = baseline();
        facts.candidate.provider = "fireworks".to_owned();
        facts.candidate.model = "accounts/fireworks/models/qwen3p7-plus".to_owned();
        facts.display_id = "fireworks/qwen3.7-plus".to_owned();
        facts.prices.cache_write_per_mtok = None; // fireworks is not an Anthropic-dialect plane
        facts.retention.weight_open = true;
        facts.retention.basis = Some(Basis {
            kind: BasisKind::PublishedDefault,
            posture: RetentionPosture::Zero,
            covers_this_model: true,
            human_attested: false,
            description: "Fireworks has Zero Data Retention by default for open models.".to_owned(),
            source_url: "https://docs.fireworks.ai/guides/security_compliance/data_handling"
                .to_owned(),
            source_sha256: "c".repeat(64),
            source_extract: "prompt and generation data exist only in volatile memory".to_owned(),
        });
        facts
    }

    #[test]
    fn published_default_open_weight_yields_zero() {
        // MUTATION: if the zero_ok conjunction dropped `basis.posture == zero`
        // or `covers_this_model`, the downgrade tests below would still pass but
        // several standard-expecting cases would flip to zero.
        let draft = drafted(&zero_published_default_open());
        assert_eq!(draft.posture, RetentionPosture::Zero);
        assert!(!draft.override_emitted, "a zero lane needs no override");
    }

    #[test]
    fn closed_weight_published_default_zero_downgrades_to_standard_with_override() {
        // The qwen3.8-max scope case: a closed-weight model may not ride an
        // open-scoped published-default zero pin.
        // MUTATION: if the weight guard (`weight_open || account_private`) were
        // dropped, this would emit `zero` — a false customer-facing claim.
        let mut facts = zero_published_default_open();
        facts.retention.weight_open = false;
        let draft = drafted(&facts);
        assert_eq!(draft.posture, RetentionPosture::Standard);
        assert!(
            draft.override_emitted,
            "standard on a zero-basis provider must emit an explicit override"
        );
        assert!(draft.stanza_toml.contains(".retention]"));
        assert!(draft.stanza_toml.contains("posture = \"standard\""));
    }

    #[test]
    fn enforced_zero_without_human_attestation_downgrades_to_standard() {
        // Basis 2 (enforced account setting) without human_attested: a research
        // agent cannot verify the account, so it may not auto-zero.
        // MUTATION: if `kind_ok` dropped the `human_attested` requirement for
        // account-private bases, this would emit zero.
        let mut facts = zero_published_default_open();
        facts.retention.basis.as_mut().unwrap().kind = BasisKind::Enforced;
        facts.retention.basis.as_mut().unwrap().human_attested = false;
        let draft = drafted(&facts);
        assert_eq!(draft.posture, RetentionPosture::Standard);
        assert!(draft.override_emitted);
    }

    #[test]
    fn enforced_zero_with_human_attestation_yields_zero_even_closed_weight() {
        // The gemini-on-vertex[zero] shape: a closed-weight model on an enforced
        // account setting a human attested to. Basis 1/2 CAN cover a closed
        // model — only a published default cannot.
        let mut facts = zero_published_default_open();
        facts.candidate.provider = "vertex".to_owned();
        facts.candidate.model = "gemini-3.8-flash".to_owned();
        facts.display_id = "vertex/gemini-3.8-flash".to_owned();
        facts.retention.weight_open = false;
        let basis = facts.retention.basis.as_mut().unwrap();
        basis.kind = BasisKind::Enforced;
        basis.human_attested = true;
        let draft = drafted(&facts);
        assert_eq!(draft.posture, RetentionPosture::Zero);
        assert!(!draft.override_emitted);
    }

    #[test]
    fn covers_this_model_false_downgrades_to_standard() {
        let mut facts = zero_published_default_open();
        facts.retention.basis.as_mut().unwrap().covers_this_model = false;
        assert_eq!(drafted(&facts).posture, RetentionPosture::Standard);
    }

    #[test]
    fn no_basis_yields_standard_and_flags_needs_evidence() {
        let mut facts = baseline();
        facts.retention.basis = None;
        let draft = drafted(&facts);
        assert_eq!(draft.posture, RetentionPosture::Standard);
        assert!(!draft.override_emitted);
        assert!(
            draft
                .flags
                .iter()
                .any(|f| f.contains("NEEDS RETENTION EVIDENCE")),
            "a lane with no basis must flag it"
        );
    }

    #[test]
    fn zero_basis_without_provenance_never_becomes_zero() {
        // A zero claim with no evidence must never be honoured.
        let mut facts = zero_published_default_open();
        facts.retention.basis.as_mut().unwrap().source_extract = String::new();
        assert_eq!(drafted(&facts).posture, RetentionPosture::Standard);
    }

    // ---- Invariant 6: price sanity flags -----------------------------------

    #[test]
    fn suspicious_cache_price_is_flagged() {
        let mut facts = baseline();
        facts.prices.input_per_mtok = Some(1.0);
        facts.prices.cached_input_per_mtok = Some(0.007); // deepseek-v4-flash case
        let draft = drafted(&facts);
        assert!(
            draft
                .flags
                .iter()
                .any(|f| f.contains("suspicious cache price"))
        );
    }

    #[test]
    fn output_cheaper_than_input_is_flagged() {
        let mut facts = baseline();
        facts.prices.input_per_mtok = Some(10.0);
        facts.prices.output_per_mtok = Some(2.0);
        let draft = drafted(&facts);
        assert!(draft.flags.iter().any(|f| f.contains("cheaper than input")));
    }

    #[test]
    fn cache_write_without_cached_input_is_flagged() {
        let mut facts = baseline();
        facts.prices.cached_input_per_mtok = None;
        // keep cache_write present
        let draft = drafted(&facts);
        assert!(draft.flags.iter().any(|f| f.contains("cache_write")));
    }

    #[test]
    fn absent_context_window_is_flagged() {
        let mut facts = baseline();
        facts.prices.context_window = None;
        let draft = drafted(&facts);
        assert!(draft.flags.iter().any(|f| f.contains("context_window")));
    }

    // ---- Invariant 7: determinism ------------------------------------------

    #[test]
    fn same_facts_produce_byte_identical_output() {
        let facts = zero_published_default_open();
        let first = drafted(&facts);
        let second = drafted(&facts);
        assert_eq!(first.stanza_toml, second.stanza_toml);
        assert_eq!(first.dossier_markdown, second.dossier_markdown);
        assert_eq!(first.flags, second.flags);
    }

    // ---- The emitted TOML shape --------------------------------------------

    #[test]
    fn stanza_carries_candidate_rates_and_conditional() {
        // The candidate MUST carry its own rates or the loader rejects it; the
        // brief's output template omitted them, but every real lane has them.
        let mut facts = baseline();
        facts.prices.conditional = vec![ConditionalPrice {
            min_prompt_tokens: Some(272_000),
            input_per_mtok: Some(10.0),
            output_per_mtok: Some(50.0),
            cached_input_per_mtok: Some(1.0),
            cache_write_per_mtok: None,
        }];
        let draft = drafted(&facts);
        assert!(draft.stanza_toml.contains(".candidates.rates]"));
        assert!(draft.stanza_toml.contains(".rates.conditional]]"));
        assert!(draft.stanza_toml.contains("min_prompt_tokens = 272000"));
        assert!(draft.stanza_toml.contains("context_window = 200000"));
    }

    #[test]
    fn fmt_price_preserves_value_and_pads_to_two_places() {
        assert_eq!(fmt_price(2.0), "2.00");
        assert_eq!(fmt_price(0.2), "0.20");
        assert_eq!(fmt_price(0.007), "0.007");
        assert_eq!(fmt_price(0.04), "0.04");
        assert_eq!(fmt_price(1.25), "1.25");
    }

    // ---- Invariant 5: the draft must load through the real loader ----------

    #[tokio::test]
    async fn a_valid_draft_loads_through_the_real_loader() {
        let outcome = validate(&zero_published_default_open()).await;
        assert_eq!(
            outcome.status,
            Status::Draft,
            "reason: {:?}",
            outcome.refuse_reason
        );
        assert_eq!(outcome.posture, Some(RetentionPosture::Zero));
    }

    #[tokio::test]
    async fn the_anthropic_golden_lane_validates_as_standard() {
        let outcome = validate(&baseline()).await;
        assert_eq!(
            outcome.status,
            Status::Draft,
            "reason: {:?}",
            outcome.refuse_reason
        );
        assert_eq!(outcome.posture, Some(RetentionPosture::Standard));
    }

    /// A vertex/gemini-3.8-flash fixture in the REAL dossier's shape: `basis:
    /// null` (Vertex zero retention is an enforced project setting a research
    /// agent cannot verify), closed-weight, no Anthropic-dialect cache write.
    fn gemini_basis_null() -> FactsDossier {
        let mut facts = baseline();
        facts.candidate.category = "version-bump".to_owned();
        facts.candidate.provider = "vertex".to_owned();
        facts.candidate.model = "gemini-3.8-flash".to_owned();
        facts.display_id = "vertex/gemini-3.8-flash".to_owned();
        facts.prices.cache_write_per_mtok = None;
        facts.retention = RetentionFacts {
            weight_open: false,
            basis: None,
        };
        facts.standard_evidence = None;
        facts
    }

    #[tokio::test]
    async fn the_gemini_vertex_golden_lane_validates_as_a_standard_override() {
        // FIX 3: the REAL gemini dossier has basis:null on vertex, whose LIVE
        // pin is `zero`. A plain standard lane would inherit that zero — a false
        // claim. WITH a standard_evidence receipt, the drafter pins `standard`
        // EXPLICITLY, and that override must load.
        let mut facts = gemini_basis_null();
        facts.standard_evidence = Some(standard_evidence());
        let outcome = validate(&facts).await;
        assert_eq!(
            outcome.status,
            Status::Draft,
            "reason: {:?}",
            outcome.refuse_reason
        );
        assert_eq!(outcome.posture, Some(RetentionPosture::Standard));
        let stanza = outcome.stanza_toml.unwrap();
        assert!(
            stanza.contains(".retention]"),
            "an explicit standard override must be emitted, not inherited"
        );
        assert!(stanza.contains("posture = \"standard\""));
    }

    #[tokio::test]
    async fn the_gemini_vertex_golden_without_standard_evidence_refuses() {
        // FIX 3: the REAL gemini dossier as delivered — basis:null AND no
        // standard_evidence — on a zero provider. There is no receipt to pin
        // standard, so the drafter REFUSES rather than emit a false zero.
        // MUTATION: if the standard-on-zero-provider branch fell through to a
        // plain lane instead of refusing, this lane would inherit vertex's zero.
        let outcome = validate(&gemini_basis_null()).await;
        assert_eq!(outcome.status, Status::Refused);
        assert!(
            outcome
                .refuse_reason
                .unwrap()
                .contains("inherit an unearned zero")
        );
    }

    #[tokio::test]
    async fn an_enforced_attested_zero_lane_validates_as_zero() {
        // The zero path still validates end-to-end: a closed-weight model on an
        // enforced account setting a human attested to, on a zero provider.
        let mut facts = gemini_basis_null();
        facts.retention.basis = Some(Basis {
            kind: BasisKind::Enforced,
            posture: RetentionPosture::Zero,
            covers_this_model: true,
            human_attested: true,
            description: "Vertex project configured for zero data retention.".to_owned(),
            source_url: "https://cloud.google.com/vertex-ai/zdr".to_owned(),
            source_sha256: "e".repeat(64),
            source_extract: "no request or response data is retained".to_owned(),
        });
        let outcome = validate(&facts).await;
        assert_eq!(
            outcome.status,
            Status::Draft,
            "reason: {:?}",
            outcome.refuse_reason
        );
        assert_eq!(outcome.posture, Some(RetentionPosture::Zero));
    }

    #[tokio::test]
    async fn the_closed_weight_override_lane_validates_as_standard() {
        // qwen3.8-max shape: closed-weight, published-default zero basis, so an
        // explicit standard override is emitted AND must load.
        let mut facts = zero_published_default_open();
        facts.retention.weight_open = false;
        let outcome = validate(&facts).await;
        assert_eq!(
            outcome.status,
            Status::Draft,
            "reason: {:?}",
            outcome.refuse_reason
        );
        assert_eq!(outcome.posture, Some(RetentionPosture::Standard));
        assert!(outcome.stanza_toml.unwrap().contains(".retention]"));
    }

    #[test]
    fn a_brief_shaped_json_dossier_deserializes_and_drafts() {
        // The wire contract: snake_case field names (matching tiers.toml), a
        // kebab-case `published-default` kind, a kebab-case category string, and
        // lowercase postures. Building structs in Rust would never exercise the
        // serde field names, so this is the regression guard for exactly that.
        let json = r#"{
          "candidate": {
            "category": "version-bump",
            "provider": "fireworks",
            "model": "accounts/fireworks/models/qwen3p7-plus",
            "note": "higher qwen on fireworks"
          },
          "display_id": "fireworks/qwen3.7-plus",
          "prices": {
            "input_per_mtok": 0.40,
            "output_per_mtok": 1.60,
            "cached_input_per_mtok": 0.08,
            "cache_write_per_mtok": null,
            "context_window": 262144,
            "conditional": [],
            "source_url": "https://docs.fireworks.ai/serverless/pricing",
            "source_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_extract": "Qwen 3.7 Plus: $0.40 / $1.60 per Mtok"
          },
          "retention": {
            "weight_open": true,
            "basis": {
              "kind": "published-default",
              "posture": "zero",
              "covers_this_model": true,
              "human_attested": false,
              "description": "Fireworks has Zero Data Retention by default for open models.",
              "source_url": "https://docs.fireworks.ai/guides/security_compliance/data_handling",
              "source_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "source_extract": "prompt and generation data exist only in volatile memory"
            }
          },
          "invokable": true,
          "invoke_evidence": "200 OK from an open-weight chat completion"
        }"#;
        let facts: FactsDossier = serde_json::from_str(json).expect("brief-shaped JSON parses");
        assert_eq!(facts.candidate.category, "version-bump");
        assert!(facts.invokable.is_proven());
        assert_eq!(facts.prices.input_per_mtok, Some(0.40));
        assert_eq!(
            facts.retention.basis.as_ref().unwrap().kind,
            BasisKind::PublishedDefault
        );
        // And it drafts a zero lane (open-weight, published-default).
        assert_eq!(drafted(&facts).posture, RetentionPosture::Zero);
    }

    #[tokio::test]
    async fn an_unsupported_provider_is_refused_by_the_loader() {
        // Invariant 5 catches what the pure core does not: a provider the router
        // cannot dispatch on.
        let mut facts = baseline();
        facts.candidate.provider = "not-a-real-provider".to_owned();
        facts.display_id = "not-a-real-provider/model".to_owned();
        let outcome = validate(&facts).await;
        assert_eq!(outcome.status, Status::Refused);
        assert!(
            outcome
                .refuse_reason
                .unwrap()
                .contains("did not load through the router's own tier loader")
        );
    }

    #[tokio::test]
    async fn a_cache_write_on_a_non_anthropic_plane_is_refused_by_the_loader() {
        // A fireworks lane cannot sell cache writes; the loader refuses it, and
        // the drafter surfaces that as a refusal rather than a bad draft.
        let mut facts = zero_published_default_open();
        facts.prices.cached_input_per_mtok = Some(1.0);
        facts.prices.cache_write_per_mtok = Some(2.5);
        let outcome = validate(&facts).await;
        assert_eq!(outcome.status, Status::Refused);
    }

    #[tokio::test]
    async fn the_golden_refusal_unknown_invokable() {
        let mut facts = baseline();
        facts.invokable = Invokable::Unknown("unknown".to_owned());
        let outcome = validate(&facts).await;
        assert_eq!(outcome.status, Status::Refused);
        assert!(outcome.stanza_toml.is_none());
    }

    // ---- FIX 1: tolerate research-agent input -----------------------------

    #[test]
    fn stray_descriptive_fields_are_tolerated() {
        // A real research agent decorates the dossier with fields a strict schema
        // never anticipated: a top-level extra, a per-field `note`, and
        // `label`/`note` on price bands. None may kill the draft.
        // MUTATION: restoring `deny_unknown_fields` makes this fail to parse.
        let json = r#"{
          "candidate": { "category": "new", "provider": "fireworks",
            "model": "accounts/fireworks/models/qwen3p7-plus", "note": "hi" },
          "display_id": "fireworks/qwen3.7-plus",
          "researcher_confidence": "high",
          "prices": {
            "input_per_mtok": 0.40, "output_per_mtok": 1.60,
            "cached_input_per_mtok": 0.08,
            "context_window": 262144,
            "conditional": [
              { "label": "1h cache write", "cache_write_per_mtok": 10.0,
                "note": "not a min_prompt_tokens tier" }
            ],
            "source_url": "https://docs.fireworks.ai/serverless/pricing",
            "source_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_extract": "Qwen 3.7 Plus: $0.40 / $1.60"
          },
          "retention": {
            "weight_open": true,
            "basis": {
              "kind": "published-default", "posture": "zero",
              "covers_this_model": true, "note": "open models only",
              "description": "ZDR by default for open models.",
              "source_url": "https://docs.fireworks.ai/data",
              "source_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "source_extract": "data exists only in volatile memory"
            }
          },
          "invokable": true,
          "invoke_evidence": "200 OK"
        }"#;
        let facts: FactsDossier =
            serde_json::from_str(json).expect("stray fields must be tolerated, not rejected");
        // Still drafts correctly: open-weight published-default zero on a zero
        // provider → a plain zero lane.
        let draft = drafted(&facts);
        assert_eq!(draft.posture, RetentionPosture::Zero);
        // The label/note-only conditional entry carried no min_prompt_tokens and
        // is ignored, not emitted.
        assert!(!draft.stanza_toml.contains(".rates.conditional]]"));
        assert!(draft.flags.iter().any(|f| f.contains("IGNORED")));
    }

    #[test]
    fn a_mistyped_required_price_still_refuses() {
        // With lenient parsing, a misspelled REQUIRED key is dropped, so the
        // field reads as absent — and absent required prices REFUSE (never a
        // silently-wrong lane). This is the safety the coordinator required.
        let json = r#"{
          "candidate": { "category": "new", "provider": "anthropic", "model": "m" },
          "display_id": "anthropic/m",
          "prices": {
            "input_per_mtokk": 5.0, "output_per_mtok": 25.0,
            "source_url": "https://x/", "source_sha256": "aa", "source_extract": "x"
          },
          "retention": { "weight_open": false },
          "invokable": true
        }"#;
        let facts: FactsDossier = serde_json::from_str(json).expect("parses (lenient)");
        assert_eq!(
            facts.prices.input_per_mtok, None,
            "the typo'd key is dropped"
        );
        assert!(refusal(&facts).contains("input_per_mtok"));
    }

    #[test]
    fn conditional_without_min_prompt_tokens_is_ignored_not_emitted() {
        let mut facts = baseline();
        facts.prices.conditional = vec![
            ConditionalPrice {
                min_prompt_tokens: None,
                input_per_mtok: Some(2.5),
                output_per_mtok: Some(12.5),
                cached_input_per_mtok: None,
                cache_write_per_mtok: None,
            },
            ConditionalPrice {
                min_prompt_tokens: None,
                input_per_mtok: Some(10.0),
                output_per_mtok: Some(50.0),
                cached_input_per_mtok: None,
                cache_write_per_mtok: None,
            },
        ];
        let draft = drafted(&facts);
        assert!(
            !draft.stanza_toml.contains(".rates.conditional]]"),
            "no min_prompt_tokens band means no emitted conditional rate"
        );
        assert!(draft.flags.iter().any(|f| f.contains("2 conditional")));
    }

    // ---- FIX 2: consume standard_evidence and gaps -------------------------

    #[test]
    fn standard_evidence_suppresses_the_needs_evidence_flag_and_is_surfaced() {
        // basis:null on a STANDARD provider would normally flag NEEDS RETENTION
        // EVIDENCE; a standard_evidence receipt is the evidence, so the flag is
        // suppressed and the receipt is shown in the dossier.
        let mut facts = baseline();
        facts.retention = RetentionFacts {
            weight_open: false,
            basis: None,
        };
        facts.standard_evidence = Some(standard_evidence());
        let draft = drafted(&facts);
        assert_eq!(draft.posture, RetentionPosture::Standard);
        assert!(
            !draft
                .flags
                .iter()
                .any(|f| f.contains("NEEDS RETENTION EVIDENCE")),
            "a present standard receipt suppresses the gap flag"
        );
        assert!(
            draft
                .dossier_markdown
                .contains("Standard evidence (retention receipt)")
        );
        assert!(draft.dossier_markdown.contains("up to 30 days"));
    }

    #[test]
    fn gaps_are_surfaced_in_the_dossier() {
        let mut facts = baseline();
        facts.gaps = vec!["Retention: no bot-establishable zero basis.".to_owned()];
        let draft = drafted(&facts);
        assert!(draft.dossier_markdown.contains("Researcher-flagged gaps"));
        assert!(
            draft
                .dossier_markdown
                .contains("no bot-establishable zero basis")
        );
    }

    #[tokio::test]
    async fn a_real_shaped_anthropic_dossier_drafts_a_standard_plain_lane() {
        // The shape the real research agent emits for claude-opus-4-8: basis
        // null, a standard_evidence receipt, gaps, and conditional variants with
        // no min_prompt_tokens (1h-cache / batch / fast-mode). Anthropic's live
        // pin is standard, so a plain standard lane is correct and validates.
        let json = r#"{
          "candidate": { "category": "new", "provider": "anthropic",
            "model": "claude-opus-4-8", "note": "legacy-but-available GA" },
          "display_id": "anthropic/claude-opus-4-8",
          "prices": {
            "input_per_mtok": 5.0, "output_per_mtok": 25.0,
            "cached_input_per_mtok": 0.5, "cache_write_per_mtok": 6.25,
            "context_window": 1000000,
            "conditional": [
              { "label": "1h cache write", "cache_write_per_mtok": 10.0, "note": "TTL variant" },
              { "label": "Batch API", "input_per_mtok": 2.5, "output_per_mtok": 12.5 }
            ],
            "source_url": "https://platform.claude.com/docs/en/about-claude/pricing",
            "source_sha256": "a5e896b765ff04ca5c0431d94c3eb83fd237af881bdae539a256205fecaa66a4",
            "source_extract": "Claude Opus 4.8 $5 / MTok ... $25 / MTok"
          },
          "retention": { "weight_open": false, "basis": null },
          "standard_evidence": {
            "description": "Anthropic deletes API inputs and outputs within 30 days; zero only by signed agreement.",
            "source_url": "https://privacy.claude.com/en/articles/7996866",
            "source_sha256": "8ec3ae6afa8c7639ae015af62f455f2e414107712df72fe2747a16c9371b4335",
            "source_extract": "we automatically delete inputs and outputs on our backend within 30 days"
          },
          "gaps": ["Retention: zero only by signed agreement (basis kind signed, human_attested)."],
          "invokable": true,
          "invoke_evidence": "Official model page lists it as legacy-but-available GA"
        }"#;
        let facts: FactsDossier = serde_json::from_str(json).expect("real anthropic shape parses");
        let outcome = validate(&facts).await;
        assert_eq!(
            outcome.status,
            Status::Draft,
            "reason: {:?}",
            outcome.refuse_reason
        );
        assert_eq!(outcome.posture, Some(RetentionPosture::Standard));
        let stanza = outcome.stanza_toml.unwrap();
        assert!(
            !stanza.contains(".retention]"),
            "a standard lane on a standard provider needs no override"
        );
        assert!(
            !stanza.contains(".rates.conditional]]"),
            "no real context tier"
        );
        assert!(
            !outcome
                .flags
                .iter()
                .any(|f| f.contains("NEEDS RETENTION EVIDENCE")),
            "standard_evidence is the receipt"
        );
        assert!(outcome.flags.iter().any(|f| f.contains("IGNORED")));
    }

    // ---- verified-date resolution ------------------------------------------

    #[test]
    fn verified_prefers_flag_then_dossier_then_errors() {
        assert_eq!(
            resolve_verified(Some("2026-09-04"), Some("2026-01-01")).unwrap(),
            "2026-09-04"
        );
        assert_eq!(
            resolve_verified(None, Some("2026-01-01")).unwrap(),
            "2026-01-01"
        );
        assert!(matches!(
            resolve_verified(None, None),
            Err(VerifiedError::Missing)
        ));
        assert!(matches!(
            resolve_verified(Some("nope"), None),
            Err(VerifiedError::Malformed(_))
        ));
    }
}
