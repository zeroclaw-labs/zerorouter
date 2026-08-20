use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use crate::provider::{
    ChatRequest, ChatResponse, ModelProvider, StreamEvent, StreamOptions, StreamResult,
};
use futures_util::stream::BoxStream;
use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::TierCandidate,
    wire::{AnthropicWire, ChatCompletionsWire, OpenAiResponsesWire},
};

const PROVIDER_INVENTORY_JSON: &str = include_str!("../config/providers.json");

/// Env var naming an operator-supplied provider inventory, layered over the
/// shipped one at startup (edge mode, stage 2:
/// `docs/design/edge-mode-local-rung.md`).
///
/// Shaped exactly like [`crate::config::TIER_CONFIG_PATH_ENV`] — an env var
/// naming a file — because that is this repo's existing answer to "the
/// operator needs to say something the shipped file cannot". The two differ in
/// one deliberate way: the tier path *replaces* the shipped catalog, while
/// this one *adds to* the shipped inventory. Replacement would let a
/// deployment redefine `openai` to point somewhere else, which is the shape of
/// a credential-exfiltration bug rather than a configuration feature; an entry
/// whose key collides with a shipped one is refused (see
/// [`ProviderInventory::assembled`]).
pub const PROVIDER_INVENTORY_PATH_ENV: &str = "ZEROROUTER_PROVIDERS_PATH";

#[derive(Debug, Deserialize)]
struct ProviderInventory {
    providers: Vec<ProviderMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
// A misspelled field must not silently default. `"credentail": "none"` would
// otherwise leave `credential` at `required` and the typo invisible — and the
// two fields this struct gained in stage 2 are both safety declarations whose
// absence is meaningful, which is exactly the shape where a silent default is
// worst.
#[serde(deny_unknown_fields)]
struct ProviderMetadata {
    key: String,
    adapter: ProviderAdapter,
    /// Whether requests to this upstream cost anyone money.
    ///
    /// Defaults to [`SettlementDeclaration::Metered`], so the shipped file and
    /// every entry written before this field existed keep meaning what they
    /// meant. Declaring `free` is the ONLY way a candidate on this provider may
    /// be priced at $0 — see [`provider_settles_free`] for why the adapter
    /// cannot answer that question.
    #[serde(default)]
    settlement: SettlementDeclaration,
    /// Whether this upstream takes a credential at all. Defaults to
    /// [`CredentialRequirement::Required`], so the shipped file — and every
    /// entry written before this field existed — keeps meaning exactly what it
    /// meant.
    #[serde(default)]
    credential: CredentialRequirement,
    /// The environment variable holding the credential. Required when
    /// `credential` is `required`, and refused when it is `none`: an entry
    /// that claims to need no key while naming the variable holding one is
    /// contradicting itself, and guessing which half is the truth is how a
    /// credential ends up unused or a keyless upstream ends up authenticated.
    #[serde(default)]
    credential_env: Option<String>,
    /// The deployment's name for the secret behind `credential_env`. Same
    /// present-iff-required rule, for the same reason: there is no secret to
    /// name when there is no credential.
    #[serde(default)]
    secret_name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// The environment variable holding the AWS region this upstream's
    /// endpoint is addressed in, for an entry whose `base_url` carries the
    /// [`REGION_PLACEHOLDER`].
    ///
    /// Exists for exactly one shape of upstream: a regional endpoint whose
    /// host contains the region, where hardcoding one region into the shipped
    /// inventory would silently send a deployment in another region's traffic —
    /// and its prompts — across a boundary the operator did not choose. Bedrock
    /// is that shape (`bedrock-mantle.{region}.api.aws`), and it is in-region
    /// only, so the region is not a detail the endpoint can shrug off.
    ///
    /// Present iff the `base_url` interpolates, enforced both ways by
    /// [`ProviderMetadata::validate_region`]. A placeholder with nothing to fill
    /// it would dial a host named `{region}`; a `region_env` with no placeholder
    /// names a variable that could never be read, which is a claim about
    /// configuration that is not true.
    #[serde(default)]
    region_env: Option<String>,
    /// Why the public catalog `admin catalog-drift` reconciles against cannot
    /// price this upstream, and where its rates really come from.
    ///
    /// **Presence is the exemption; the text is the justification, and it is
    /// required rather than decorative** — the same rule a retention pin's
    /// evidence fields follow. A declaration with nothing behind it would let
    /// "we could not be bothered to check" wear the same shape as a real,
    /// argued gap, and this field disables the one alarm that guards margin.
    /// Blank is refused at load ([`ProviderMetadata::validate_reconciliation`]).
    ///
    /// It exists for a fault that has no honest fix inside the reconciliation:
    /// models.dev DOES carry Bedrock, under `amazon-bedrock`, and prices the
    /// bare id `anthropic.claude-sonnet-5` at the GLOBAL cross-region rate.
    /// ZeroRouter dials the mantle endpoint, which is in-region only and cannot
    /// use a global inference profile, so AWS meters the unqualified `_standard`
    /// SKU — 10% dearer. Joining the two would have reported `ok` over a basis
    /// 10% under the invoice, which is precisely the silent-margin failure
    /// [`crate::drift`] was written to break. Mapping the provider key to
    /// `amazon-bedrock` was tried and rejected for that reason: a green row
    /// against the wrong SKU class is worse than an admitted gap.
    ///
    /// It is deliberately NOT a way to skip a lane quietly. A declaring
    /// provider's candidates still appear in every report, under their own
    /// verdict, with this text printed beside them on every run.
    #[serde(default)]
    unreconcilable_reason: Option<String>,
}

/// The token a regional `base_url` writes where its region belongs.
const REGION_PLACEHOLDER: &str = "{region}";

/// Whether a deployment holds everything one upstream needs.
enum Dispatchable {
    Ready {
        credential: String,
        /// The endpoint to dial, region already substituted. `None` means the
        /// wire uses its own default.
        endpoint: Option<String>,
    },
    /// The environment variable whose absence disqualifies this provider.
    /// Empty only for a malformed entry that validation already refuses.
    Missing { env: String },
}

/// Whether this deployment can dispatch to `provider` at all.
///
/// **The catalog's gate, and it must answer exactly as route construction
/// does** — both read [`ProviderMetadata::dispatchable`], which is why that
/// function exists rather than the two growing their own conditions.
///
/// An unknown provider is not dispatchable. That is the conservative answer for
/// the caller that matters: `/v1/models` would otherwise advertise a lane whose
/// upstream this build has no entry for, and the catalog validator already
/// refuses such a file at load, so this branch is reachable only if the two ever
/// disagree.
#[must_use]
pub fn provider_is_dispatchable(provider: &str) -> bool {
    provider_is_dispatchable_with(provider, read_credential)
}

/// [`provider_is_dispatchable`], with the environment supplied.
///
/// The seam exists so the property that matters can be tested for what it is:
/// this function and [`build_with_credentials`] must reach the SAME verdict
/// from the same environment. Reading the process environment in both would
/// make that untestable, and untestable is how the two drift.
fn provider_is_dispatchable_with<F>(provider: &str, read_env: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory.provider(provider).is_some_and(|metadata| {
            matches!(metadata.dispatchable(read_env), Dispatchable::Ready { .. })
        })
    })
}

/// Whether an upstream's traffic costs anyone money.
///
/// The free lane is entered by this declaration and by nothing else — not by
/// the adapter, and not by the absence of a credential. Both of those were
/// considered and both are wrong:
///
/// - **The adapter cannot answer it.** `chat_completions` is dual-use by the
///   design's own scope (`docs/design/edge-mode-local-rung.md`): it serves the
///   operator's local models AND a hosted ZeroRouter `/v1` taking metered burst
///   traffic. "On the local wire" and "bills nobody" are different claims, and
///   only one of them is about money.
/// - **Credential presence cannot answer it either.** A local vLLM behind a
///   bearer token is an ordinary deployment; it is free because its operator
///   runs it, not because it happens to be unauthenticated. A free provider may
///   be keyless or credentialed.
///
/// So it is stated twice, deliberately: once here, by the provider, and once in
/// `tiers.toml`, by the candidate's $0 price. [`crate::config::TierCandidate::is_free`]
/// — the key stage 3's metering skip is specified to read — requires both.
///
/// What this does NOT do, and must not be described as doing: make it
/// impossible to run a paid model through the free lane. An operator who
/// declares `settlement: free` on an upstream that bills them has lied to their
/// own configuration, and no validation here can detect that — the router has
/// no way to know what an arbitrary endpoint charges. What the double
/// declaration buys is that it cannot happen by ACCIDENT: not by a typo in a
/// rate, not by picking the wrong adapter, not by forgetting a credential. The
/// free lane is always something someone wrote down on purpose.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SettlementDeclaration {
    #[default]
    Metered,
    Free,
}

