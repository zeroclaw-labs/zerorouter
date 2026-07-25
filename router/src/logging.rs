//! Log filtering, and the one boundary in it that an operator may not move.
//!
//! ZeroRouter's retention contract permits request *metadata* and never
//! provider request or response bodies. The pinned provider dependency does not
//! share that contract: `zeroclaw-log`'s event target and `zeroclaw-providers`'
//! own events carry sanitized upstream bodies, so those targets must not reach
//! ZeroRouter's log sink at all.
//!
//! # Why this is not an `EnvFilter` directive
//!
//! It used to be. `main` appended `zeroclaw_log_event=off,zeroclaw_providers=off`
//! to the operator's `RUST_LOG` and called that a guarantee. It was not one:
//! `EnvFilter` resolves overlapping directives by *specificity*, not by
//! position, so any operator directive more specific than a bare target — a
//! field-qualified one such as `zeroclaw_log_event[{body}]=trace` — outranks the
//! appended `off` and switches the bodies back on. A data-retention control that
//! an environment variable can revoke is not a control.
//!
//! # The shape that does hold
//!
//! [`subscriber`] layers two *global* filters onto the registry. The operator's
//! `EnvFilter` is one of them and the retention deny is the other, and
//! `tracing_subscriber`'s `Layered` composes global filters by conjunction: a
//! callsite is enabled only if **every** layer enables it, and a layer returning
//! `Interest::never` short-circuits the ones beneath it. The operator's filter
//! can therefore only ever *narrow* what is logged. There is no directive, no
//! syntax, and no specificity ranking that reaches back across a layer that has
//! already said no.
//!
//! # What that guarantee does and does not cover
//!
//! Airtight: **no `RUST_LOG` value can re-enable a protected target.** That is
//! the whole of the defect this replaced, and it is now a structural property of
//! the layer stack rather than a claim about directive precedence.
//!
//! Not covered, and deliberately stated rather than implied: the boundary is
//! defined over *targets*. An upstream event emitted under some third target
//! this list does not name would pass, exactly as it would have before — which
//! is why [`RETENTION_PROTECTED_TARGETS`] is the thing to extend when the pinned
//! dependency grows a new body-bearing target, and why the list matches whole
//! target path segments rather than bare string prefixes.

use tracing::Subscriber;
use tracing_subscriber::{
    EnvFilter, filter::filter_fn, fmt::MakeWriter, layer::SubscriberExt, registry,
    util::SubscriberInitExt,
};

/// Targets whose events may carry provider request or response bodies.
///
/// Matching is by whole path segment: an entry matches the target exactly, or as
/// a `::`-delimited ancestor of it (`zeroclaw_providers` covers
/// `zeroclaw_providers::openai`). A bare string prefix would be wrong in both
/// directions — it would sweep in an unrelated `zeroclaw_providers_metrics` and
/// still say nothing useful about intent.
pub const RETENTION_PROTECTED_TARGETS: &[&str] = &["zeroclaw_log_event", "zeroclaw_providers"];

/// Whether `target` sits inside the retention boundary and must never be logged.
#[must_use]
pub fn is_retention_protected(target: &str) -> bool {
    RETENTION_PROTECTED_TARGETS.iter().any(|protected| {
        target == *protected
            || target
                .strip_prefix(*protected)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// The router's subscriber: the operator's filter, the retention deny, and a
/// JSON sink, in that order of authority.
///
/// `requested_filter` is parsed leniently (an unusable directive is dropped
/// rather than failing startup), which is the long-standing behavior of the
/// `EnvFilter::new` call this replaced.
pub fn subscriber<W>(requested_filter: &str, writer: W) -> impl Subscriber + Send + Sync + use<W>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    registry()
        // What the operator asked for. It selects among what is permitted; it
        // does not decide what is permitted.
        .with(EnvFilter::new(requested_filter))
        // The retention boundary. A separate layer precisely so it is not
        // expressible in — and so cannot be outranked by — the filter above.
        .with(filter_fn(|metadata| {
            !is_retention_protected(metadata.target())
        }))
        .with(tracing_subscriber::fmt::layer().json().with_writer(writer))
}

/// Install [`subscriber`] as the process-wide default, writing JSON to stderr.
///
/// # Panics
///
/// If a global subscriber has already been installed.
pub fn init(requested_filter: &str) {
    subscriber(requested_filter, std::io::stderr).init();
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Arc, Mutex, PoisonError},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl CapturedLog {
        fn contents(&self) -> String {
            let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedLog {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn the_boundary_matches_whole_target_segments() {
        assert!(is_retention_protected("zeroclaw_log_event"));
        assert!(is_retention_protected("zeroclaw_providers"));
        assert!(is_retention_protected("zeroclaw_providers::openai"));
        assert!(is_retention_protected("zeroclaw_providers::openai::stream"));
        // A bare string prefix would sweep these in; a path-segment match must
        // not, or the boundary silently grows past what it documents.
        assert!(!is_retention_protected("zeroclaw_providers_metrics"));
        assert!(!is_retention_protected("zerorouter::api"));
        assert!(!is_retention_protected(""));
    }

    /// The defect itself: an operator filter crafted to outrank a target-only
    /// `off` directive. `zeroclaw_log_event[{body}]=trace` is field-qualified and
    /// therefore *more specific* than `zeroclaw_log_event=off`, so under the old
    /// "append `off` to `RUST_LOG`" scheme it won and the bodies were logged.
    ///
    /// It must now emit nothing — while the same filter's ordinary directives
    /// keep working, which the positive control below proves. Without that
    /// control an all-suppressing regression would pass this test.
    #[test]
    fn no_operator_filter_can_reopen_a_protected_target() {
        let captured = CapturedLog::default();
        let adversarial = "info,zeroclaw_log_event[{body}]=trace,\
                           zeroclaw_providers[{response_body}]=trace,zeroclaw_providers=trace";

        tracing::subscriber::with_default(subscriber(adversarial, captured.clone()), || {
            tracing::error!(target: "zeroclaw_log_event", body = "PROVIDER-BODY", "upstream call");
            tracing::error!(target: "zeroclaw_providers", response_body = "PROVIDER-BODY", "upstream call");
            tracing::error!(target: "zeroclaw_providers::openai", response_body = "PROVIDER-BODY", "upstream call");
            tracing::info!(target: "zerorouter::api", request_id = "req-1", "router metadata");
        });

        let logged = captured.contents();
        assert!(
            !logged.contains("PROVIDER-BODY"),
            "a protected target reached the sink: {logged}"
        );
        assert!(
            logged.contains("req-1"),
            "the operator's filter must still admit everything outside the boundary: {logged}"
        );
    }

    /// The boundary is a floor, not a ceiling: an operator who narrows past it
    /// still gets what they asked for.
    #[test]
    fn the_operator_filter_can_still_narrow() {
        let captured = CapturedLog::default();

        tracing::subscriber::with_default(subscriber("warn", captured.clone()), || {
            tracing::info!(target: "zerorouter::api", marker = "INFO-EVENT", "below the filter");
            tracing::warn!(target: "zerorouter::api", marker = "WARN-EVENT", "at the filter");
        });

        let logged = captured.contents();
        assert!(!logged.contains("INFO-EVENT"), "{logged}");
        assert!(logged.contains("WARN-EVENT"), "{logged}");
    }
}
