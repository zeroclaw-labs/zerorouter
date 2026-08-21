//! Failure classification, backoff, and prompt repair for the router-owned
//! candidate walk.
//!
//! # Why this module exists at all
//!
//! The non-streaming walk used to be delegated to ZeroClaw's
//! `ReliableModelProvider`, which decided — inside a pinned git dependency the
//! router cannot edit — when to retry, when to abandon a candidate, and how
//! long to wait. Unrolling the walk into the router means reproducing those
//! decisions here, and roughly half the surface that made them is PRIVATE to
//! `zeroclaw_providers::reliable`:
//!
//! | behavior | pinned source | disposition |
//! |---|---|---|
//! | `is_non_retryable` | `reliable.rs:174` | **imported** (`pub`) |
//! | `is_context_window_exceeded` | `reliable.rs:278` | **imported** (`pub`) |
//! | `is_rate_limited` | `reliable.rs:297` | copied |
//! | `is_non_retryable_rate_limit` | `reliable.rs:308` | copied |
//! | `parse_retry_after_ms` | `reliable.rs:350` | copied, **diverged** |
//! | `is_empty_completion` | `reliable.rs:685` | copied |
//! | `truncate_for_context` | `reliable.rs:634` | copied |
//! | `compute_backoff` | `reliable.rs:854` | copied |
//!
//! A copy is a fork the day the pin moves, so the copies are byte-faithful to
//! the cited lines except where the table says otherwise, and the tests below
//! are a table over their observable verdicts rather than a smoke test. Exactly
//! one row diverges today, and it is called out below so the divergence has to
//! be re-decided rather than silently re-imported. **When the `zeroclaw-providers` pin
//! moves, diff those eight functions and re-run this module's tests before
//! anything else** — a silent classifier drift changes how many times a
//! customer's request is dispatched upstream, which is a COGS change nobody
//! would see in a diff.
//!
//! # The one row that is not byte-faithful
//!
//! [`parse_retry_after_ms`] fixes a panic the pinned original still carries: it
//! finds a byte offset in a lowercased copy of the message and slices the
//! ORIGINAL with it, which is out of bounds or mid-codepoint whenever
//! lowercasing changed the byte length. The text is an upstream response body,
//! so the input is not ours to constrain, and the blast radius differs by host:
//! in ZeroClaw the panic fails one agent turn, here it unwinds a spawned walk
//! and strands a reservation with no ledger row. **Do not re-import this one
//! when the pin moves** unless the upstream has fixed it too — see the function
//! for the exact divergence and
//! `retry_after_survives_non_ascii_upstream_text` for the inputs that pin it.
//!
//! No BRANCH here is a policy decision: every value the walk dispatches on
//! reproduces what the delegated walk already did, and the walk that consumes
//! them lives in [`crate::api`]. [`FailureClass::outcome`] is the exception by
//! construction — the delegated walk kept no ledger, so its label has no pinned
//! counterpart to be faithful to and answers to migration 0004 instead.

use std::time::Duration;

use crate::provider::{ChatMessage, ChatResponse};

/// Ceiling on one backoff interval. `reliable.rs:1842` / `reliable.rs:972`.
const BACKOFF_CAP_MS: u64 = 10_000;

/// Ceiling on an upstream-supplied `Retry-After`, so a hostile or mistaken
/// header cannot park a request for the rest of its deadline.
/// `reliable.rs:856`.
const RETRY_AFTER_CAP_MS: u64 = 30_000;

/// What one upstream failure means for the walk.
///
/// The two bits that decide it are exactly the two locals `reliable.rs:1791-1793`
/// computes — `non_retryable` (which folds in a business 429) and
/// `rate_limited` — plus the context-window check that `reliable.rs:1735`
/// makes ahead of both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    /// Retrying could work: transport faults, 5xx, anything unclassified.
    Retryable,
    /// A live 429. Still retryable, but a walk with somewhere else to go goes
    /// there instead of waiting (`reliable.rs:1814`).
    RateLimited,
    /// A 429 whose text says the account cannot pay — an exhausted quota, an
    /// unfunded balance, a plan that excludes the model. Waiting cannot fix it.
    RateLimitedNonRetryable,
    /// A 4xx, an auth failure, or a missing model. Retrying cannot fix it.
    NonRetryable,
    /// The prompt does not fit. Uniquely recoverable in place, by shortening
    /// the prompt rather than by waiting or by moving on.
    ///
    /// `rate_limited` carries what the upstream ALSO said. A TPM rejection
    /// reading `429 Too Many Requests: token limit exceeded` matches both hint
    /// lists — `token limit exceeded` is a context-window hint
    /// (`reliable.rs:283-292`) and `429` + `limit` is a rate-limit one — and
    /// truncating is the right response to it either way. Only the ledger
    /// LABEL depends on the bit, which is why it rides here instead of
    /// changing the class: see [`Self::outcome`].
    ContextWindow { rate_limited: bool },
    /// The upstream answered without attesting the data guarantee this lane is
    /// sold under (`crate::wire::ResponseAttestation`).
    ///
    /// The only class here with no pinned counterpart, because the delegated
    /// walk had no such concept — see the note on [`classify`].
    RetentionAttestation,
}