/// Whether an upstream authenticates.
///
/// The keyless case exists because most local inference servers — llama.cpp,
/// Ollama, LM Studio, vLLM started without `--api-key` — take no credential at
/// all, while [`build_with_credentials`] drops any candidate whose credential
/// is missing from the environment. Without a way to *say* "there is no
/// credential", an operator's only options would be to invent a fake key (so a
/// genuinely missing paid key becomes indistinguishable from a deliberately
/// absent local one) or to weaken the missing-credential skip for everybody.
///
/// It is declared, never inferred. A keyless entry is one that says
/// `"credential": "none"`, not one that forgot to say anything — the same
/// reasoning that makes `base_url` mandatory on this adapter rather than
/// defaulted (stage 1). Forgetting a field must never be the way a safety
/// property gets turned off.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CredentialRequirement {
    #[default]
    Required,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum ProviderAdapter {
    #[serde(rename = "anthropic")]
    Anthropic,
    /// First-party OpenAI on ZeroRouter's own Responses wire client
    /// (`crate::wire`) — billing-grade usage extraction on the wire where
    /// gpt-5.x and codex actually live. The pinned adapters cannot serve
    /// this family: the chat wire rejects it and the pinned Responses
    /// provider discards usage.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Any upstream speaking OpenAI **chat completions** — the dialect
    /// llama.cpp, vLLM, Ollama, and LM Studio all implement, and the one a
    /// hosted ZeroRouter `/v1` serves. Unlike the other two adapters this one
    /// has no implied endpoint, so a provider entry selecting it is expected
    /// to carry a `base_url` (edge mode, stage 1:
    /// `docs/design/edge-mode-local-rung.md`).
    #[serde(rename = "chat_completions")]
    ChatCompletions,
}

/// The operator's inventory, parsed and validated once at startup by
/// [`load_operator_inventory`].
///
/// Process-global and read-only after installation, which is the honest shape
/// for this data: half the inventory is `include_str!`-embedded, so the
/// provider list is already a restart-scoped artifact — unlike
/// `config/tiers.toml`, which is re-read per request because prices change
/// under a running service. It also keeps [`ProviderInventory::load`] free of
/// file I/O, and that function runs once per candidate per request.
static OPERATOR_INVENTORY: OnceLock<Vec<ProviderMetadata>> = OnceLock::new();

fn operator_inventory() -> &'static [ProviderMetadata] {
    OPERATOR_INVENTORY.get().map_or(&[], Vec::as_slice)
}

/// Read, validate, and install the operator's provider inventory.
///
/// Called once, from `serve`, before the router accepts anything. Every
/// failure mode is a startup failure rather than a degraded run: a deployment
/// whose local upstream is misconfigured must not come up serving cloud
/// traffic as though the operator had not asked for a local rung at all, and
/// must certainly not come up with a half-applied inventory.
///
/// The merged inventory is validated *before* the overlay is installed, so a
/// bad file cannot poison the loads that follow.
pub fn load_operator_inventory(path: &Path) -> Result<usize, ProviderBuildError> {
    let source = std::fs::read_to_string(path).map_err(|source| {
        ProviderBuildError::OperatorInventoryUnreadable {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let entries = ProviderInventory::parse_operator(&source)?;
    ProviderInventory::assembled(&entries)?;
    let count = entries.len();
    let keys: Vec<String> = entries.iter().map(|entry| entry.key.clone()).collect();
    OPERATOR_INVENTORY
        .set(entries)
        .map_err(|_| ProviderBuildError::OperatorInventoryAlreadyLoaded)?;
    tracing::info!(
        path = %path.display(),
        providers = ?keys,
        "operator provider inventory loaded"
    );
    Ok(count)
}

impl ProviderInventory {
    fn load() -> Result<Self, ProviderBuildError> {
        Self::assembled(operator_inventory())
    }

    fn shipped() -> Result<Self, ProviderBuildError> {
        serde_json::from_str::<Self>(PROVIDER_INVENTORY_JSON).map_err(|source| {
            ProviderBuildError::InvalidInventory {
                detail: source.to_string(),
            }
        })
    }

    /// Parse an operator-supplied inventory document.
    fn parse_operator(source: &str) -> Result<Vec<ProviderMetadata>, ProviderBuildError> {
        let inventory = serde_json::from_str::<Self>(source).map_err(|source| {
            ProviderBuildError::InvalidInventory {
                detail: format!("operator provider inventory is not valid: {source}"),
            }
        })?;
        Ok(inventory.providers)
    }

    /// The shipped inventory with the operator's entries layered on top.
    ///
    /// Additive, and only additive. An operator entry whose key matches a
    /// shipped provider is refused rather than allowed to win: the shipped
    /// entries name the endpoints ZeroRouter's own credentials are sent to, and
    /// a config file that could silently repoint `openai` at another host is a
    /// credential-exfiltration primitive, not an extension point. Renaming the
    /// operator's entry costs one line and leaves both providers addressable.
    ///
    /// The same rule covers the credential the entry reads. Blocking the key
    /// while leaving `credential_env` free would stop an operator entry from
    /// *being* `anthropic` but not from *reading* `ANTHROPIC_API_KEY` and
    /// posting it to an address of their choosing — the identical outcome by a
    /// different door. Both halves of "where ZeroRouter's own credential goes"
    /// are therefore reserved.
    fn assembled(operator: &[ProviderMetadata]) -> Result<Self, ProviderBuildError> {
        let mut inventory = Self::shipped()?;
        if !operator.is_empty() {
            let shipped_keys: BTreeSet<String> = inventory
                .providers
                .iter()
                .map(|provider| provider.key.clone())
                .collect();
            let shipped_credentials: BTreeSet<String> = inventory
                .providers
                .iter()
                .filter_map(|provider| provider.credential_env.clone())
                .collect();
            for entry in operator {
                if shipped_keys.contains(&entry.key) {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "operator provider {} shadows a shipped provider of the same key; \
                             the shipped entry names where ZeroRouter's own credential is sent, \
                             so it may be added to but never redefined — rename the operator entry",
                            entry.key
                        ),
                    });
                }
                if let Some(credential_env) = entry
                    .credential_env
                    .as_deref()
                    .filter(|name| shipped_credentials.contains(*name))
                {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "operator provider {} reads {credential_env}, which is a shipped \
                             provider's credential; an operator entry may not borrow a credential \
                             ZeroRouter holds for an upstream of its own — give this provider its \
                             own environment variable",
                            entry.key
                        ),
                    });
                }
                inventory.providers.push(entry.clone());
            }
        }
        inventory.validate()?;
        Ok(inventory)
    }

    fn validate(&self) -> Result<(), ProviderBuildError> {
        if self.providers.is_empty() {
            return Err(ProviderBuildError::InvalidInventory {
                detail: "provider list is empty".to_owned(),
            });
        }

        let mut keys = BTreeSet::new();
        for provider in &self.providers {
            if provider.key.trim().is_empty()
                || provider
                    .display_name
                    .as_deref()
                    .is_some_and(|name| name.trim().is_empty())
            {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: "provider metadata contains an empty required value".to_owned(),
                });
            }
            provider.validate_credential()?;
            provider.validate_settlement()?;
            provider.validate_region()?;
            provider.validate_reconciliation()?;
            if !keys.insert(provider.key.as_str()) {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!("duplicate provider key {}", provider.key),
                });
            }
            // Neither adapter in the SHIPPED inventory takes a base_url
            // override: both of those wires own their endpoints, and the only
            // override is the documented test seam. The per-adapter validation
            // this replaced covered aggregator and Bedrock shapes that no
            // longer exist.
            if provider
                .base_url
                .as_deref()
                .is_some_and(|url| url.trim().is_empty())
            {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!("provider {} has an empty base_url", provider.key),
                });
            }
            // The chat-completions adapter is the exception, and it is a
            // CREDENTIAL-SAFETY rule rather than a tidiness one. That wire
            // defaults to `https://api.openai.com/v1/chat/completions` when no
            // endpoint is given, because that is the right default for the
            // hosted dialect owner. An operator adding a local entry —
            // llama.cpp on a LAN address, say — who forgets `base_url` would
            // otherwise get a silently valid inventory that ships their
            // credential and their prompts to OpenAI, which is the exact
            // opposite of what running a model locally is for. There is no
            // sensible default for an adapter with no implied endpoint, so the
            // entry must state one.
            if provider.adapter == ProviderAdapter::ChatCompletions && provider.base_url.is_none() {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {} uses the chat_completions adapter and must declare a \
                         base_url; it has no implied endpoint, and defaulting one would send \
                         this provider's credential to a different host than configured",
                        provider.key
                    ),
                });
            }
        }
        Ok(())
    }

    fn provider(&self, key: &str) -> Option<&ProviderMetadata> {
        self.providers.iter().find(|provider| provider.key == key)
    }
}

