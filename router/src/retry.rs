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
//! | `parse_retry_after_ms` | `reliable.rs:350` | copied |
//! | `is_empty_completion` | `reliable.rs:685` | copied |
//! | `truncate_for_context` | `reliable.rs:634` | copied |
//! | `compute_backoff` | `reliable.rs:854` | copied |
//!
//! A copy is a fork the day the pin moves, so the copies are byte-faithful to
//! the cited lines and the tests below are a table over their observable
//! verdicts rather than a smoke test. **When the `zeroclaw-providers` pin
//! moves, diff those eight functions and re-run this module's tests before
//! anything else** — a silent classifier drift changes how many times a
//! customer's request is dispatched upstream, which is a COGS change nobody
//! would see in a diff.
//!
//! Nothing here is a policy decision. Every value and every branch reproduces
//! what the delegated walk already did; the walk that consumes them lives in
//! [`crate::api`].

use std::time::Duration;

use zeroclaw_providers::{
    reliable::{is_context_window_exceeded, is_non_retryable},
    traits::{ChatMessage, ChatResponse},
};

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
    ContextWindow,
}

impl FailureClass {
    /// Whether this failure ends the candidate immediately
    /// (`reliable.rs:1811`).
    #[must_use]
    pub fn is_non_retryable(self) -> bool {
        matches!(self, Self::NonRetryable | Self::RateLimitedNonRetryable)
    }

    /// Whether the upstream said 429, in either flavour. Gates the
    /// move-on-instead-of-waiting short circuit (`reliable.rs:1814`) and picks
    /// the ledger outcome.
    #[must_use]
    pub fn is_rate_limited(self) -> bool {
        matches!(self, Self::RateLimited | Self::RateLimitedNonRetryable)
    }

    /// The `request_attempts.outcome` for an attempt that failed this way.
    /// Every value is admitted by `request_attempts_outcome_is_known`
    /// (migration 0004) — an unknown string would abort the settle
    /// transaction, i.e. lose a settlement.
    #[must_use]
    pub fn outcome(self) -> &'static str {
        if self.is_rate_limited() {
            "rate_limited"
        } else {
            "upstream_error"
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
/// A [`FailureClass::ContextWindow`] returned after the prompt has ALREADY been
/// truncated once degrades to [`FailureClass::Retryable`], which is what the
/// delegated walk did by falling through to its general classifier — see
/// `context_window_errors_are_retryable_once_the_prompt_is_already_truncated`
/// for the proof that the general classifier reads it that way.
#[must_use]
pub fn classify(err: &anyhow::Error) -> FailureClass {
    if is_context_window_exceeded(err) {
        return FailureClass::ContextWindow;
    }
    let rate_limited = is_rate_limited(err);
    let non_retryable = is_non_retryable(err) || is_non_retryable_rate_limit(err);
    match (non_retryable, rate_limited) {
        (true, true) => FailureClass::RateLimitedNonRetryable,
        (true, false) => FailureClass::NonRetryable,
        (false, true) => FailureClass::RateLimited,
        (false, false) => FailureClass::Retryable,
    }
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
/// verbatim from `reliable.rs:350-380`.
#[must_use]
pub fn parse_retry_after_ms(err: &anyhow::Error) -> Option<u64> {
    let msg = err.to_string();
    let lower = msg.to_lowercase();

    for prefix in &[
        "retry-after:",
        "retry_after:",
        "retry-after ",
        "retry_after ",
    ] {
        if let Some(pos) = lower.find(prefix) {
            let after = &msg[pos + prefix.len()..];
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
    zeroclaw_providers::sanitize_api_error(&format!("{err:#}"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use zeroclaw_providers::traits::ToolCall;

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
                FailureClass::ContextWindow,
            ),
            ("prompt is too long", FailureClass::ContextWindow),
        ];

        for (detail, expected) in table {
            assert_eq!(classify(&error(detail)), expected, "classifying {detail:?}");
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
        assert!(!FailureClass::ContextWindow.is_non_retryable());

        assert!(FailureClass::RateLimited.is_rate_limited());
        assert!(FailureClass::RateLimitedNonRetryable.is_rate_limited());
        assert!(!FailureClass::NonRetryable.is_rate_limited());

        // Only these two outcome strings can reach the walk ledger from a
        // failed attempt, and both are in migration 0004's CHECK.
        assert_eq!(FailureClass::Retryable.outcome(), "upstream_error");
        assert_eq!(FailureClass::NonRetryable.outcome(), "upstream_error");
        assert_eq!(FailureClass::ContextWindow.outcome(), "upstream_error");
        assert_eq!(FailureClass::RateLimited.outcome(), "rate_limited");
        assert_eq!(
            FailureClass::RateLimitedNonRetryable.outcome(),
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