impl FailureClass {
    /// Whether this failure ends the candidate immediately
    /// (`reliable.rs:1811`).
    #[must_use]
    pub fn is_non_retryable(self) -> bool {
        matches!(
            self,
            Self::NonRetryable | Self::RateLimitedNonRetryable | Self::RetentionAttestation
        )
    }

    /// Whether the walk should abandon this candidate rather than wait out a
    /// 429. Gates the move-on-instead-of-waiting short circuit
    /// (`reliable.rs:1814`).
    ///
    /// Deliberately FALSE for a rate-limited [`Self::ContextWindow`]. That
    /// class's response is the in-place repair, not moving on, and the pinned
    /// walk only ever reached its 429 short circuit on a SECOND occurrence
    /// (`reliable.rs:1735` gates the whole context branch on
    /// `!context_truncated`) — by which point [`classify`] no longer returns
    /// `ContextWindow` at all. Keeping this predicate about control flow alone
    /// means no reordering of the walk can turn a repairable prompt into an
    /// abandoned candidate. The ledger reads the 429 separately, below.
    #[must_use]
    pub fn is_rate_limited(self) -> bool {
        matches!(self, Self::RateLimited | Self::RateLimitedNonRetryable)
    }

    /// Whether a BYOK attempt that failed this way may be retried on
    /// ZeroRouter's own credential, for a customer who opted in (migration
    /// 0028).
    ///
    /// # Why exactly one class is excluded
    ///
    /// The opt-in says "use your key if MINE fails", and every class here is a
    /// failure of the customer's key at the upstream — a 401 from a revoked
    /// credential, a 5xx, a timeout, a live 429 — except one.
    ///
    /// [`Self::RateLimitedNonRetryable`] is the 429 whose body says the ACCOUNT
    /// cannot pay: an exhausted quota, an unfunded balance, a plan that
    /// excludes the model ([`is_non_retryable_rate_limit`]). Falling back there
    /// would take a customer whose own provider account has run dry and start
    /// serving them at the full catalog price out of their ZeroRouter balance —
    /// converting their vendor's spending limit into a ZeroRouter bill, without
    /// them asking, at exactly the moment their own controls were trying to
    /// stop them spending. That is the surprise-bill #103's no-fallback rule
    /// exists to prevent, and an opt-in about reliability is not consent to it.
    ///
    /// A customer who wants that behaviour can detach the key. The refusal is
    /// deliberately not configurable: it is the one case where the customer's
    /// own cost control is the thing being overridden.
    ///
    /// [`Self::RetentionAttestation`] cannot occur on a BYOK attempt at all —
    /// the house attestation is not asserted on one ([`crate::providers`]) — so
    /// its value here is unreachable rather than chosen. It is written as
    /// permitted because the class means "this upstream would not promise what
    /// ZeroRouter sells", which is precisely a reason to try the house lane
    /// where that promise IS ZeroRouter's to make and IS checked.
    #[must_use]
    pub fn permits_byok_fallback(self) -> bool {
        !matches!(self, Self::RateLimitedNonRetryable)
    }

    /// The `request_attempts.outcome` for an attempt that failed this way.
    /// Every value is admitted by `request_attempts_outcome_is_known`
    /// (migration 0004) — an unknown string would abort the settle
    /// transaction, i.e. lose a settlement.
    ///
    /// This reads the 429 bit rather than [`Self::is_rate_limited`], so a
    /// context-window rejection the upstream delivered AS a 429 is recorded as
    /// the 429 it was. Migration 0004 documents this column as what feeds the
    /// health cooldown; labelling a real rate limit `upstream_error` because
    /// the router happened to have a repair for it hides the rung's state from
    /// the thing that has to notice it.
    ///
    /// Matched exhaustively on purpose: a new class must decide its label here
    /// rather than inherit one from a wildcard.
    #[must_use]
    pub fn outcome(self) -> &'static str {
        match self {
            Self::RateLimited
            | Self::RateLimitedNonRetryable
            | Self::ContextWindow { rate_limited: true } => "rate_limited",
            Self::Retryable
            | Self::NonRetryable
            | Self::ContextWindow {
                rate_limited: false,
            }
            // `upstream_error` rather than a label of its own, and the choice
            // is constrained rather than free: migration 0004's
            // `request_attempts_outcome_is_known` admits nine strings, and a
            // tenth would abort the settle transaction — i.e. LOSE a
            // settlement — until a migration widened the CHECK. Of the nine
            // this is the honest one: the upstream failed to answer usably.
            // It also feeds the health cooldown, which is the behaviour worth
            // having here — an upstream whose ZDR toggle went off fails every
            // request, and cooling the rung is the correct response to a rung
            // that cannot serve.
            | Self::RetentionAttestation => "upstream_error",
        }
    }
}