impl ProviderMetadata {
    /// Enforce the credential declaration, which is where the keyless escape
    /// hatch is kept from becoming a way to run a PAID upstream unauthenticated.
    ///
    /// Two rules, and the first is the load-bearing one:
    ///
    /// - `"credential": "none"` is legal **only** on the chat-completions
    ///   adapter. That is the adapter with no implied endpoint — an entry using
    ///   it must state its own `base_url`, so it is the operator's own upstream
    ///   by construction. The other two adapters point at api.anthropic.com and
    ///   api.openai.com, and there is no configuration in which sending those
    ///   hosts no credential is what somebody meant; allowing it would turn a
    ///   typo into a provider that silently contributes 401s to every walk it
    ///   is on, and would make "this candidate has no key" stop being a
    ///   reliable reason to skip a paid rung.
    /// - A keyless entry must not also name a credential env var or a secret.
    ///   Contradiction is refused rather than resolved: an entry saying both
    ///   things has one of them wrong, and picking a winner would mean guessing
    ///   which.
    ///
    /// A `required` entry is checked exactly as it was before this field
    /// existed — both names present and non-blank — so the shipped inventory is
    /// unaffected.
    /// Enforce the settlement declaration.
    ///
    /// One rule: `free` is legal only on the chat-completions adapter. That
    /// adapter is not *sufficient* for free settlement — the whole point of
    /// [`SettlementDeclaration`] is that it cannot be — but it is necessary.
    /// The other two adapters exist to talk to api.anthropic.com and
    /// api.openai.com, whose traffic ZeroRouter is invoiced for, so an entry
    /// claiming that traffic settles free is stating something known to be
    /// untrue and is refused before it can reach a rate table.
    fn validate_settlement(&self) -> Result<(), ProviderBuildError> {
        if self.settlement == SettlementDeclaration::Free
            && self.adapter != ProviderAdapter::ChatCompletions
        {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares \"settlement\": \"free\" on an adapter that dials a \
                     cloud endpoint ZeroRouter is invoiced for; only the chat_completions adapter \
                     — which must name its own base_url — may settle free",
                    self.key
                ),
            });
        }
        Ok(())
    }

    /// Everything this upstream needs before a request can be sent to it, or
    /// the name of the one environment variable that is missing.
    ///
    /// **This is the single definition of "can this deployment dispatch to this
    /// provider", and it exists because the answer is now read from two
    /// places.** [`build_with_credentials`] reads it to decide whether a rung
    /// joins a route; [`provider_is_dispatchable`] reads it to decide whether a
    /// lane appears in `/v1/models`. Those two answers must be the same answer.
    ///
    /// They were not, and that is the incident this function was extracted for.
    /// The catalog was credential-blind by design, so a deployment missing
    /// `BEDROCK_API_KEY` advertised both Bedrock lanes on the storefront —
    /// ZeroRouter's flagship zero-retention lanes — while every call to them
    /// was refused. Two separate implementations of "is this provider usable"
    /// would let that reopen the first time one grew a condition the other
    /// lacked, which is exactly how it happened: the region check was added to
    /// dispatch and the listing never had a check at all.
    ///
    /// A test-seam base-URL override is honored here rather than in the caller,
    /// so a harness that stands a fake upstream in front of a provider makes
    /// that provider dispatchable AND listable, in one place.
    fn dispatchable<F>(&self, mut read_env: F) -> Dispatchable
    where
        F: FnMut(&str) -> Option<String>,
    {
        // The endpoint before the credential, because an unresolvable region
        // disqualifies for the same reason and by the same mechanism a missing
        // key does. A test-seam override is a complete URL supplied by a
        // harness, so it wins and never interpolates.
        let endpoint = match base_url_override(&self.key) {
            Some(url) => Some(Some(url)),
            None => self.endpoint(&mut read_env),
        };
        let Some(endpoint) = endpoint else {
            // Named in the same list a missing credential reports. Both answer
            // the operator's one question — which environment variable is this
            // deployment missing — and an endpoint this provider cannot address
            // is as disqualifying as a key it cannot present.
            return Dispatchable::Missing {
                env: self.region_env.clone().unwrap_or_default(),
            };
        };
        let credential = match self.credential {
            // A keyless upstream has nothing to look up, so it can never be the
            // rung a route loses to a missing key — which is the whole point: a
            // local server that takes no credential must not need a fake one to
            // stay in the walk. Inventory validation has already refused this
            // declaration on every adapter that owns a cloud endpoint, so
            // nothing reachable from here can be a paid upstream dispatched
            // without authentication.
            CredentialRequirement::None => String::new(),
            CredentialRequirement::Required => {
                // Validated present; `unwrap_or_default` reads an empty name
                // rather than panicking, and an empty name resolves to no
                // credential, so the rung drops out exactly as it would for a
                // genuinely absent key.
                let credential_env = self.credential_env.as_deref().unwrap_or_default();
                let Some(credential) = read_env(credential_env) else {
                    return Dispatchable::Missing {
                        env: credential_env.to_owned(),
                    };
                };
                credential
            }
        };
        Dispatchable::Ready {
            credential,
            endpoint,
        }
    }

    /// Enforce that an exemption from price reconciliation states its case.
    ///
    /// `unreconcilable_reason` turns off the alarm that guards margin for every
    /// candidate on this upstream. A present-but-blank one would do that while
    /// asserting nothing, and would be indistinguishable in the report from a
    /// gap somebody actually thought about — so an empty string is refused
    /// exactly as a blank retention-pin field is. Writing the sentence is the
    /// cost of the exemption, and it is the whole cost.
    fn validate_reconciliation(&self) -> Result<(), ProviderBuildError> {
        if self
            .unreconcilable_reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty())
        {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares an empty unreconcilable_reason; that field exempts every \
                     candidate on this upstream from price reconciliation, so it must say why the \
                     public catalog cannot price it and where the real rates come from",
                    self.key
                ),
            });
        }
        Ok(())
    }

    /// Enforce the region declaration: a `base_url` interpolates iff the entry
    /// names the variable that fills it.
    ///
    /// Refused in BOTH directions, and neither is tidiness:
    ///
    /// - A `{region}` with no `region_env` has nothing to resolve it. The
    ///   substitution would not happen, and the wire would POST this provider's
    ///   credential and the customer's prompt to a host literally named
    ///   `bedrock-mantle.{region}.api.aws` — a DNS failure on a good day, and a
    ///   domain somebody else can register on a bad one.
    /// - A `region_env` with no `{region}` names a variable nothing reads. The
    ///   entry then claims to be region-configurable while dialling a fixed
    ///   host, so an operator who sets the variable and redeploys gets traffic
    ///   in the old region and no indication of it.
    ///
    /// The same present-iff-required shape as
    /// [`ProviderMetadata::validate_credential`], for the same reason: a
    /// half-written declaration is refused rather than half-applied.
    fn validate_region(&self) -> Result<(), ProviderBuildError> {
        let interpolates = self
            .base_url
            .as_deref()
            .is_some_and(|url| url.contains(REGION_PLACEHOLDER));
        let declared = self
            .region_env
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty());
        match (interpolates, declared) {
            (true, false) => Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} writes {REGION_PLACEHOLDER} in its base_url and declares no \
                     region_env to fill it; the placeholder would survive into the request URL \
                     and send this provider's credential to a host named {REGION_PLACEHOLDER}",
                    self.key
                ),
            }),
            (false, true) => Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares region_env but its base_url has no {REGION_PLACEHOLDER} \
                     to substitute; the entry would read as region-configurable while dialling a \
                     fixed host, so setting that variable would change nothing",
                    self.key
                ),
            }),
            _ => Ok(()),
        }
    }

    /// This entry's endpoint with its region substituted, or `None` when the
    /// region cannot be resolved.
    ///
    /// `None` is NOT an error here, and that is the whole design: an
    /// unresolvable region makes this provider unavailable exactly as a missing
    /// credential does (see [`build_with_credentials`]), so a deployment that
    /// forgot `BEDROCK_REGION` loses its Bedrock rungs and keeps serving
    /// everything else. Guessing a default region would be the alternative, and
    /// it is the wrong one: `us-east-1` is a plausible guess that silently
    /// routes an eu-west-1 deployment's prompts to Virginia.
    fn endpoint<F>(&self, mut read_env: F) -> Option<Option<String>>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let Some(base_url) = self.base_url.as_deref() else {
            return Some(None);
        };
        if !base_url.contains(REGION_PLACEHOLDER) {
            return Some(Some(base_url.to_owned()));
        }
        // Validated present whenever the placeholder is, so an empty name here
        // is unreachable; reading it as "no region" keeps this total rather
        // than panicking.
        let region_env = self.region_env.as_deref().unwrap_or_default();
        let region = read_env(region_env)?;
        Some(Some(base_url.replace(REGION_PLACEHOLDER, &region)))
    }

    fn validate_credential(&self) -> Result<(), ProviderBuildError> {
        let declared = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        };
        match self.credential {
            CredentialRequirement::Required => {
                if !declared(&self.credential_env) || !declared(&self.secret_name) {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "provider {} must declare credential_env and secret_name, or declare \
                             \"credential\": \"none\" if its upstream takes no credential",
                            self.key
                        ),
                    });
                }
            }
            CredentialRequirement::None => {
                if self.adapter != ProviderAdapter::ChatCompletions {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "provider {} declares \"credential\": \"none\" on an adapter that \
                             owns a cloud endpoint; only the chat_completions adapter — which \
                             has no implied endpoint and must name its own base_url — may be \
                             keyless",
                            self.key
                        ),
                    });
                }
                if self.credential_env.is_some() || self.secret_name.is_some() {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "provider {} declares \"credential\": \"none\" and still names a \
                             credential_env or secret_name; an entry cannot both need and not \
                             need a credential",
                            self.key
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Returns whether `provider` has a constructor in this module.
///
/// Configuration validation should use this function instead of maintaining a
/// second provider-name list.
#[must_use]
pub fn is_supported_provider(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| inventory.provider(provider).is_some())
}

/// Whether this provider declares that its traffic bills nobody (edge mode,
/// stage 2) — half of what makes a candidate free, the other half being a $0
/// price in `tiers.toml`.
///
/// Read in three places, all of which must agree on one answer: catalog
/// validation (a $0 candidate on a metered provider refuses the file),
/// [`crate::config::TierCandidate::is_free`] (the selection-time key, and
/// stage 3's metering-skip key), and the drift reconciliation (a free upstream
/// is one no public catalog covers). Answering from the inventory rather than
/// from a list of vendor names kept elsewhere is what keeps those three from
/// drifting apart.
///
/// See [`SettlementDeclaration`] for why neither the adapter nor the presence
/// of a credential can stand in for this declaration, and for the honest limit
/// of what it guarantees.
#[must_use]
pub fn provider_settles_free(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory
            .provider(provider)
            .is_some_and(|metadata| metadata.settlement == SettlementDeclaration::Free)
    })
}

/// This upstream's declared reason for being outside the reconciliation
/// source's coverage, if it declares one.
///
/// `None` — the overwhelmingly common case — means the provider is reconciled
/// exactly as it always was. `Some(reason)` means `admin catalog-drift` has no
/// trustworthy row to compare against and says so in the report, quoting this
/// text so the gap is argued in front of the operator on every run rather than
/// hidden in a config file.
///
/// Read from the inventory rather than from a table inside `drift.rs`, for the
/// reason [`provider_settles_free`] gives: the inventory is where per-upstream
/// facts live, and a second list elsewhere is a second list to drift.
#[must_use]
pub fn provider_unreconcilable_reason(provider: &str) -> Option<String> {
    ProviderInventory::load().ok().and_then(|inventory| {
        inventory
            .provider(provider)
            .and_then(|metadata| metadata.unreconcilable_reason.clone())
    })
}

/// The environment variables one provider reads: its credential, and its region
/// when it has a regional endpoint.
///
/// Exposed so a tool that must talk to an upstream OUTSIDE the request path —
/// `admin retention-drift --bedrock-live`, which calls Bedrock's control API
/// rather than its inference API — reads the same variable names the router
/// itself dispatches with, instead of restating them somewhere else. Two lists
/// of environment variable names is two lists to drift, and the failure would be
/// silent in the worst way: a live retention check reading a variable nobody
/// sets reports "cannot verify" forever while the router serves happily.
#[must_use]
pub fn provider_env_names(provider: &str) -> Option<(String, Option<String>)> {
    let inventory = ProviderInventory::load().ok()?;
    let metadata = inventory.provider(provider)?;
    Some((
        metadata.credential_env.clone()?,
        metadata.region_env.clone(),
    ))
}

#[derive(Debug, Error)]
pub enum ProviderBuildError {
    #[error("embedded provider inventory is invalid: {detail}")]
    InvalidInventory { detail: String },
    #[error("failed to read the operator provider inventory at {path}")]
    OperatorInventoryUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the operator provider inventory has already been loaded")]
    OperatorInventoryAlreadyLoaded,
    #[error("provider route has no candidates")]
    EmptyRoute,
    #[error("candidate {candidate} names unsupported provider {provider}")]
    UnsupportedProvider { candidate: String, provider: String },
    #[error(
        "no upstream credentials are available; checked environment variables {credential_envs:?}"
    )]
    NoAvailableCredentials { credential_envs: Vec<String> },
}

/// An available upstream candidate and its canonical tier-table definition.
///
/// The provider client is deliberately private so all attributed calls pass
/// through [`ProviderDispatch`]. Both walks try these entries in order
/// themselves; nothing in this module decides when to retry or when to move on.
pub struct ProviderCandidate {
    definition: TierCandidate,
    provider: Arc<dyn ModelProvider>,
}

#[cfg(test)]
impl ProviderCandidate {
    /// Aim a candidate at a scripted local upstream so the router-owned
    /// streaming walk can be driven end to end without a network provider.
    pub(crate) fn against_local_upstream(definition: TierCandidate, base_url: &str) -> Self {
        // A stand-in upstream for tests that only need "some provider":
        // the owned Responses wire, aimed at a local scripted server.
        Self::with_provider(
            definition,
            Arc::new(crate::wire::OpenAiResponsesWire::new(
                "test-upstream",
                "test-credential",
                Some(base_url),
                None,
                1,
            )),
        )
    }
}

#[cfg(feature = "testing")]
impl ProviderCandidate {
    /// Put a pre-built provider client behind a canonical tier candidate.
    ///
    /// The only way anything other than a configured upstream can serve a
    /// candidate, so it is gated on the `testing` feature and cannot exist in
    /// a production binary.
    #[must_use]
    pub fn with_provider(definition: TierCandidate, provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            definition,
            provider,
        }
    }
}

impl ProviderCandidate {
    #[must_use]
    pub fn definition(&self) -> &TierCandidate {
        &self.definition
    }

    #[must_use]
    pub fn supports_streaming(&self) -> bool {
        self.provider.supports_streaming()
    }

    /// Run a non-streaming call directly against this candidate's configured
    /// model, without retrying another candidate.
    pub async fn chat(
        &self,
        request: ChatRequest<'_>,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        self.provider
            .chat(request, &self.definition.model, temperature)
            .await
    }

    /// Start a stream directly against this candidate's configured model.
    #[must_use]
    pub fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>> {
        self.provider
            .stream_chat(request, &self.definition.model, temperature, options)
    }
}

impl fmt::Debug for ProviderCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCandidate")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// Provider clients for one resolved route.
///
/// `candidates` preserves the tier-table order after candidates whose ECS
/// credential is unavailable have been removed. That order is the whole of the
/// route's policy: which candidate is tried, how often, and when to give up are
/// the walk's decisions, in `api.rs`.
pub struct ProviderRoute {
    candidates: Vec<ProviderCandidate>,
}

impl ProviderRoute {
    /// Build a per-request provider route from canonical tier candidates.
    ///
    /// Missing credentials make an individual candidate unavailable. Building
    /// fails only when no configured candidate can be constructed. Do not cache
    /// this value across requests: fallback selection metadata is request-scoped.
    pub fn new(
        candidates: Vec<TierCandidate>,
        max_output_tokens: u32,
    ) -> Result<Self, ProviderBuildError> {
        let inventory = ProviderInventory::load()?;
        build_with_credentials(&inventory, candidates, max_output_tokens, read_credential)
    }

    /// Assemble a route from pre-built candidates.
    ///
    /// Goes through the same [`assemble_route`] wiring as [`Self::new`], so a
    /// test drives the production route and only the leaf provider clients
    /// differ. Gated on the `testing` feature for the same reason
    /// [`ProviderCandidate::with_provider`] is.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn from_candidates(candidates: Vec<ProviderCandidate>) -> Self {
        assemble_route(candidates)
    }

    #[must_use]
    pub fn candidates(&self) -> &[ProviderCandidate] {
        &self.candidates
    }

    /// The selection-policy seam (design doc: Engine "Selection policy"):
    /// `api::order_candidates` permutes the route here, AFTER construction,
    /// so a reorder moves each candidate's definition and its transport
    /// together and can never pair one rung's definition with another's
    /// client — and an injected test route is reordered exactly as a
    /// production one.
    pub fn candidates_mut(&mut self) -> &mut Vec<ProviderCandidate> {
        &mut self.candidates
    }

    #[must_use]
    pub fn into_candidates(self) -> Vec<ProviderCandidate> {
        self.candidates
    }
}

impl fmt::Debug for ProviderRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRoute")
            .field("candidates", &self.candidates)
            .finish_non_exhaustive()
    }
}