/// Classify one upstream failure.
///
/// Order is load-bearing and reproduces `reliable.rs:1735 → 1791-1793`: the
/// context-window check comes first because it alone has an in-place repair,
/// and a business 429 is folded into "non-retryable" before the plain
/// rate-limit short circuit is consulted.
///
/// `context_truncated` says whether this walk has already spent its one
/// truncation, and it gates the context-window check exactly as
/// `reliable.rs:1735` gated the whole branch. On a second occurrence the pinned
/// walk fell through to the general classifier it computes at
/// `reliable.rs:1791-1793`, i.e. it classified the error by everything it is
/// APART from being a context-window error — so this does too.
///
/// That distinction is not cosmetic. A TPM rejection reading
/// `429 Too Many Requests: token limit exceeded` is simultaneously
/// context-window-shaped and rate-limit-shaped. Returning
/// [`FailureClass::ContextWindow`] for it a second time would drop it past the
/// abandon-on-non-retryable check and past the move-on-for-a-live-429 check
/// into another backoff and another billable upstream call, on a candidate the
/// upstream has already refused twice.
///
/// For the plain case the flag changes nothing, which is what makes it
/// faithful rather than a policy choice: `is_non_retryable` short-circuits to
/// false for any context-window error (`reliable.rs:176-179`), so a second
/// `maximum context length exceeded` still classifies
/// [`FailureClass::Retryable`] and still burns the candidate's whole retry
/// budget — see
/// `context_window_errors_are_retryable_once_the_prompt_is_already_truncated`.
#[must_use]
pub fn classify(err: &anyhow::Error, context_truncated: bool) -> FailureClass {
    // THE SECOND DEPARTURE FROM THE PINNED WALK, and like `FailureClass::outcome`
    // it is one by construction rather than by drift: the delegated walk had no
    // notion of a retention guarantee, so there is no pinned behaviour for this
    // branch to be faithful to. Everything below this line still reproduces
    // `reliable.rs:1735 → 1791-1793` exactly.
    //
    // It is FIRST, ahead of even the context-window check, and the ordering is
    // load-bearing in a way the others are not. Every arm below leads either to
    // a repair or to another dispatch, and both of those send the customer's
    // prompt to this upstream AGAIN. That is the one response to this failure
    // that is worse than useless: the guarantee did not hold a moment ago, so a
    // retry is not a second chance at an answer, it is a second copy of the
    // prompt delivered to an upstream that just said it may keep it. Reaching
    // this check first is what makes the walk's response "stop" rather than
    // "try harder".
    //
    // The failure text is the channel because the two dispatch paths deliver it
    // as different types and both are reduced to a string before arriving here
    // — see `crate::wire::RETENTION_ATTESTATION_MARKER`.
    if is_retention_attestation_failure(err) {
        return FailureClass::RetentionAttestation;
    }
    // Hoisted only so the context-window arm can carry it; it decides nothing
    // here. The context check is still the first thing that DECIDES among the
    // pinned classes, which is the ordering `reliable.rs:1735 → 1791-1793`
    // fixes.
    let rate_limited = is_rate_limited(err);
    if !context_truncated && is_context_window_exceeded(err) {
        return FailureClass::ContextWindow { rate_limited };
    }
    let non_retryable = is_non_retryable(err) || is_non_retryable_rate_limit(err);
    match (non_retryable, rate_limited) {
        (true, true) => FailureClass::RateLimitedNonRetryable,
        (true, false) => FailureClass::NonRetryable,
        (false, true) => FailureClass::RateLimited,
        (false, false) => FailureClass::Retryable,
    }
}

/// Whether this failure is an upstream declining to attest a retention
/// guarantee ([`crate::wire::ResponseAttestation`]).
///
/// NOT copied from the pinned walk — there is nothing to copy, this concept did
/// not exist there. It reads the failure TEXT for the same reason every
/// predicate in this module does: by the time the walk classifies anything, the
/// only thing left of an upstream failure is `err.to_string()`. The marker is a
/// single constant shared with the wire that produces it, so the two cannot
/// drift apart the way a duplicated literal would.
///
/// This must never match anything an ordinary upstream could put in an error
/// body. A false positive here refuses a servable request and, worse, hides a
/// real upstream fault behind a retention alarm; the marker is therefore a
/// specific eight-word phrase rather than a word like "retention" that a
/// provider's own error prose could plausibly contain.
#[must_use]
pub fn is_retention_attestation_failure(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains(crate::wire::RETENTION_ATTESTATION_MARKER)
}

/// Whether an error is a rate-limit (429). Copied verbatim from
/// `reliable.rs:297-306`.
#[must_use]
pub fn is_rate_limited(err: &anyhow::Error) -> bool {
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>()
        && let Some(status) = reqwest_err.status()
    {
        return status.as_u16() == 429;
    }
    let msg = err.to_string();
    msg.contains("429")
        && (msg.contains("Too Many") || msg.contains("rate") || msg.contains("limit"))
}

/// Whether a 429 is a business refusal rather than a transient one. Copied
/// verbatim from `reliable.rs:308-346`.
#[must_use]
pub fn is_non_retryable_rate_limit(err: &anyhow::Error) -> bool {
    if !is_rate_limited(err) {
        return false;
    }

    let msg = err.to_string();
    let lower = msg.to_lowercase();

    let business_hints = [
        "plan does not include",
        "doesn't include",
        "not include",
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "quota exhausted",
        "out of credits",
        "no available package",
        "package not active",
        "purchase package",
        "model not available for your plan",
    ];

    if business_hints.iter().any(|hint| lower.contains(hint)) {
        return true;
    }

    // Known provider business codes observed for 429 where retry is futile.
    for token in lower.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = token.parse::<u16>()
            && matches!(code, 1113 | 1311)
        {
            return true;
        }
    }

    false
}