/// Put each candidate behind a client, dropping the ones whose provider has no
/// credential in the environment.
///
/// The inventory is a parameter for the same reason `credential_for` is: both
/// are the deployment's answers to questions this function does not ask, and
/// passing them in lets a test drive the production code path over a
/// configuration it constructed rather than over whatever the process happens
/// to have installed globally.
fn build_with_credentials<F>(
    inventory: &ProviderInventory,
    candidates: Vec<TierCandidate>,
    max_output_tokens: u32,
    mut credential_for: F,
) -> Result<ProviderRoute, ProviderBuildError>
where
    F: FnMut(&str) -> Option<String>,
{
    if candidates.is_empty() {
        return Err(ProviderBuildError::EmptyRoute);
    }

    let mut available = Vec::with_capacity(candidates.len());
    let mut missing_credentials = Vec::new();
    let mut providers = BTreeMap::<&str, Arc<dyn ModelProvider>>::new();
    let mut unavailable = BTreeSet::new();

    for definition in candidates {
        let metadata = inventory.provider(&definition.provider).ok_or_else(|| {
            ProviderBuildError::UnsupportedProvider {
                candidate: definition.id.clone(),
                provider: definition.provider.clone(),
            }
        })?;
        let provider_key = metadata.key.as_str();
        if unavailable.contains(provider_key) {
            continue;
        }

        let provider = if let Some(provider) = providers.get(provider_key) {
            Arc::clone(provider)
        } else {
            let (credential, endpoint) = match metadata.dispatchable(&mut credential_for) {
                Dispatchable::Ready {
                    credential,
                    endpoint,
                } => (credential, endpoint),
                Dispatchable::Missing { env } => {
                    missing_credentials.push(env);
                    unavailable.insert(provider_key);
                    continue;
                }
            };
            let provider = create_provider(
                metadata,
                &credential,
                max_output_tokens,
                endpoint.as_deref(),
            )?;
            providers.insert(provider_key, Arc::clone(&provider));
            provider
        };

        available.push(ProviderCandidate {
            definition,
            provider,
        });
    }

    if available.is_empty() {
        return Err(ProviderBuildError::NoAvailableCredentials {
            credential_envs: missing_credentials,
        });
    }

    Ok(assemble_route(available))
}

/// Put ordered candidates behind a route.
///
/// Kept as its own function, rather than inlined into the two constructors, so
/// a test-supplied route and a credential-built one go through the same wiring
/// and cannot diverge.
fn assemble_route(candidates: Vec<ProviderCandidate>) -> ProviderRoute {
    ProviderRoute { candidates }
}