/// Extract a `Retry-After` value in milliseconds from an error message. Copied
/// from `reliable.rs:350-380` with one deliberate divergence.
///
/// # The divergence
///
/// The pinned original searches `msg.to_lowercase()` for the prefix and then
/// slices the ORIGINAL `msg` at the offset it found. `to_lowercase` is not
/// length-preserving — U+0130 is two bytes and lowercases to three, U+212A is
/// three bytes and lowercases to one — so that offset is not an index into
/// `msg`, and the slice lands mid-codepoint or past the end. Both panic.
///
/// The error text here is the upstream's HTTP response body verbatim
/// (`compatible.rs:2682-2685`), and `sanitize_api_error` scrubs credential
/// prefixes without normalising case or stripping non-ASCII — so the input is
/// upstream-controlled and need not be ASCII. In ZeroClaw a panic here fails
/// one agent turn; here it unwinds a spawned walk past
/// `UsageSession::record`, which leaves a reservation counting against the
/// user's caps with no ledger row and no settlement intent to replay.
///
/// So this reads the payload out of `lower` rather than out of `msg`. The
/// payload is ASCII digits and `.`, which `to_lowercase` does not alter, so
/// every input the original parsed still parses to the same value — the
/// divergence is confined to the inputs that used to abort.
#[must_use]
pub fn parse_retry_after_ms(err: &anyhow::Error) -> Option<u64> {
    let lower = err.to_string().to_lowercase();

    for prefix in &[
        "retry-after:",
        "retry_after:",
        "retry-after ",
        "retry_after ",
    ] {
        if let Some(pos) = lower.find(prefix) {
            // Same buffer the offset came from. `pos + prefix.len()` is a char
            // boundary in `lower` because the prefix is ASCII and matched
            // there; it is nothing in particular in `msg`.
            let after = &lower[pos + prefix.len()..];
            let num_str: String = after
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(secs) = num_str.parse::<f64>()
                && secs.is_finite()
                && secs >= 0.0
            {
                let millis = Duration::from_secs_f64(secs).as_millis();
                if let Ok(value) = u64::try_from(millis) {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// How long to wait before the next attempt: the upstream's `Retry-After` when
/// it gave one, capped, and never SHORTER than the schedule would have waited
/// anyway. Copied verbatim from `reliable.rs:854-861`.
#[must_use]
pub fn compute_backoff(base_ms: u64, err: &anyhow::Error) -> u64 {
    if let Some(retry_after) = parse_retry_after_ms(err) {
        retry_after.min(RETRY_AFTER_CAP_MS).max(base_ms)
    } else {
        base_ms
    }
}

/// Double the schedule, capped. `reliable.rs:972` / `reliable.rs:1842`.
///
/// Deliberately jitter-free: the delegated walk imported no RNG, so adding
/// jitter here would change the observable retry spacing rather than preserve
/// it.
#[must_use]
pub fn next_backoff(base_ms: u64) -> u64 {
    base_ms.saturating_mul(2).min(BACKOFF_CAP_MS)
}

/// Whether a completion is a blank turn: no text, no tool calls, no reasoning.
/// Copied verbatim from `reliable.rs:685-692`.
///
/// All three have to be blank. A tool-call-only or reasoning-only completion is
/// a complete answer, and treating either as empty would re-roll a turn the
/// customer already paid for.
#[must_use]
pub fn is_empty_completion(response: &ChatResponse) -> bool {
    response.text_or_empty().trim().is_empty()
        && response.tool_calls.is_empty()
        && response
            .reasoning_content
            .as_deref()
            .is_none_or(|reasoning| reasoning.trim().is_empty())
}

/// Drop the oldest half of the non-system history, returning how many messages
/// went. Copied verbatim from `reliable.rs:634-658`.
///
/// Returns 0 — and changes nothing — when there is at most one non-system
/// message, which is the walk's signal that the prompt cannot be reduced any
/// further.
pub fn truncate_for_context(messages: &mut Vec<ChatMessage>) -> usize {
    let non_system: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role != "system")
        .map(|(index, _)| index)
        .collect();

    // Keep at least the last non-system message (the most recent user turn).
    if non_system.len() <= 1 {
        return 0;
    }

    let drop_count = non_system.len() / 2;
    let indices_to_remove: Vec<usize> = non_system[..drop_count].to_vec();

    // Remove in reverse order to preserve indices.
    for &index in indices_to_remove.iter().rev() {
        messages.remove(index);
    }

    drop_count
}

/// One line of upstream failure detail, whitespace-collapsed and stripped of
/// anything credential-shaped. Reproduces `reliable.rs::compact_error_detail`,
/// including its use of the alternate `{:#}` formatter so the anyhow context
/// chain survives — the classifier, by contrast, reads only `to_string()`.
///
/// The walk ledger has no free-text error column (migration 0004), so this is
/// where a failure's detail goes. It is the one piece of the delegated walk's
/// aggregate failure string that is not replaced by a `request_attempts` row.
#[must_use]
pub fn compact_error_detail(err: &anyhow::Error) -> String {
    sanitize_api_error(&format!("{err:#}"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Longest upstream error text ZeroRouter will carry into its own logs and
/// error bodies.
const MAX_API_ERROR_CHARS: usize = 500;

fn is_secret_char(c: char) -> bool {
    // Dots and colons included deliberately: JWT-shaped and namespaced keys
    // carry them, and stopping at one would redact only a token's first
    // segment and log the rest.
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

fn token_end(input: &str, from: usize) -> usize {
    let mut end = from;
    for (index, c) in input[from..].char_indices() {
        if is_secret_char(c) {
            end = from + index + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Redact credential-shaped tokens from upstream error text.
///
/// Ported from the pinned runtime rather than imported, because it stands
/// between an upstream's raw error body and ZeroRouter's logs — the
/// retention contract this router promises is its own to keep, not a
/// dependency's. An upstream that echoes a request back can echo a key
/// with it; these are the prefixes worth catching.
#[must_use]
pub fn scrub_secret_patterns(input: &str) -> String {
    const PREFIXES: [&str; 7] = [
        "sk-",
        "xoxb-",
        "xoxp-",
        "ghp_",
        "gho_",
        "ghu_",
        "github_pat_",
    ];
    let mut scrubbed = input.to_string();
    for prefix in PREFIXES {
        let mut search_from = 0;
        while let Some(relative) = scrubbed[search_from..].find(prefix) {
            let start = search_from + relative;
            let content_start = start + prefix.len();
            let end = token_end(&scrubbed, content_start);
            // A bare prefix is not a token; skip it without stopping the scan.
            if end == content_start {
                search_from = content_start;
                continue;
            }
            scrubbed.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }
    scrubbed
}

/// Scrub, then bound. Both halves matter: the first keeps a credential out
/// of the logs, the second keeps a hostile upstream from writing a novel
/// into them.
#[must_use]
pub fn sanitize_api_error(input: &str) -> String {
    let scrubbed = scrub_secret_patterns(input);
    if scrubbed.chars().count() <= MAX_API_ERROR_CHARS {
        return scrubbed;
    }
    let mut end = MAX_API_ERROR_CHARS;
    while end > 0 && !scrubbed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &scrubbed[..end])
}

// ---------------------------------------------------------------------------
// Classifiers, owned
// ---------------------------------------------------------------------------
//
// These were imported from the pinned agent runtime. They are pure string
// heuristics over an upstream's error text — no state, no I/O — and they
// decide whether a customer's request is retried, which makes them part of
// ZeroRouter's own behavior rather than a borrowed detail. Ported verbatim
// so no request changes class on the day the pin was cut; the fidelity is
// pinned by the tests that already exercised them through the import.

/// Whether an upstream error means "the prompt does not fit".
///
/// The one failure with an in-place repair: the walk drops the oldest half
/// of the history and retries the SAME candidate.
#[must_use]
pub fn is_context_window_exceeded(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    [
        "exceeds the context window",
        "exceeds the available context size",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
        "prompt exceeds max length",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}

/// Tool-schema rejections are recoverable by the upstream adapter's own
/// fallback, so they must not be classed as terminal.
fn is_tool_schema_error(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    [
        "tool call validation failed",
        "was not in request",
        "not found in tool list",
        "invalid_tool_call",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}

/// Whether an error ends this candidate immediately.
///
/// 4xx is terminal except 429 (transient rate limit) and 408 (timeout).
/// Context-window and tool-schema errors are explicitly NOT terminal —
/// both have repairs. When no HTTP status is available the status digits
/// are parsed out of the message, then auth and unknown-model phrasings
/// are matched, because several upstreams report both as prose.
#[must_use]
pub fn is_non_retryable(err: &anyhow::Error) -> bool {
    if is_context_window_exceeded(err) || is_tool_schema_error(err) {
        return false;
    }
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>()
        && let Some(status) = reqwest_err.status()
    {
        let code = status.as_u16();
        return status.is_client_error() && code != 429 && code != 408;
    }
    let msg = err.to_string();
    for word in msg.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = word.parse::<u16>()
            && (400..500).contains(&code)
        {
            return code != 429 && code != 408;
        }
    }
    let lower = msg.to_lowercase();
    let auth_hints = [
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key not set",
        "authentication failed",
        "auth failed",
        "unauthorized",
        "forbidden",
        "permission denied",
        "access denied",
        "invalid token",
    ];
    if auth_hints.iter().any(|hint| lower.contains(hint)) {
        return true;
    }
    lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("unknown")
            || lower.contains("unsupported")
            || lower.contains("does not exist")
            || lower.contains("invalid"))
}

#[cfg(test)]
mod tests {
    use crate::provider::ToolCall;

    use super::*;

    fn error(detail: &str) -> anyhow::Error {
        anyhow::anyhow!(detail.to_owned())
    }

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    fn response(text: Option<&str>, tool_calls: usize, reasoning: Option<&str>) -> ChatResponse {
        ChatResponse {
            text: text.map(str::to_owned),
            tool_calls: (0..tool_calls)
                .map(|index| ToolCall {
                    id: format!("call_{index}"),
                    name: "shell".to_owned(),
                    arguments: "{}".to_owned(),
                    extra_content: None,
                })
                .collect(),
            usage: None,
            reasoning_content: reasoning.map(str::to_owned),
            stop_reason: None,
        }
    }

    /// The round trip from the wire's failure text to the walk's decision.
    ///
    /// The two halves of this mechanism live in different modules and are
    /// joined only by a string, so this is the seam where it can silently come
    /// apart: reword the message in `wire/mod.rs` without touching the constant
    /// and every retention failure quietly reclassifies as a generic retryable
    /// upstream error — which would RETRY it, sending the prompt to the
    /// unattested upstream two more times before moving on.
    #[test]
    fn an_attestation_failure_is_classified_as_its_own_class_and_never_retried() {
        // Built the way the wire builds it, not hand-written, so a change to
        // the real message is what this test sees.
        let attestation = crate::wire::ResponseAttestation::new("x-zero-data-retention", "true")
            .expect("the shipped declaration must be constructible");
        let failure = attestation
            .verify(
                "xai",
                reqwest::StatusCode::OK,
                &reqwest::header::HeaderMap::new(),
            )
            .expect_err("an empty header map attests nothing");
        let err = error(&failure);

        assert_eq!(
            classify(&err, false),
            FailureClass::RetentionAttestation,
            "the wire's own failure text must classify as a retention failure"
        );
        // And identically once the walk has spent its truncation, because the
        // check sits ahead of the context-window branch that flag gates.
        assert_eq!(classify(&err, true), FailureClass::RetentionAttestation);

        let class = FailureClass::RetentionAttestation;
        assert!(
            class.is_non_retryable(),
            "retrying re-delivers the prompt to an upstream that just declined to \
             say it will not keep it"
        );
        assert!(!class.is_rate_limited());
        // Admitted by migration 0004's `request_attempts_outcome_is_known`. An
        // unlisted string here aborts the settle transaction, which loses a
        // settlement rather than mislabelling one.
        assert_eq!(class.outcome(), "upstream_error");
    }

    /// The predicate must not fire on ordinary upstream prose.
    ///
    /// A false positive is worse than a missed one in an unobvious way: it
    /// refuses a request that was perfectly servable AND files a real upstream
    /// fault under a retention alarm, so the operator debugs the wrong thing.
    #[test]
    fn ordinary_upstream_failures_are_not_read_as_retention_failures() {
        for text in [
            "429 Too Many Requests",
            "connection reset by peer",
            "400 Bad Request: data retention policy violation for this org",
            "500 internal error: zero data retention subsystem unavailable",
            "the model did not attest anything",
        ] {
            let err = error(text);
            assert!(
                !is_retention_attestation_failure(&err),
                "{text:?} is an upstream's own prose, not ZeroRouter's attestation failure"
            );
            assert_ne!(classify(&err, false), FailureClass::RetentionAttestation);
        }
    }

    /// The anti-drift harness. Every row is a verdict the delegated walk
    /// reached, and each one decides how many times a customer's request is
    /// dispatched upstream. A row that flips when the pin moves is a COGS
    /// change; make it fail here rather than in production.
    #[test]
    fn classifier_table() {
        let table = [
            // Transport faults carry no status digits at all.
            ("connection reset by peer", FailureClass::Retryable),
            ("upstream sent an incomplete body", FailureClass::Retryable),
            // A live 429 in either of the two shapes the check accepts.
            ("429 Too Many Requests", FailureClass::RateLimited),
            (
                "provider API error (429 rate limit reached)",
                FailureClass::RateLimited,
            ),
            // Business 429s: waiting cannot fund the account.
            (
                "429 Too Many Requests: insufficient balance",
                FailureClass::RateLimitedNonRetryable,
            ),
            (
                "429 rate limit: error 1113 quota exhausted",
                FailureClass::RateLimitedNonRetryable,
            ),
            // A 429 whose text carries none of the three trigger words is not
            // read as a rate limit at all — the check is a conjunction, and
            // this row exists so that stays deliberate rather than assumed.
            (
                "429 insufficient balance for this request",
                FailureClass::Retryable,
            ),
            // 4xx status tokens scanned out of the message text.
            ("400 fake upstream x", FailureClass::NonRetryable),
            ("401 Unauthorized", FailureClass::NonRetryable),
            ("404 model not found", FailureClass::NonRetryable),
            // The two 4xx codes that are explicitly still worth retrying.
            ("408 Request Timeout", FailureClass::Retryable),
            ("429 rate", FailureClass::RateLimited),
            // Keyword heuristics for transports that carry no status.
            ("invalid api key", FailureClass::NonRetryable),
            (
                "model unknown to this deployment",
                FailureClass::NonRetryable,
            ),
            // 5xx is not a client error.
            ("503 Service Unavailable", FailureClass::Retryable),
            // The one class with an in-place repair.
            (
                "maximum context length exceeded",
                FailureClass::ContextWindow {
                    rate_limited: false,
                },
            ),
            (
                "prompt is too long",
                FailureClass::ContextWindow {
                    rate_limited: false,
                },
            ),
            // Both at once: `token limit exceeded` is a context-window hint
            // and `429` + `limit` is a rate-limit one. The repair still wins
            // the first time — but the 429 rides along so the ledger can say
            // what the upstream actually said.
            (
                "429 Too Many Requests: token limit exceeded",
                FailureClass::ContextWindow { rate_limited: true },
            ),
        ];

        for (detail, expected) in table {
            assert_eq!(
                classify(&error(detail), false),
                expected,
                "classifying {detail:?}"
            );
        }
    }

    /// The second occurrence is classified by everything the error is APART
    /// from being a context-window error, because the walk has no second
    /// repair to offer it (`reliable.rs:1735`).
    ///
    /// The 429-shaped row is the one that costs money: read as
    /// `ContextWindow` it would fall past both short circuits into a backoff
    /// and a third dispatch, where the pinned walk broke to the next candidate
    /// after two. The plain rows prove the flag is not a licence to give up
    /// early — they still burn the full budget.
    #[test]
    fn classifier_degrades_once_the_prompt_is_already_truncated() {
        let table = [
            (
                "429 Too Many Requests: token limit exceeded",
                FailureClass::RateLimited,
            ),
            ("maximum context length exceeded", FailureClass::Retryable),
            ("prompt is too long", FailureClass::Retryable),
            // A business 429 that is also context-shaped is still unpayable.
            (
                "429 rate limit: insufficient quota, prompt is too long",
                FailureClass::RateLimitedNonRetryable,
            ),
        ];

        for (detail, expected) in table {
            assert_eq!(
                classify(&error(detail), true),
                expected,
                "classifying {detail:?} after truncation"
            );
        }

        // Nothing else moves: the flag gates the context check and only that.
        for detail in [
            "connection reset by peer",
            "401 Unauthorized",
            "429 Too Many Requests",
            "503 Service Unavailable",
        ] {
            assert_eq!(
                classify(&error(detail), true),
                classify(&error(detail), false),
                "{detail:?} must not depend on the truncation flag"
            );
        }
    }

    /// The two derived bits the walk actually branches on, asserted apart from
    /// the variant names so a refactor of the enum cannot quietly re-point a
    /// branch.
    #[test]
    fn classifier_bits_drive_the_walk_branches() {
        assert!(FailureClass::NonRetryable.is_non_retryable());
        assert!(FailureClass::RateLimitedNonRetryable.is_non_retryable());
        assert!(!FailureClass::RateLimited.is_non_retryable());
        assert!(!FailureClass::Retryable.is_non_retryable());
        assert!(
            !FailureClass::ContextWindow {
                rate_limited: false
            }
            .is_non_retryable()
        );

        assert!(FailureClass::RateLimited.is_rate_limited());
        assert!(FailureClass::RateLimitedNonRetryable.is_rate_limited());
        assert!(!FailureClass::NonRetryable.is_rate_limited());
        // The 429 bit on a repairable prompt is a LABEL, not a branch: a walk
        // that read it as one would abandon a candidate whose prompt it could
        // still have shortened. Both flavours must answer no.
        assert!(
            !FailureClass::ContextWindow { rate_limited: true }.is_rate_limited(),
            "a rate-limited context window must not divert the walk"
        );
        assert!(
            !FailureClass::ContextWindow {
                rate_limited: false
            }
            .is_rate_limited()
        );

        // Only these two outcome strings can reach the walk ledger from a
        // failed attempt, and both are in migration 0004's CHECK.
        assert_eq!(FailureClass::Retryable.outcome(), "upstream_error");
        assert_eq!(FailureClass::NonRetryable.outcome(), "upstream_error");
        assert_eq!(
            FailureClass::ContextWindow {
                rate_limited: false
            }
            .outcome(),
            "upstream_error"
        );
        assert_eq!(FailureClass::RateLimited.outcome(), "rate_limited");
        assert_eq!(
            FailureClass::RateLimitedNonRetryable.outcome(),
            "rate_limited"
        );
        // The ledger says what the upstream said, even where the walk's
        // response was the truncation rather than a cooldown.
        assert_eq!(
            FailureClass::ContextWindow { rate_limited: true }.outcome(),
            "rate_limited"
        );
    }

    /// A context-window error is deliberately NOT non-retryable
    /// (`reliable.rs:177-179`), which is what makes degrading
    /// [`FailureClass::ContextWindow`] to [`FailureClass::Retryable`] — once
    /// the prompt has already been truncated — faithful rather than a policy
    /// choice.
    #[test]
    fn context_window_errors_are_retryable_once_the_prompt_is_already_truncated() {
        for detail in ["maximum context length exceeded", "prompt is too long"] {
            let err = error(detail);
            assert!(is_context_window_exceeded(&err));
            assert!(
                !is_non_retryable(&err),
                "{detail:?} must not read as non-retryable"
            );
            assert!(
                !is_rate_limited(&err),
                "{detail:?} must not read as rate-limited"
            );
        }
    }

    /// 500ms, 1000ms, then capped. The schedule the delegated walk spent, and
    /// the reason three calls to one candidate cost at least 1.5 seconds.
    #[test]
    fn backoff_schedule_doubles_to_a_cap() {
        assert_eq!(next_backoff(500), 1_000);
        assert_eq!(next_backoff(1_000), 2_000);
        assert_eq!(next_backoff(9_000), 10_000);
        assert_eq!(next_backoff(10_000), 10_000);
        assert_eq!(next_backoff(u64::MAX), 10_000, "doubling must not overflow");
    }

    /// `Retry-After` can only ever LENGTHEN a wait, and only up to 30s.
    #[test]
    fn retry_after_raises_but_never_lowers() {
        assert_eq!(
            compute_backoff(500, &error("429 Too Many Requests; Retry-After: 45")),
            30_000,
            "a long Retry-After is capped, not honored"
        );
        assert_eq!(
            compute_backoff(5_000, &error("429 retry_after 1")),
            5_000,
            "a short Retry-After never shortens the schedule"
        );
        assert_eq!(
            compute_backoff(500, &error("429 Too Many Requests")),
            500,
            "no header means the schedule stands"
        );
        assert_eq!(compute_backoff(500, &error("retry-after: 2.5")), 2_500);
        assert_eq!(
            parse_retry_after_ms(&error("retry-after: 2.5")),
            Some(2_500)
        );
        assert_eq!(parse_retry_after_ms(&error("Retry-After 3")), Some(3_000));
        assert_eq!(parse_retry_after_ms(&error("no header here")), None);
        assert_eq!(
            parse_retry_after_ms(&error("retry-after: soon")),
            None,
            "an unparseable value is no value"
        );
    }

    /// The error text is the upstream's HTTP response body verbatim
    /// (`compatible.rs:2682-2685` bails with `response.text()`, and
    /// `sanitize_api_error` scrubs seven credential prefixes without touching
    /// case or non-ASCII), so it is attacker-influenced and need not be ASCII.
    ///
    /// `to_lowercase` is not length-preserving — U+0130 is two bytes and
    /// lowercases to three, U+212A is three bytes and lowercases to one — so an
    /// offset found in the lowercased copy is not an index into the original.
    /// Scanning one buffer and slicing the other panicked the walk, and the
    /// walk is a spawned task: the panic unwinds past
    /// `UsageSession::record`, leaving the reservation open against the user's
    /// caps until the TTL sweep deletes it, with no `usage_events` row, no
    /// `request_attempts` row, and no settlement intent for recovery to replay.
    #[test]
    fn retry_after_survives_non_ascii_upstream_text() {
        // 22 bytes; 26 once lowercased. The old code sliced `msg` at 25.
        assert_eq!(
            parse_retry_after_ms(&error("\u{130}\u{130}\u{130}\u{130} retry_after 1")),
            Some(1_000),
            "a multi-byte-expanding prefix must not move the payload"
        );
        // The other direction: U+212A shrinks, so the offset lands early and
        // used to split a codepoint or silently read the wrong bytes.
        assert_eq!(
            parse_retry_after_ms(&error("429 K\u{212A} model\u{e9}x retry-after: 5")),
            Some(5_000),
            "a shrinking prefix must not discard a valid Retry-After"
        );
        // The whole point of parsing it: a wait the walk actually takes.
        assert_eq!(
            compute_backoff(500, &error("\u{130}\u{130}\u{130}\u{130} retry_after 1")),
            1_000
        );
    }

    /// Emptiness is the conjunction of three blanks. Any one of them being
    /// present makes the turn a real answer that must not be re-rolled.
    #[test]
    fn is_empty_completion_needs_all_three_blank() {
        let table = [
            (None, 0, None, true),
            (Some("   "), 0, None, true),
            (Some(""), 0, Some("  "), true),
            (Some("hello"), 0, None, false),
            (None, 1, None, false),
            (None, 0, Some("thinking"), false),
            (Some("hello"), 1, None, false),
            (Some("hello"), 1, Some("thinking"), false),
        ];

        for (text, tool_calls, reasoning, expected) in table {
            assert_eq!(
                is_empty_completion(&response(text, tool_calls, reasoning)),
                expected,
                "text={text:?} tool_calls={tool_calls} reasoning={reasoning:?}"
            );
        }
    }

    /// Truncation drops the oldest half of the non-system history and keeps the
    /// system prompt wherever it sits.
    #[test]
    fn truncate_for_context_drops_the_oldest_half_of_non_system() {
        let mut messages = vec![
            message("system", "rules"),
            message("user", "one"),
            message("assistant", "two"),
            message("user", "three"),
        ];
        assert_eq!(truncate_for_context(&mut messages), 1);
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(roles, ["system", "assistant", "user"]);
        assert_eq!(contents, ["rules", "two", "three"]);
    }

    /// Nothing left to drop is reported as 0 dropped, with the prompt
    /// untouched — the walk's signal that this prompt is irreducible.
    #[test]
    fn truncate_for_context_reports_an_irreducible_prompt() {
        let mut messages = vec![message("system", "rules"), message("user", "one")];
        assert_eq!(truncate_for_context(&mut messages), 0);
        assert_eq!(messages.len(), 2);

        let mut only_system = vec![message("system", "rules")];
        assert_eq!(truncate_for_context(&mut only_system), 0);
        assert_eq!(only_system.len(), 1);

        let mut empty: Vec<ChatMessage> = Vec::new();
        assert_eq!(truncate_for_context(&mut empty), 0);
    }

    /// A four-turn history loses two, not one: the drop count is half the
    /// non-system messages, so a long conversation sheds proportionally.
    #[test]
    fn truncate_for_context_scales_with_history_length() {
        let mut messages = vec![
            message("user", "one"),
            message("assistant", "two"),
            message("user", "three"),
            message("assistant", "four"),
            message("user", "five"),
        ];
        assert_eq!(truncate_for_context(&mut messages), 2);
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["three", "four", "five"]);
    }
}