fn read_credential(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Test seam, `STRIPE_API_BASE`-shaped: `ZEROROUTER_PROVIDER_BASE_URL_<KEY>`
/// (key uppercased) overrides a provider's endpoint so a failure-injection
/// harness can stand a misbehaving upstream in front of a REAL router
/// process — half-open sockets, mid-stream drops, 429 storms are not
/// reachable from the in-process fakes. Leave unset in any real deployment;
/// an override is logged loudly at startup so it can never hide in one.
fn base_url_override(key: &str) -> Option<String> {
    let variable = format!(
        "ZEROROUTER_PROVIDER_BASE_URL_{}",
        key.to_ascii_uppercase().replace('-', "_")
    );
    let value = env::var(&variable).ok()?.trim().to_owned();
    if value.is_empty() {
        return None;
    }
    tracing::warn!(provider = key, %variable, "provider base URL overridden — test seam active");
    Some(value)
}

/// Put one upstream behind its wire.
///
/// `effective_base_url` is resolved by the caller rather than read from
/// `metadata` here, because resolving it can FAIL — a regional endpoint whose
/// region variable is unset has no address — and the caller is the one that can
/// answer that failure correctly, by dropping the rung instead of the route.
fn create_provider(
    metadata: &ProviderMetadata,
    credential: &str,
    max_output_tokens: u32,
    effective_base_url: Option<&str>,
) -> Result<Arc<dyn ModelProvider>, ProviderBuildError> {
    let alias = metadata.key.as_str();
    let provider: Arc<dyn ModelProvider> = match metadata.adapter {
        ProviderAdapter::Anthropic => Arc::new(AnthropicWire::new(
            alias,
            credential,
            effective_base_url,
            max_output_tokens,
            // Honor ZR's 15-minute upstream budget (api.rs), not an adapter
            // default.
            900,
        )),
        ProviderAdapter::OpenAiResponses => Arc::new(OpenAiResponsesWire::new(
            alias,
            credential,
            effective_base_url,
            Some(max_output_tokens),
            // Same budget note as the Anthropic arm: honor ZR's 15-minute
            // upstream budget, not an adapter default.
            900,
        )),
        ProviderAdapter::ChatCompletions => Arc::new(ChatCompletionsWire::new(
            alias,
            credential,
            effective_base_url,
            Some(max_output_tokens),
            // Same budget note as the arms above.
            900,
        )),
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use crate::config::ModelMetadata;
    use crate::provider::ModelRates;

    use super::*;

    fn candidate(id: &str, provider: &str) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: provider.to_owned(),
            model: format!("upstream/{id}"),
            rates: crate::provider::RateSchedule::flat(ModelRates {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: None,
            }),
            // Provider dispatch reads `provider` and `model` and nothing else.
            metadata: ModelMetadata::default(),
        }
    }

    #[test]
    fn supported_provider_check_uses_constructor_table() {
        for provider in ["anthropic", "openai", "google", "bedrock"] {
            assert!(is_supported_provider(provider));
        }
        // Retired with the git dependency that supplied their adapters, and
        // still gone. `bedrock` used to sit in this list; it came BACK on
        // 2026-08-20 as the zero-retention lane, on ZeroRouter's own Messages
        // wire rather than the pinned Converse adapter that once served it.
        for provider in ["deepinfra", "fireworks", "together", "minimax"] {
            assert!(
                !is_supported_provider(provider),
                "{provider} is no longer in the inventory"
            );
        }
        assert!(!is_supported_provider("unknown"));
    }

    #[test]
    fn the_shipped_inventory_dispatches_exactly_where_it_says_it_does() {
        // Every upstream a default deployment can reach, and the wire it speaks
        // — pinned so that adding an adapter, or a provider that uses one, is
        // always a deliberate edit to this list rather than a side effect.
        //
        // Stage 1 of edge mode added the chat-completions adapter with no
        // shipped route using it, and this assertion held that separation until
        // Google arrived on 2026-08-18: Gemini is served over Google's
        // OpenAI-compatible endpoint, so it is the first shipped provider on
        // that wire. The adapter being dual-use (local rungs AND a metered
        // cloud vendor) is the design's own scope, not a widening of it — see
        // `SettlementDeclaration`, which is why "on the local wire" still
        // cannot make anything free.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let adapters: Vec<(&str, ProviderAdapter)> = inventory
            .providers
            .iter()
            .map(|provider| (provider.key.as_str(), provider.adapter))
            .collect();
        //
        // Bedrock joined on 2026-08-20 on the ANTHROPIC adapter, which is the
        // surprising half and the reason it is pinned here. Bedrock's mantle
        // plane does serve an OpenAI-compatible `/v1/chat/completions`, and the
        // obvious move was to put it on the chat-completions wire beside
        // Google. AWS's model cards say otherwise: Claude marks Chat
        // Completions and Responses unsupported and Messages supported, so the
        // lane rides the Messages wire — which already sends the `x-api-key`
        // and `anthropic-version` headers that endpoint takes.
        assert_eq!(
            adapters,
            [
                ("anthropic", ProviderAdapter::Anthropic),
                ("openai", ProviderAdapter::OpenAiResponses),
                ("google", ProviderAdapter::ChatCompletions),
                ("bedrock", ProviderAdapter::Anthropic),
            ]
        );

        // Bedrock is a SECOND entry on the Anthropic adapter, and the two must
        // stay distinguishable in every way that matters: its own key, its own
        // credential, its own endpoint. Sharing a wire is not sharing an
        // account, and the whole point of the lane is that the account differs.
        let bedrock = inventory.provider("bedrock").expect("bedrock is shipped");
        let anthropic = inventory
            .provider("anthropic")
            .expect("anthropic is shipped");
        assert_ne!(bedrock.credential_env, anthropic.credential_env);
        assert_eq!(bedrock.credential_env.as_deref(), Some("BEDROCK_API_KEY"));
        assert_eq!(bedrock.secret_name.as_deref(), Some("bedrock-api-key"));
        assert_eq!(bedrock.settlement, SettlementDeclaration::Metered);
        assert_eq!(bedrock.credential, CredentialRequirement::Required);
        assert!(
            anthropic.base_url.is_none(),
            "the first-party Anthropic entry still owns its endpoint; only the \
             Bedrock one overrides, and it must not drag Anthropic's traffic with it"
        );

        // The endpoint is REGIONAL and says so. Both halves are asserted
        // because either one alone is a live foot-gun: a placeholder with no
        // variable dials a host named `{region}`, and a hardcoded region sends
        // an eu-west-1 deployment's prompts to Virginia.
        let base_url = bedrock
            .base_url
            .as_deref()
            .expect("bedrock declares its endpoint");
        assert!(base_url.contains(REGION_PLACEHOLDER), "{base_url}");
        assert_eq!(bedrock.region_env.as_deref(), Some("BEDROCK_REGION"));
        assert!(
            base_url.ends_with("/anthropic/v1/messages"),
            "the Messages wire posts to the URL verbatim, so the entry must carry \
             the full mantle path — NOT the /v1/chat/completions surface, which \
             AWS documents Claude as not supporting: {base_url}"
        );

        // Google bills like any other vendor: the free lane is entered only by
        // an explicit `settlement: free`, never by the wire a provider speaks.
        let google = inventory.provider("google").expect("google is shipped");
        assert_eq!(google.settlement, SettlementDeclaration::Metered);
        assert_eq!(google.credential, CredentialRequirement::Required);
        assert!(
            google
                .base_url
                .as_deref()
                .is_some_and(|url| url.ends_with("/chat/completions")),
            "the chat-completions wire posts to the URL verbatim, so the entry \
             must carry the full endpoint path, not an OpenAI-style /v1 base"
        );
    }

    #[test]
    fn a_provider_entry_can_select_the_chat_completions_adapter() {
        // The config contract: the tag a provider entry writes to reach the
        // new wire, and the base_url that entry needs — this adapter is the
        // one with no implied endpoint.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [{
                "key": "local-llama",
                "adapter": "chat_completions",
                "credential_env": "LOCAL_LLAMA_API_KEY",
                "secret_name": "local-llama-api-key",
                "display_name": "llama.cpp (local)",
                "base_url": "http://127.0.0.1:8080/v1/chat/completions"
            }]}"#,
        )
        .expect("a chat_completions entry parses");
        inventory.validate().expect("and validates");
        let metadata = inventory
            .provider("local-llama")
            .expect("the entry is addressable by key");
        assert_eq!(metadata.adapter, ProviderAdapter::ChatCompletions);

        // And it builds a client, through the same constructor a production
        // route uses.
        let provider = create_provider(
            metadata,
            "secret",
            crate::provider::BASELINE_MAX_TOKENS,
            metadata.base_url.as_deref(),
        )
        .expect("the chat-completions arm builds");
        assert_eq!(provider.alias(), "local-llama");
        assert!(provider.supports_streaming());
    }

    #[test]
    fn a_chat_completions_entry_without_a_base_url_is_refused() {
        // The config foot-gun this rule exists for: the wire's default
        // endpoint is OpenAI's, so an operator's LOCAL entry that forgets
        // base_url would validate cleanly and then ship that provider's
        // credential and prompts to api.openai.com. An adapter with no
        // implied endpoint has to be told its endpoint.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [{
                "key": "local-llama",
                "adapter": "chat_completions",
                "credential_env": "LOCAL_LLAMA_API_KEY",
                "secret_name": "local-llama-api-key"
            }]}"#,
        )
        .expect("the entry parses");
        let error = inventory
            .validate()
            .expect_err("a chat_completions entry with no base_url is invalid");
        let detail = error.to_string();
        assert!(detail.contains("local-llama"), "{detail}");
        assert!(detail.contains("base_url"), "{detail}");

        // The other two adapters are unaffected: their wires own their
        // endpoints, so omitting base_url is the normal case for them.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [
                {"key": "anthropic", "adapter": "anthropic",
                 "credential_env": "A", "secret_name": "a"},
                {"key": "openai", "adapter": "openai_responses",
                 "credential_env": "O", "secret_name": "o"}
            ]}"#,
        )
        .expect("the entries parse");
        inventory
            .validate()
            .expect("the endpoint-owning adapters still need no base_url");
    }

    #[test]
    fn an_empty_base_url_is_still_refused_for_the_new_adapter() {
        // base_url is meaningful configuration for this adapter rather than a
        // test seam, which makes the existing empty-value rule load-bearing:
        // an entry that declares one must declare a real one.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [{
                "key": "local-llama",
                "adapter": "chat_completions",
                "credential_env": "LOCAL_LLAMA_API_KEY",
                "secret_name": "local-llama-api-key",
                "base_url": "   "
            }]}"#,
        )
        .expect("the entry parses");
        assert!(matches!(
            inventory.validate(),
            Err(ProviderBuildError::InvalidInventory { .. })
        ));
    }

    #[test]
    fn an_unknown_adapter_tag_is_a_loud_inventory_error() {
        // The adapter tag is a closed set; a typo must fail the whole
        // inventory rather than silently selecting a default wire.
        assert!(
            serde_json::from_str::<ProviderInventory>(
                r#"{"providers": [{
                    "key": "x", "adapter": "chat_completion",
                    "credential_env": "E", "secret_name": "s"
                }]}"#,
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // Edge mode, stage 2: the operator's own inventory, layered over the
    // shipped one, and the keyless declaration a local server needs
    // (`docs/design/edge-mode-local-rung.md`).
    // -----------------------------------------------------------------------

    /// A one-entry operator inventory document.
    fn operator_json(body: &str) -> String {
        format!(r#"{{"providers": [{body}]}}"#)
    }

    /// The canonical local entry: keyless, free-settling, chat-completions,
    /// own endpoint.
    const LOCAL_ENTRY: &str = r#"{
        "key": "local-llama",
        "adapter": "chat_completions",
        "credential": "none",
        "settlement": "free",
        "display_name": "llama.cpp (local)",
        "base_url": "http://127.0.0.1:8080/v1/chat/completions"
    }"#;

    fn assemble(body: &str) -> Result<ProviderInventory, ProviderBuildError> {
        let entries = ProviderInventory::parse_operator(&operator_json(body))
            .expect("the operator document should parse");
        ProviderInventory::assembled(&entries)
    }

    #[test]
    fn an_operator_entry_is_added_to_the_shipped_inventory_not_merged_into_it() {
        // The layering contract in one assertion: the shipped entries are
        // untouched and the operator's is addressable beside them. Both facts
        // matter — a deployment that gains a local rung must not lose, or
        // silently alter, the cloud rungs it bursts to.
        let inventory = assemble(LOCAL_ENTRY).expect("a valid operator entry should assemble");
        let keys: Vec<&str> = inventory
            .providers
            .iter()
            .map(|provider| provider.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["anthropic", "openai", "google", "bedrock", "local-llama"]
        );

        let local = inventory
            .provider("local-llama")
            .expect("the operator entry is addressable by key");
        assert_eq!(local.adapter, ProviderAdapter::ChatCompletions);
        assert_eq!(local.credential, CredentialRequirement::None);
        assert_eq!(local.settlement, SettlementDeclaration::Free);
        for shipped in ["anthropic", "openai"] {
            let metadata = inventory.provider(shipped).expect("shipped entry survives");
            assert_eq!(metadata.credential, CredentialRequirement::Required);
            assert_eq!(
                metadata.settlement,
                SettlementDeclaration::Metered,
                "a shipped provider bills, and says so by saying nothing"
            );
        }
    }

    #[test]
    fn an_operator_entry_may_not_shadow_a_shipped_provider() {
        // The rule that keeps this an extension point rather than a
        // credential-exfiltration primitive: `openai` names the endpoint
        // ZeroRouter's own OpenAI key is sent to, and no file on the
        // operator's disk may repoint it.
        let error = assemble(
            r#"{
                "key": "openai",
                "adapter": "chat_completions",
                "credential": "none",
                "base_url": "http://127.0.0.1:8080/v1/chat/completions"
            }"#,
        )
        .expect_err("shadowing a shipped provider must be refused");
        let detail = error.to_string();
        assert!(detail.contains("openai"), "{detail}");
        assert!(detail.contains("shadows"), "{detail}");
    }

    #[test]
    fn free_settlement_is_refused_on_every_adapter_that_dials_a_billed_endpoint() {
        // Necessary but not sufficient. The chat-completions adapter is the
        // only one that CAN settle free, because it is the only one without an
        // endpoint ZeroRouter is invoiced for — but being on it proves nothing
        // (see the dual-use note on `SettlementDeclaration`), which is why the
        // declaration exists at all.
        for adapter in ["anthropic", "openai_responses"] {
            let error = assemble(&format!(
                r#"{{"key": "wishful", "adapter": "{adapter}", "settlement": "free",
                    "credential_env": "W", "secret_name": "w"}}"#
            ))
            .expect_err("free settlement must be refused on a billed adapter");
            let detail = error.to_string();
            assert!(detail.contains("wishful"), "{adapter}: {detail}");
            assert!(detail.contains("settlement"), "{adapter}: {detail}");
        }
    }

    #[test]
    fn a_free_settling_provider_may_be_keyless_or_credentialed() {
        // The correction the review forced. Credential presence was never the
        // right key: a local vLLM behind a bearer token is an ordinary
        // deployment, and it is free because its operator runs it — not because
        // it happens to be unauthenticated. Both shapes assemble.
        let keyless = assemble(LOCAL_ENTRY).expect("a keyless free provider assembles");
        assert_eq!(
            keyless
                .provider("local-llama")
                .expect("entry exists")
                .settlement,
            SettlementDeclaration::Free
        );

        let credentialed = assemble(
            r#"{
                "key": "secure-local",
                "adapter": "chat_completions",
                "credential_env": "SECURE_LOCAL_API_KEY",
                "secret_name": "secure-local-api-key",
                "settlement": "free",
                "base_url": "http://127.0.0.1:8000/v1/chat/completions"
            }"#,
        )
        .expect("a credentialed free provider assembles too");
        let entry = credentialed.provider("secure-local").expect("entry exists");
        assert_eq!(entry.settlement, SettlementDeclaration::Free);
        assert_eq!(entry.credential, CredentialRequirement::Required);
    }

    #[test]
    fn an_operator_entry_may_not_borrow_a_shipped_providers_credential() {
        // The other door into the same room the shadowing rule guards. Blocking
        // the KEY `anthropic` while leaving `ANTHROPIC_API_KEY` readable would
        // stop an operator entry from being Anthropic and not from reading
        // Anthropic's key and posting it to an address of its choosing.
        let error = assemble(
            r#"{
                "key": "definitely-local",
                "adapter": "chat_completions",
                "credential_env": "ANTHROPIC_API_KEY",
                "secret_name": "anthropic-api-key",
                "base_url": "http://198.51.100.7:8080/v1/chat/completions"
            }"#,
        )
        .expect_err("borrowing a shipped credential must be refused");
        let detail = error.to_string();
        assert!(detail.contains("definitely-local"), "{detail}");
        assert!(detail.contains("ANTHROPIC_API_KEY"), "{detail}");
    }

    #[test]
    fn a_misspelled_field_refuses_the_inventory_instead_of_defaulting() {
        // Every field this struct gained in stage 2 is a safety declaration
        // whose ABSENCE is meaningful, which is the shape where a silent
        // default is worst: `"credentail": "none"` would leave the entry
        // `required` and the typo invisible until a route quietly lost a rung.
        for typo in [
            r#""credentail": "none""#,
            r#""settlment": "free""#,
            r#""base_urls": "http://127.0.0.1:8080/v1/chat/completions""#,
        ] {
            let refused = ProviderInventory::parse_operator(&operator_json(&format!(
                r#"{{"key": "local-llama", "adapter": "chat_completions", {typo}}}"#
            )));
            assert!(
                refused.is_err(),
                "a misspelled field must refuse the document: {typo}"
            );
        }

        // And the fields that DO exist still parse — including the shipped
        // inventory, which names none of the stage-2 ones.
        ProviderInventory::shipped().expect("the shipped inventory still parses");
        assemble(LOCAL_ENTRY).expect("a fully-specified entry still parses");
    }

    #[test]
    fn a_keyless_declaration_is_refused_on_every_adapter_that_owns_a_cloud_endpoint() {
        // The abuse this stage must not open. "No credential needed" is a fact
        // about a server on the operator's own network; on an adapter that
        // dials api.anthropic.com or api.openai.com it is either a typo or an
        // attempt to run a paid upstream unauthenticated, and either way it
        // would erode "this candidate has no key" as a reason to skip a paid
        // rung.
        for adapter in ["anthropic", "openai_responses"] {
            let error = assemble(&format!(
                r#"{{"key": "sneaky", "adapter": "{adapter}", "credential": "none"}}"#
            ))
            .expect_err("keyless must be refused on a cloud adapter");
            let detail = error.to_string();
            assert!(detail.contains("sneaky"), "{adapter}: {detail}");
            assert!(detail.contains("chat_completions"), "{adapter}: {detail}");
        }
    }

    #[test]
    fn a_keyless_entry_that_still_names_a_credential_is_refused() {
        // Contradiction is refused rather than resolved: an entry claiming
        // both to need and not need a credential has one half wrong, and
        // picking a winner means guessing which.
        for extra in [
            r#""credential_env": "LOCAL_KEY""#,
            r#""secret_name": "local-key""#,
        ] {
            let error = assemble(&format!(
                r#"{{
                    "key": "local-llama",
                    "adapter": "chat_completions",
                    "credential": "none",
                    "base_url": "http://127.0.0.1:8080/v1/chat/completions",
                    {extra}
                }}"#
            ))
            .expect_err("a contradictory credential declaration must be refused");
            assert!(error.to_string().contains("cannot both need"), "{error}");
        }
    }

    #[test]
    fn a_keyless_entry_still_has_to_name_its_endpoint() {
        // The stage-1 rule is not weakened by the stage-2 one. Without
        // base_url this wire defaults to api.openai.com, so a keyless entry
        // that omitted it would send the operator's prompts to OpenAI with no
        // credential — the exact opposite of running a model locally, twice
        // over.
        let error = assemble(
            r#"{"key": "local-llama", "adapter": "chat_completions", "credential": "none"}"#,
        )
        .expect_err("a keyless entry with no base_url must be refused");
        assert!(error.to_string().contains("base_url"), "{error}");
    }

    #[test]
    fn a_credentialed_entry_must_still_name_both_its_credential_and_its_secret() {
        // The pre-existing rule, unchanged, now that both fields are optional
        // in the SCHEMA: an entry that says nothing about its credential is
        // still `required` and must still name where the key lives. Silence is
        // not the way to become keyless.
        for body in [
            r#"{"key": "half", "adapter": "anthropic", "credential_env": "A"}"#,
            r#"{"key": "half", "adapter": "anthropic", "secret_name": "a"}"#,
            r#"{"key": "half", "adapter": "anthropic"}"#,
            r#"{"key": "half", "adapter": "anthropic", "credential_env": "  ", "secret_name": "a"}"#,
        ] {
            let error =
                assemble(body).expect_err("an under-declared credentialed entry must be refused");
            assert!(error.to_string().contains("credential_env"), "{error}");
        }
    }

    #[test]
    fn a_keyless_provider_is_never_the_rung_a_route_loses_to_a_missing_key() {
        // The behavioural point of the whole declaration, over the production
        // route builder. With NO credentials in the environment at all, the
        // paid rung drops out — as it always has — and the local rung still
        // builds. The operator never had to invent a fake key to get that, and
        // the missing-credential skip is untouched for everyone else.
        let inventory = assemble(LOCAL_ENTRY).expect("the local entry should assemble");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("cloud", "openai"),
                candidate("local", "local-llama"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| None,
        )
        .expect("a keyless provider should still build a route");

        let ids: Vec<&str> = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect();
        assert_eq!(ids, ["local"]);
    }

    // -----------------------------------------------------------------------
    // Regional endpoints (2026-08-20): Bedrock's mantle host carries the region
    // in its name, so the shipped entry interpolates rather than hardcoding one.
    // -----------------------------------------------------------------------

    #[test]
    fn a_regional_base_url_must_name_the_variable_that_fills_it() {
        // The foot-gun: without a `region_env` the placeholder survives into
        // the request URL, so this provider's credential and the customer's
        // prompt go to a host literally named `bedrock-mantle.{region}.api.aws`
        // — a domain ZeroRouter does not own and somebody else could.
        let error = assemble(
            r#"{
                "key": "regional",
                "adapter": "chat_completions",
                "credential_env": "REGIONAL_API_KEY",
                "secret_name": "regional-api-key",
                "base_url": "https://svc.{region}.example.test/v1/chat/completions"
            }"#,
        )
        .expect_err("a placeholder with nothing to fill it must be refused");
        let detail = error.to_string();
        assert!(detail.contains("regional"), "{detail}");
        assert!(detail.contains("{region}"), "{detail}");
    }

    #[test]
    fn a_region_variable_with_no_placeholder_to_fill_is_refused_too() {
        // The other direction, and it is not tidiness. The entry reads as
        // region-configurable while dialling a fixed host, so an operator who
        // sets the variable and redeploys gets traffic in the old region and
        // nothing anywhere says so.
        let error = assemble(
            r#"{
                "key": "fixed",
                "adapter": "chat_completions",
                "credential_env": "FIXED_API_KEY",
                "secret_name": "fixed-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "region_env": "FIXED_REGION"
            }"#,
        )
        .expect_err("a region_env with no placeholder must be refused");
        assert!(error.to_string().contains("fixed"), "{error}");
    }

    #[test]
    fn a_regional_endpoint_resolves_its_region_from_the_declared_variable() {
        // The substitution itself, over the SHIPPED Bedrock entry rather than a
        // fixture, so the assertion is about the endpoint real traffic goes to.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock = inventory.provider("bedrock").expect("bedrock is shipped");

        let resolved = bedrock
            .endpoint(|name| (name == "BEDROCK_REGION").then(|| "eu-west-1".to_owned()))
            .expect("a resolvable region yields an endpoint")
            .expect("the entry declares a base_url");
        assert_eq!(
            resolved,
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1/messages"
        );
        assert!(
            !resolved.contains(REGION_PLACEHOLDER),
            "no placeholder may survive into a dialled URL: {resolved}"
        );

        // A non-regional entry is untouched by any of this.
        let google = inventory.provider("google").expect("google is shipped");
        assert_eq!(
            google.endpoint(|_| None).expect("no region to resolve"),
            google.base_url.clone()
        );
    }

    #[test]
    fn an_unresolvable_region_drops_the_rung_and_not_the_route() {
        // The failure mode, and it is chosen rather than inherited. A
        // deployment that sets BEDROCK_API_KEY but forgets BEDROCK_REGION has
        // no address for that upstream, and the alternative to dropping the
        // rung would be defaulting a region — where `us-east-1` is the
        // plausible guess that silently routes an eu-west-1 deployment's
        // prompts to Virginia. So it behaves exactly as a missing credential
        // does: this rung goes, the rest of the route serves.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("bedrock-rung", "bedrock"),
                candidate("anthropic-rung", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            // Every credential present; the REGION is what is missing.
            |name| (name != "BEDROCK_REGION").then(|| "secret".to_owned()),
        )
        .expect("the other rungs still build");
        let ids: Vec<&str> = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect();
        assert_eq!(ids, ["anthropic-rung"]);

        // And with the region present it is back, on its own client rather
        // than sharing the first-party Anthropic one — same adapter, different
        // account, and conflating them would send Bedrock traffic to
        // api.anthropic.com under the wrong key.
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("bedrock-rung", "bedrock"),
                candidate("anthropic-rung", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("a fully configured route builds");
        let ids: Vec<&str> = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect();
        assert_eq!(ids, ["bedrock-rung", "anthropic-rung"]);
        assert!(
            !Arc::ptr_eq(&route.candidates[0].provider, &route.candidates[1].provider),
            "two providers on one adapter must not share a client"
        );
    }

    // -----------------------------------------------------------------------
    // The price-reconciliation exemption.
    // -----------------------------------------------------------------------

    /// THE ANTI-DRIFT PROPERTY, and the reason
    /// [`ProviderMetadata::dispatchable`] was extracted.
    ///
    /// `/v1/models` and route construction must reach the same verdict from the
    /// same environment, for every provider, in every state. When they disagree
    /// the catalog either advertises a lane that cannot serve — the incident —
    /// or hides one that can.
    ///
    /// The Bedrock row is the one a second implementation gets wrong: a
    /// credential-only check passes it with `BEDROCK_API_KEY` set and
    /// `BEDROCK_REGION` unset, while dispatch drops it for want of an address.
    #[test]
    fn the_catalog_and_the_route_agree_on_every_environment() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        // (name, environment) pairs covering each way a provider can fail.
        let environments: Vec<(&str, Vec<&str>)> = vec![
            ("nothing set", vec![]),
            ("only anthropic", vec!["ANTHROPIC_API_KEY"]),
            ("bedrock key without its region", vec!["BEDROCK_API_KEY"]),
            ("bedrock region without its key", vec!["BEDROCK_REGION"]),
            (
                "bedrock fully configured",
                vec!["BEDROCK_API_KEY", "BEDROCK_REGION"],
            ),
            (
                "everything set",
                vec![
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "GEMINI_API_KEY",
                    "BEDROCK_API_KEY",
                    "BEDROCK_REGION",
                ],
            ),
        ];

        for (label, present) in environments {
            let read_env = |name: &str| present.contains(&name).then(|| "value".to_owned());
            for provider in ["anthropic", "openai", "google", "bedrock"] {
                let listed = provider_is_dispatchable_with(provider, read_env);
                // The route builder's verdict for the same provider and the
                // same environment: it either keeps the rung or drops it.
                let routed = build_with_credentials(
                    &inventory,
                    vec![candidate("only", provider)],
                    crate::provider::BASELINE_MAX_TOKENS,
                    read_env,
                )
                .is_ok();
                assert_eq!(
                    listed, routed,
                    "{label}: /v1/models says dispatchable={listed} for {provider} \
                     but route construction says {routed}"
                );
            }
        }
    }

    /// The specific pair that a credential-only check would get wrong, spelled
    /// out so the failure names the region rather than just "they disagree".
    #[test]
    fn a_key_without_its_region_is_not_dispatchable() {
        let with_key_only = |name: &str| (name == "BEDROCK_API_KEY").then(|| "value".to_owned());
        assert!(
            !provider_is_dispatchable_with("bedrock", with_key_only),
            "a regional provider with no region has no address, so it must not be listed"
        );

        let with_both = |name: &str| {
            matches!(name, "BEDROCK_API_KEY" | "BEDROCK_REGION").then(|| "value".to_owned())
        };
        assert!(provider_is_dispatchable_with("bedrock", with_both));

        // And a non-regional provider is unaffected by the region rule.
        let anthropic_only = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| "value".to_owned());
        assert!(provider_is_dispatchable_with("anthropic", anthropic_only));
    }

    #[test]
    fn an_exemption_from_price_reconciliation_must_state_its_case() {
        // This field turns off the alarm that guards margin for every candidate
        // on an upstream. A blank one would do that while asserting nothing,
        // and would read in the report exactly like a gap somebody argued.
        let error = assemble(
            r#"{
                "key": "silent",
                "adapter": "chat_completions",
                "credential_env": "SILENT_API_KEY",
                "secret_name": "silent-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "unreconcilable_reason": "   "
            }"#,
        )
        .expect_err("a blank exemption must be refused");
        let detail = error.to_string();
        assert!(detail.contains("silent"), "{detail}");
        assert!(detail.contains("unreconcilable_reason"), "{detail}");
    }

    #[test]
    fn only_the_declaring_provider_is_exempt_from_reconciliation() {
        // The narrowness that makes the exemption tolerable. Bedrock declares
        // one; nothing else does, and a lane that stopped being reconciled by
        // accident is the failure this asserts against.
        assert!(
            provider_unreconcilable_reason("bedrock")
                .is_some_and(|reason| reason.contains("_standard")),
            "bedrock's exemption must name the SKU class its rates really come from"
        );
        for provider in ["anthropic", "openai", "google"] {
            assert!(
                provider_unreconcilable_reason(provider).is_none(),
                "{provider} must still be reconciled against the public catalog"
            );
        }
    }

    #[test]
    fn missing_credentials_skip_candidates_without_reordering() {
        // A provider with no credential in the environment contributes no
        // candidates, and the survivors keep their catalog order — failover
        // must not silently reshuffle because a key happened to be absent.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let anthropic_env = inventory
            .provider("anthropic")
            .expect("anthropic metadata should exist")
            .credential_env
            .clone()
            .expect("a shipped provider declares its credential env var");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("one", "openai"),
                candidate("two", "anthropic"),
                candidate("three", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            // Only Anthropic is credentialed, so the OpenAI rung drops out.
            |name| (name == anthropic_env).then(|| "secret".to_owned()),
        )
        .expect("a credentialed provider should still build a route");

        let ids = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["two", "three"]);
    }

    #[test]
    fn candidates_on_same_upstream_share_one_client() {
        let route = build_with_credentials(
            &ProviderInventory::load().expect("the shipped inventory should load"),
            vec![candidate("one", "openai"), candidate("two", "openai")],
            8_192,
            |_| Some("secret".to_owned()),
        )
        .expect("providers should build");

        assert!(Arc::ptr_eq(
            &route.candidates[0].provider,
            &route.candidates[1].provider
        ));
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let credential = "credential-that-must-not-be-logged";
        let route = build_with_credentials(
            &ProviderInventory::load().expect("the shipped inventory should load"),
            vec![candidate("one", "openai")],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some(credential.to_owned()),
        )
        .expect("DeepInfra provider should build");

        assert!(!format!("{route:?}").contains(credential));
    }

    /// The upstream model a candidate is dispatched against is its own, taken
    /// from the tier table — never the tier id the customer asked for. The pin
    /// used to come from a wrapper around the chain; it now comes from
    /// `ProviderCandidate::chat`, and this asserts the value it reads.
    #[test]
    fn candidates_carry_their_pinned_upstream_model() {
        let route = build_with_credentials(
            &ProviderInventory::load().expect("the shipped inventory should load"),
            vec![candidate("one", "openai")],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("Fireworks provider should build");

        assert_eq!(route.candidates[0].definition.model, "upstream/one");
    }
}
