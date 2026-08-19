//! Minimal hand-rolled Stripe integration: Checkout Session creation,
//! webhook signature verification, and prepaid-credit application.
//!
//! The `async-stripe` SDK is deliberately not used — ZeroRouter needs a
//! handful of interactions (create a Checkout Session, read one back for
//! display, verify and apply a `checkout.session.completed` webhook), each
//! small enough to implement against the documented wire formats. Only the
//! first and third can move money; see [`checkout_status`] for why the second
//! cannot. The webhook path is fail-closed: a
//! missing or invalid signature, a stale timestamp, or unknown/malformed
//! metadata rejects the event and credits nothing. Replays are idempotent —
//! `crate::billing::credit_purchase` anchors each purchase to the unique
//! Stripe session id, so a redelivered event is acknowledged without a second
//! credit.
//!
//! # What the signature does and does not prove
//!
//! A valid HMAC proves the event came from Stripe. It proves nothing about
//! who created the session it describes: `metadata` is chosen by whoever
//! created the Checkout Session, and any party able to create a paid session
//! in this Stripe account — a second integration, an operational mistake, a
//! leaked restricted API key — can attach arbitrary `credit_usd` and
//! `user_id` to a session Stripe will then sign legitimately. Crediting the
//! metadata alone lets $1 collected mint $1000 of inference credit.
//!
//! Two independent preconditions therefore gate every credit, both applied
//! before [`billing::credit_purchase`] is reached:
//!
//! 1. **The event must corroborate itself.** The session's own `amount_total`
//!    and `currency` — what Stripe actually collected — must match the
//!    claimed credit, converted through the single [`usd_to_cents`] helper
//!    that also produces the quote, so the two directions agree by
//!    construction and no float ever touches money.
//! 2. **ZeroRouter must have priced the session.** A
//!    `stripe_checkout_intents` row written at session creation
//!    (migration `0005`) must exist and agree on user, amount, and currency.
//!    Layer 1 alone still trusts that any paid session in the account is ours;
//!    this layer does not.
//!
//! The dollars credited and the user credited both come from that stored
//! record, never from the event. A session created before migration 0005 has
//! no record and is rejected — see [`stripe_webhook`].
//!
//! # Events consumed
//!
//! | Event | Action |
//! |---|---|
//! | `checkout.session.completed` / `.async_payment_succeeded` | credit the purchase, once per session id |
//! | `payment_intent.succeeded` / `.payment_failed` | settle or fail an autopay charge (migration 0008) |
//! | `charge.dispute.created` | freeze the account and reverse the credit (migration 0009) |
//! | `charge.refunded` | reverse the credit; no freeze (migration 0009) |
//!
//! # Where the payment form lives
//!
//! Inside the portal, not on a Stripe-hosted page. Sessions are created with
//! `ui_mode=embedded_page`, which makes Stripe return a `client_secret`
//! instead of a redirect `url`; the portal mounts the form in a modal with
//! Stripe.js, and the customer never leaves the Credits page. `success_url`
//! and `cancel_url` cannot be sent in this mode — Stripe rejects them — so a
//! single `return_url` carrying the `{CHECKOUT_SESSION_ID}` template variable
//! replaces both, and the page it lands on reads the session's status to
//! decide what to show.
//!
//! **None of that changes what moves money.** The return page is a display
//! surface: it asks [`checkout_status`], which is read-only and refuses
//! sessions the caller does not own. Credit is applied by the signed webhook
//! and nothing else, exactly as before, so a customer who closes the tab
//! before returning is still credited and a customer who forges the return
//! page's answer gains nothing.
//!
//! # Sales tax
//!
//! Checkout Sessions are created with `automatic_tax[enabled]=true` and
//! **nothing else**. Stripe determines whether tax is due from the buyer's
//! address and the registrations configured in the dashboard, and it takes the
//! product tax code and the tax behavior from Tax Settings, because this
//! request deliberately specifies neither: "If you don't specify a tax code,
//! Stripe Tax uses the default tax code from your Tax Settings", and the same
//! fallback governs tax behavior.
//!
//! ## Why the policy lives in the dashboard and not here
//!
//! Because it is not settled, and it is not ours to settle. The correct
//! treatment of prepaid credits is genuinely contested — Massachusetts has
//! published authority pointing at redemption-time (M.G.L. c. 64H § 1's "rights
//! and credits" exclusion, Directive 12-4, LR 16-1) while a draft revision of
//! 830 CMR 64H.1.3 points the other way, and no US authority addresses per-token
//! AI APIs at all. That is an accountant's determination, it is likely to
//! change, and Stripe's own guidance to integrators is not to make the legal
//! classification on the seller's behalf.
//!
//! A value in this file would mean every revision of that determination is a
//! code change, a review, and a deploy. In Tax Settings it is a dropdown, it
//! takes effect immediately, and one setting governs checkout and any future
//! Stripe-billed surface alike rather than each hardcoding its own answer. The
//! research behind the recommended selections is preserved below — it is real
//! work and the operator will need it — but as guidance for what to choose in
//! the dashboard, not as a value this code transmits.
//!
//! Not sending `tax_behavior` has a second benefit: Stripe refuses to change a
//! `tax_behavior` once set on a Price, so pinning it per session pinned it
//! per session forever. Leaving it to Tax Settings keeps it revisable.
//!
//! ## What to select in Tax Settings
//!
//! **Default tax behavior — must be `Exclusive`** (or `Automatic`, which
//! resolves to exclusive for USD and CAD; equivalent here because
//! [`CHECKOUT_CURRENCY`] is USD, but `Exclusive` stays correct if a second
//! currency is ever added). This is not a preference. The ToS says prices are
//! exclusive of taxes; [`DEPOSIT_FEE_FLOOR_USD`] is sized so the gross covers
//! the credit plus Stripe's per-charge cost, so tax carved OUT of the gross
//! would consume more than the whole margin on the smallest deposit; and the
//! ledger records the ex-tax gross, so a session that carved tax out of it
//! collected less than ZeroRouter sold. **Selecting `Inclusive` does not
//! silently under-collect — it stops purchases working**: every session then
//! arrives at the webhook as a short payment and credits nothing. That is the
//! designed outcome (money is never credited against money that did not
//! arrive), but it is a total checkout outage, so get this one right.
//!
//! **Preset product tax code — recommended starting selection
//! `txcd_10105001`** (AIaaS – Cloud Based – Personal Use), pending the
//! accountant. Two separate questions sit behind it.
//!
//! *Which product?* Answered with reasonable confidence. Stripe publishes
//! dedicated AI-service codes and asks sellers to match delivery model and
//! customer; ZeroRouter is delivered entirely over the cloud with nothing
//! downloaded, which is the "Cloud Based" pair (`txcd_10105001` personal /
//! `txcd_10105002` business). Stripe explicitly warns against the generic
//! `txcd_10000000` for US sales. Personal use is the half ZeroRouter can
//! actually evidence — checkout is self-serve and no business identifier or tax
//! ID is collected — and of the two it errs toward collecting.
//!
//! *When is tax due?* Not answered, by anyone. The sale-versus-redemption
//! question above is unresolved, and a product tax code cannot express timing,
//! so no selection here settles it. What the architecture settles is narrower:
//! redemption is a metered balance debit in [`crate::billing`] with no Stripe
//! object and no tax computation, so a stored-value code (`txcd_10502000`, which
//! Stripe calls multi-purpose) defers tax to a point that will never collect it
//! — in practice a choice to collect nothing anywhere. Avoid `txcd_00000000`
//! (Nontaxable) for a different reason: it makes Stripe's
//! `taxability_reason=not_collecting` indistinguishable from a missing
//! registration, hiding real misconfiguration. Massachusetts DOR issues letter
//! rulings for exactly this situation (830 CMR 62C.3.1(6)).
//!
//! Whichever is selected, it does not fix transactions already taken: Stripe
//! cannot retroactively correct a sale that collected the wrong tax.
//!
//! ## Tax IDs and reverse charge
//!
//! Sessions also carry `tax_id_collection[enabled]=true`, which makes the
//! embedded form show a VAT/tax-ID field when the buyer's address is somewhere
//! Stripe supports one. It is **optional for the buyer**: `required` is left at
//! its default `never`, because the alternative (`if_supported`) makes a tax ID
//! mandatory for everyone in a supported billing country, and an EU consumer has
//! no business tax ID to give. For a self-serve product that is a checkout
//! outage, not a policy.
//!
//! The point of collecting it is **reverse charge**: on a cross-border B2B sale
//! of services into the EU or UK, a VAT-registered buyer accounts for the VAT
//! themselves, the seller collects zero, and the invoice must cite the buyer's
//! VAT number. Stripe applies this automatically when a valid tax ID is present
//! and the jurisdictions line up. Collecting it is also what lets a US business
//! buyer self-identify, though nothing in US sales tax acts on that today.
//!
//! **What it does NOT do — read this before expecting a number to change.**
//! Reverse charge only produces a visible change where the operator is
//! REGISTERED to collect VAT in the first place (an EU OSS or UK registration).
//! Stripe calculates zero tax for an unregistered jurisdiction regardless — see
//! failure mode 3 below — so with only the Massachusetts registration in place,
//! an EU buyer's session collects zero tax whether or not they enter a VAT
//! number. The field is collected and recorded; the tax was already zero. It
//! likewise changes nothing on a US sale: reverse charge is a VAT mechanism and
//! US sales tax has no equivalent, so a US business entering an EIN is taxed
//! exactly as before. "Tax ID entered" and "tax went to zero" are independent
//! facts, and only the registration list connects them.
//!
//! ## What the reverse-charged event looks like, and why nothing here changes
//!
//! A reverse-charged purchase arrives at the webhook as `amount_total` equal to
//! the ex-tax gross, `total_details.amount_tax = 0`, `automatic_tax.status =
//! complete`, and the buyer's id in `customer_details.tax_ids[]`. That is
//! numerically identical to an untaxed session, which is the whole reason the
//! ex-tax accounting needs no new case: [`collected_ex_tax_cents`] subtracts a
//! zero and the corroborations compare the same figures they always did, so the
//! buyer is credited exactly `credit_usd`. `customer_details` is not read by
//! this module at all.
//!
//! **The tax ID is deliberately not stored.** A reverse-charge invoice must cite
//! the buyer's VAT number, so the operator does need it — but Stripe already
//! keeps it, on the Checkout Session (`customer_details.tax_ids[]`) and in the
//! Tax reports that a VAT return is filed from, which is where the rest of the
//! filing figures come from anyway. Copying it into `stripe_checkout_intents`
//! would mean a migration, a second copy of a customer identifier to keep
//! correct, and a new answer to give a deletion request — to duplicate a record
//! the filing workflow does not read. Retrieve it with
//! `stripe checkout sessions retrieve <id>` (or Dashboard → Payments → the
//! session), or in bulk from Tax → Registrations → reports, which break out
//! reverse-charged transactions with the buyer's tax ID per row. Revisit this
//! only if the operator's accountant needs the ID inside ZeroRouter's own books
//! rather than at filing time.
//!
//! ## Where the tax lands
//!
//! Nowhere in ZeroRouter's ledger, and that is the point:
//!
//! | Money | Where it goes |
//! |---|---|
//! | `credit_usd` | the user's spendable balance, via a `purchase` ledger row |
//! | the deposit fee | revenue; never a ledger row, derivable as `expected_amount_cents - expected_credit_usd * 100` |
//! | sales tax | collected by Stripe on ZeroRouter's behalf and owed to a taxing jurisdiction — not revenue, not balance, and not recorded here |
//!
//! The table above describes CHECKOUT, but the same three rules now hold for
//! autopay: a raw PaymentIntent still takes no `automatic_tax`, so autopay
//! prices tax through the Tax Calculation API instead and records its own tax
//! transaction — see the autopay section below. Tax lands nowhere in the ledger
//! on either path.
//!
//! `stripe_checkout_intents.expected_amount_cents` therefore keeps meaning the
//! **ex-tax** gross ZeroRouter quoted, so fee revenue stays exactly
//! `gross - credit` and tax can never be mistaken for either. The consequence
//! is that it no longer equals the amount Stripe charged the card; the tax
//! figures live at Stripe (Tax reports, the balance transaction) and nowhere
//! else, so reconciling ZeroRouter against a Stripe payout means adding the
//! tax back from Stripe's side. Recording tax locally would need a new column
//! and so a migration.
//!
//! ## What this means for the webhook
//!
//! "The amount charged equals the gross" stopped being true. Every
//! ZeroRouter-side figure is ex-tax, so the corroborations compare against
//! [`collected_ex_tax_cents`] — the money that moved, less the part that is
//! not ours — never against `amount_total` directly.
//!
//! ## Operator prerequisites — three different failure modes
//!
//! Every tax decision now lives in dashboard state that no deployment step
//! checks, and the pieces fail in three unrelated ways:
//!
//! 1. **Stripe Tax not activated on the account** — Stripe rejects the session
//!    creation outright (`stripe_tax_inactive`), so `POST /api/billing/checkout`
//!    returns 502 `checkout_failed` and NOBODY can buy credits. Loud, total, and
//!    immediate on deploy.
//! 2. **Default tax behavior set to `Inclusive`** — sessions are created fine
//!    and the customer pays, but the tax is carved out of ZeroRouter's price
//!    instead of added to it, so every event reaches the webhook as a short
//!    payment and credits nothing. Purchases fail closed: money collected, no
//!    credit, `amount_mismatch` in Stripe's webhook dashboard. Correct
//!    behaviour, awful outcome — set `Exclusive` (or `Automatic`).
//! 3. **Activated, but no registration covering the buyer** — Stripe accepts
//!    the session and calculates zero tax. Checkout works, purchases credit
//!    normally, and nothing in these logs says tax is not being collected. This
//!    is the quiet one, and it is the failure Stripe itself calls the most
//!    common Stripe Tax mistake.
//!
//! Note the shape of that list: since this request no longer carries the tax
//! code or the behavior, a wrong preset in Tax Settings is now the ONLY thing
//! standing between a correct deployment and mode 2. The trade is deliberate —
//! the policy becomes revisable without a deploy — but it moves a load-bearing
//! setting out of code review, so it belongs in the deployment checklist
//! instead. `docs/DEPLOY.md` carries it.
//!
//! Ordering: activation and the presets must precede deployment; registration
//! must precede the first live purchase, because Stripe cannot retroactively
//! correct a transaction that collected the wrong tax. All of it is per
//! environment — a sandbox's registrations do not carry to live mode. Neither
//! the code nor a green test proves tax is being collected: only a real
//! transaction with a non-zero tax line does.
//!
//! Everything else is acknowledged without action so Stripe stops retrying it.
//! **The Stripe endpoint must be subscribed to the events above** — an event
//! Stripe does not send is an event this code never runs (see
//! `docs/DEPLOY.md`).
//!
//! The reversal arm reads none of the metadata the crediting arms have to
//! defend: a dispute is mapped to a user through Stripe's own `payment_intent`
//! id joined against ZeroRouter's ledger — see [`handle_reversal_event`].
//!
//! Nothing in this module ever logs the Stripe secret key, the webhook
//! secret, a signature value, or a request/response body.

use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

use crate::{
    billing::{self, CreditOutcome},
    session::PortalUser,
    sqlx,
    web::{StripeSettings, WebCtx},
};

/// Header carrying Stripe's `t=<unix>,v1=<hex>` webhook signature.
pub const STRIPE_SIGNATURE_HEADER: &str = "stripe-signature";

/// Maximum accepted skew between the signed timestamp and the current time.
const WEBHOOK_TOLERANCE: Duration = Duration::from_secs(300);
fn checkout_sessions_url(settings: &StripeSettings) -> String {
    format!("{}/v1/checkout/sessions", settings.api_base)
}
/// Retrieval url for one session. The id is not percent-encoded because it has
/// already been matched against a `stripe_checkout_intents` row written by this
/// deployment — it is our own stored id, not the caller's string.
fn checkout_session_url(settings: &StripeSettings, session_id: &str) -> String {
    format!("{}/v1/checkout/sessions/{session_id}", settings.api_base)
}
const STRIPE_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CHECKOUT_COMPLETED_EVENT: &str = "checkout.session.completed";
const CHECKOUT_ASYNC_SUCCEEDED_EVENT: &str = "checkout.session.async_payment_succeeded";
/// A charge was refunded, in whole or in part. The event object is the CHARGE,
/// so its `amount_refunded` is CUMULATIVE across every refund on it.
const CHARGE_REFUNDED_EVENT: &str = "charge.refunded";
/// A cardholder disputed a charge. The event object is the DISPUTE; the money
/// has already left the ZeroRouter balance at Stripe by the time it arrives.
const DISPUTE_CREATED_EVENT: &str = "charge.dispute.created";
const CHECKOUT_PRODUCT_NAME: &str = "ZeroRouter credits";
/// Render Checkout as a form inside the portal rather than on a Stripe-hosted
/// page. Stripe's enum spells this `embedded_page` (`hosted_page` is the
/// default, and `elements` is the lower-level Checkout-elements mode).
const CHECKOUT_UI_MODE: &str = "embedded_page";
/// Header Stripe reads to pin the API version of a request. Lowercase because
/// `HeaderName::from_static` requires it; HTTP header names are
/// case-insensitive and reqwest normalizes to lowercase on the wire anyway.
pub const STRIPE_VERSION_HEADER: &str = "stripe-version";
/// The API version the checkout requests are pinned to, and **the reason they
/// have to be**.
///
/// [`CHECKOUT_UI_MODE`] does not exist before this version. Dahlia renamed the
/// `ui_mode` enum (`hosted`/`embedded`/`custom` →
/// `hosted_page`/`embedded_page`/`elements`) and the changelog marks it
/// BREAKING; `2026-03-25.dahlia` is the opening version of that release train,
/// so it is the earliest version that accepts `embedded_page`.
///
/// Without this header a request runs at whatever version the **account** is
/// pinned to in the dashboard. An account created before Dahlia would reject
/// `ui_mode=embedded_page` outright, turning every purchase into a 502
/// `checkout_failed` — a total checkout outage that no test here would catch,
/// because the mock accepts any form. It is also not something a sandbox can
/// clear: a sandbox defaults to the version current when it was created, so a
/// green sandbox says nothing about an older live account.
///
/// The opening version is deliberately chosen over the newest Dahlia release.
/// Later versions in a train are additive by policy, so nothing is lost, and
/// pinning the opener avoids inheriting later breaking changes — `2026-07-29`
/// renames a Checkout `collected_information` property, which this integration
/// does not read today but would silently depend on if the pin drifted forward.
///
/// This matches the client: the portal loads Stripe.js from the `dahlia`
/// bundle (it calls `createEmbeddedCheckoutPage`, itself a Dahlia rename), and
/// Stripe's guidance is to keep Stripe.js and the server-side API version on
/// the same release train.
///
/// **Scope: the two checkout calls only.** See [`checkout_client`].
pub const CHECKOUT_API_VERSION: &str = "2026-03-25.dahlia";
/// The portal route Checkout returns the browser to once the payment attempt
/// finishes. `{CHECKOUT_SESSION_ID}` is Stripe's template variable, replaced
/// with the real session id at redirect time — it is deliberately NOT a format
/// placeholder, so it must survive into the request verbatim.
const CHECKOUT_RETURN_PATH: &str = "/credits/return?session_id={CHECKOUT_SESSION_ID}";
/// The one ISO-4217 currency ZeroRouter prices checkout in. Quoted to Stripe
/// at session creation, stored on the pending record, and re-checked against
/// the webhook's `currency` — an amount match alone is not proof of the price,
/// because the smallest unit of a zero-decimal currency (1000 JPY, roughly $6)
/// numerically equals a cents amount ($10.00).
const CHECKOUT_CURRENCY: &str = "usd";
/// SQLSTATE for a foreign-key violation: the metadata user does not exist.
const PG_FOREIGN_KEY_VIOLATION: &str = "23503";

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Webhook signature verification (pure)
// ---------------------------------------------------------------------------

/// Why a `stripe-signature` header failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebhookVerifyError {
    /// Not in Stripe's `t=<unix>,v1=<hex>` format: the timestamp is missing
    /// or unparseable, or there is no `v1` candidate at all.
    #[error("the stripe-signature header is malformed")]
    MalformedHeader,
    /// The signed timestamp is further from now than the tolerance allows.
    #[error("the stripe-signature timestamp is outside the accepted tolerance")]
    TimestampOutOfTolerance,
    /// No `v1` candidate matches the recomputed HMAC.
    #[error("no stripe-signature candidate matches the payload")]
    SignatureMismatch,
}

/// Verify a Stripe webhook signature header against the raw request body.
///
/// Stripe signs `{t}.{payload}` with HMAC-SHA256 under the endpoint secret
/// and sends `t=<unix>,v1=<hex>[,v1=<hex>...]`. Verification succeeds when
/// the timestamp is within `tolerance` of `now_unix` and **any** `v1`
/// candidate matches the recomputed digest (constant-time comparison via
/// [`Mac::verify_slice`]). Every ambiguous input fails closed.
pub fn verify_webhook_signature(
    payload: &[u8],
    signature_header: &str,
    secret: &str,
    tolerance: Duration,
    now_unix: i64,
) -> Result<(), WebhookVerifyError> {
    let parsed = parse_signature_header(signature_header)?;
    if now_unix.abs_diff(parsed.timestamp) > tolerance.as_secs() {
        return Err(WebhookVerifyError::TimestampOutOfTolerance);
    }
    // The digest depends only on the timestamp and the payload, so it is
    // computed ONCE and compared against each candidate. Rebuilding it per
    // candidate let an unauthenticated caller — the webhook endpoint is
    // public by necessity — force arbitrary hashing work: a few thousand
    // `v1=` fields against a large body is hundreds of megabytes of SHA-256
    // before anything is rejected (sol review).
    //
    // HMAC-SHA256 accepts keys of any length, so construction cannot fail;
    // if it somehow does, fail closed.
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return Err(WebhookVerifyError::SignatureMismatch);
    };
    // Sign the exact timestamp string from the header, not a re-rendered
    // integer, so byte-level oddities cannot desynchronize the digest.
    mac.update(parsed.timestamp_raw.as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = mac.finalize().into_bytes();
    for candidate in parsed.candidates {
        // A non-hex candidate can never match; skip it rather than abort so a
        // valid sibling signature (e.g. during secret rotation) still passes.
        let Ok(candidate_bytes) = hex::decode(candidate) else {
            continue;
        };
        if candidate_bytes.len() == expected.len() && constant_time_eq(&candidate_bytes, &expected)
        {
            return Ok(());
        }
    }
    Err(WebhookVerifyError::SignatureMismatch)
}

struct ParsedSignatureHeader<'a> {
    timestamp: i64,
    timestamp_raw: &'a str,
    candidates: Vec<&'a str>,
}

/// Signatures Stripe can plausibly send at once: the current secret plus
/// one being rotated in leaves room to spare. Anything beyond this is an
/// attempt to make the endpoint do work, not to authenticate.
const MAX_SIGNATURE_CANDIDATES: usize = 8;

/// Length of a hex-encoded SHA-256 digest. A candidate of any other length
/// cannot match, so it is not worth decoding.
const SIGNATURE_HEX_LEN: usize = 64;

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn parse_signature_header(header: &str) -> Result<ParsedSignatureHeader<'_>, WebhookVerifyError> {
    let mut timestamp_raw = None;
    let mut candidates = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => timestamp_raw = Some(value.trim()),
            "v1" => {
                let value = value.trim();
                // Only well-formed candidates are kept, and only a few: an
                // unbounded list is a work amplifier, not a signature.
                if value.len() == SIGNATURE_HEX_LEN && candidates.len() < MAX_SIGNATURE_CANDIDATES {
                    candidates.push(value);
                }
            }
            // v0 (legacy) and future schemes are ignored, per Stripe's docs.
            _ => {}
        }
    }
    let timestamp_raw = timestamp_raw.ok_or(WebhookVerifyError::MalformedHeader)?;
    let timestamp = timestamp_raw
        .parse::<i64>()
        .map_err(|_| WebhookVerifyError::MalformedHeader)?;
    if candidates.is_empty() {
        return Err(WebhookVerifyError::MalformedHeader);
    }
    Ok(ParsedSignatureHeader {
        timestamp,
        timestamp_raw,
        candidates,
    })
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

/// Routes owned by the Stripe integration: portal checkout creation and the
/// webhook receiver.
pub fn router() -> Router<WebCtx> {
    Router::new()
        .route("/api/billing/checkout", post(create_checkout))
        .route(
            "/api/billing/checkout/status",
            axum::routing::get(checkout_status),
        )
        .route("/api/billing/quote", axum::routing::get(checkout_quote))
        .route(
            "/api/billing/autopay",
            axum::routing::get(get_autopay).put(put_autopay),
        )
        .route("/api/billing/autopay/setup", post(create_autopay_setup))
        .route("/webhooks/stripe", post(stripe_webhook))
}

#[derive(Debug)]
enum StripeHttpError {
    BillingUnavailable,
    InvalidAmount,
    CheckoutFailed,
    InvalidSignature,
    MalformedEvent,
    /// The signed event does not corroborate itself, or contradicts what
    /// ZeroRouter quoted for that session.
    AmountMismatch,
    /// A paid session ZeroRouter has no pending-purchase record for.
    UnknownSession,
    /// The portal asked about a Checkout Session that this deployment did not
    /// price for the authenticated user. Deliberately indistinguishable from
    /// "no such session".
    SessionNotFound,
    /// Stripe could not be asked what a session's status is. Distinct from
    /// [`Self::CheckoutFailed`] because nothing was being created — saying "the
    /// session could not be created" on a read misdirects whoever reads the
    /// log or the toast.
    StatusUnavailable,
    UnknownUser,
    DatabaseUnavailable,
}

impl IntoResponse for StripeHttpError {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            Self::BillingUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Stripe billing is not configured on this deployment.",
                "billing_unavailable",
            ),
            Self::InvalidAmount => (
                StatusCode::BAD_REQUEST,
                "The credit amount must be a USD value with at most two decimal places, within the configured checkout bounds.",
                "invalid_amount",
            ),
            Self::CheckoutFailed => (
                StatusCode::BAD_GATEWAY,
                "The Stripe checkout session could not be created; try again shortly.",
                "checkout_failed",
            ),
            Self::InvalidSignature => (
                StatusCode::BAD_REQUEST,
                "The webhook signature is missing or invalid.",
                "invalid_signature",
            ),
            Self::MalformedEvent => (
                StatusCode::BAD_REQUEST,
                "The webhook event is malformed.",
                "malformed_event",
            ),
            // 4xx rather than a silent 200: a mismatch is a security event,
            // and leaving it visibly failing in Stripe's webhook dashboard is
            // the alerting channel this deployment has.
            Self::AmountMismatch => (
                StatusCode::BAD_REQUEST,
                "The webhook event does not match the recorded checkout amount.",
                "amount_mismatch",
            ),
            Self::UnknownSession => (
                StatusCode::BAD_REQUEST,
                "The webhook event references a checkout session this deployment did not create.",
                "unknown_session",
            ),
            Self::SessionNotFound => (
                StatusCode::NOT_FOUND,
                "That checkout session was not found.",
                "session_not_found",
            ),
            Self::StatusUnavailable => (
                StatusCode::BAD_GATEWAY,
                "The payment status could not be read from Stripe just now. Your credits still \
                 arrive on their own if the payment went through — check the ledger.",
                "status_unavailable",
            ),
            Self::UnknownUser => (
                StatusCode::BAD_REQUEST,
                "The webhook event references an unknown user.",
                "unknown_user",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Credit application is temporarily unavailable.",
                "database_unavailable",
            ),
        };
        (
            status,
            Json(serde_json::json!({
                "error": { "message": message, "type": "billing_error", "code": code }
            })),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// POST /api/billing/checkout
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CheckoutRequest {
    /// `rust_decimal`'s deserializer accepts both JSON strings ("25.00") and
    /// JSON numbers (25), so no untagged wrapper is needed.
    amount_usd: Decimal,
}

async fn create_checkout(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Json(request): Json<CheckoutRequest>,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let amount_usd = request.amount_usd;
    // `amount_usd` is the CREDIT (net) the user picked. Validate its bounds and
    // whole-cent granularity, then price the deposit: the fee rides on top and
    // Stripe collects the gross.
    validate_checkout_amount(amount_usd, stripe)?;
    let quote = deposit_fee_quote(amount_usd);
    // Credit and fee are each whole cents, so the gross is too; refuse rather
    // than quote a sub-cent unit_amount Stripe would reject.
    let gross_cents = usd_to_cents(quote.gross_usd).ok_or(StripeHttpError::InvalidAmount)?;

    // Reuse an unpaid session this buyer already has at this exact price
    // rather than minting a second one.
    //
    // The portal unmounts Stripe's form on Cancel, Escape, backdrop click and
    // "Change amount", and mounts it again on the next Continue — each mount
    // calls this endpoint. Without this, one indecisive customer buying $25
    // once produces several Checkout Sessions and several
    // `stripe_checkout_intents` rows, none of which anything deletes.
    //
    // Safe because the key is `(user, gross_cents)`: the secret handed back
    // belongs to a session priced identically to the one this request would
    // have created, for the same person. Entries are dropped as soon as a
    // status read shows the session is no longer `open` (see
    // [`forget_session`]), and expire on their own well inside Stripe's 24h
    // session lifetime, so a paid or stale session is never re-served.
    if let Ok(cache) = reuse_cache().lock()
        && let Some(entry) = cache.get(&(user.user_id, gross_cents))
        && entry.at.elapsed() < SESSION_REUSE_TTL
    {
        tracing::debug!(
            user_id = %user.user_id,
            stripe_session_id = %entry.session_id,
            "reusing an open checkout session instead of creating another"
        );
        return Ok(Json(
            serde_json::json!({ "client_secret": entry.client_secret }),
        ));
    }

    let session = create_checkout_session(
        stripe,
        CheckoutSessionParams {
            user_id: user.user_id,
            customer_email: &user.email,
            credit_usd: amount_usd,
            fee_usd: quote.fee_usd,
            gross_usd: quote.gross_usd,
            unit_amount_cents: gross_cents,
            return_url: ctx.config.absolute_url(CHECKOUT_RETURN_PATH),
        },
    )
    .await?;
    // Persist what this session is worth BEFORE handing back the client
    // secret. The session id only exists after Stripe mints it, so the record
    // cannot precede the session — but it can precede the user ever seeing
    // the payment form. If this insert fails the secret is withheld, so the
    // session is unmountable and expires unpaid rather than becoming a
    // payment the webhook would (correctly) refuse to credit.
    if let Err(error) = billing::record_checkout_intent(
        &ctx.pool,
        &session.id,
        user.user_id,
        // Gross (charge) in expected_amount_cents, net (credit) in
        // expected_credit_usd — the webhook credits the net and corroborates
        // the gross.
        gross_cents,
        amount_usd,
        CHECKOUT_CURRENCY,
    )
    .await
    {
        tracing::error!(
            user_id = %user.user_id,
            stripe_session_id = %session.id,
            %error,
            "stripe checkout session created but its pending purchase record could not be \
             persisted; withholding the client secret so the session is never paid"
        );
        return Err(StripeHttpError::CheckoutFailed);
    }
    tracing::info!(
        user_id = %user.user_id,
        stripe_session_id = %session.id,
        credit_usd = %amount_usd,
        fee_usd = %quote.fee_usd,
        gross_usd = %quote.gross_usd,
        "created stripe checkout session"
    );
    // Only cached AFTER the intent row is durable. Caching earlier would let a
    // failed insert still hand out a secret for a session the webhook would
    // refuse to credit.
    if let Ok(mut cache) = reuse_cache().lock() {
        cache.insert(
            (user.user_id, gross_cents),
            ReuseEntry {
                session_id: session.id.clone(),
                client_secret: session.client_secret.clone(),
                at: std::time::Instant::now(),
            },
        );
    }
    // Shape change from the redirect era: this used to return `{"url"}`, the
    // Stripe-hosted page to send the browser to. An `embedded_page` session has
    // no such url — Stripe returns `url: null` — so returning one would mean
    // inventing it. The portal is the only consumer of this endpoint, and it
    // mounts the form in place; `/api/billing/autopay/setup` is a separate
    // endpoint and still returns `{"url"}` because card setup remains a
    // redirect.
    Ok(Json(
        serde_json::json!({ "client_secret": session.client_secret }),
    ))
}

// ---------------------------------------------------------------------------
// GET /api/billing/checkout/status
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CheckoutStatusParams {
    session_id: String,
}

/// How long a session's status may be served from memory before Stripe is
/// asked again. Short enough that the return page still feels live, long
/// enough that a client refreshing in a loop cannot turn one customer into a
/// stream of Stripe reads.
const STATUS_CACHE_TTL: Duration = Duration::from_secs(8);

/// How long a created-but-unpaid session may be handed back to the same buyer
/// for the same amount instead of creating a second one.
///
/// Well under Stripe's 24h session expiry, so a reused session is always still
/// mountable. The cap matters because nothing deletes
/// `stripe_checkout_intents` rows: every abandoned mount used to leave one
/// behind, and opening the modal, closing it, and reopening it three times
/// created three sessions for one purchase.
const SESSION_REUSE_TTL: Duration = Duration::from_secs(600);

/// Stripe's prefix for Checkout Session ids. Anything else cannot be one, so
/// it is refused before it reaches a database query or a URL.
const CHECKOUT_SESSION_ID_PREFIX: &str = "cs_";

struct StatusEntry {
    status: String,
    at: std::time::Instant,
}

struct ReuseEntry {
    session_id: String,
    client_secret: String,
    at: std::time::Instant,
}

/// Cached session statuses, keyed by session id.
fn status_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, StatusEntry>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, StatusEntry>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Reusable unpaid sessions, keyed by `(user, ex-tax gross in cents)`.
///
/// Keyed by the buyer AND the price so a reused session can never be one
/// quoted for a different amount — the client secret handed back always
/// belongs to a session priced exactly as this request would have priced it.
fn reuse_cache() -> &'static std::sync::Mutex<std::collections::HashMap<(Uuid, i64), ReuseEntry>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(Uuid, i64), ReuseEntry>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Drop a session from both caches once it is known to be unusable (paid,
/// expired, or gone). Without this a customer who pays $25 and immediately
/// buys another $25 would be handed the completed session back.
fn forget_session(user_id: Uuid, gross_cents: i64, session_id: &str) {
    if let Ok(mut cache) = reuse_cache().lock()
        && cache
            .get(&(user_id, gross_cents))
            .is_some_and(|entry| entry.session_id == session_id)
    {
        cache.remove(&(user_id, gross_cents));
    }
}

/// GET /api/billing/checkout/status?session_id=S — report whether a Checkout
/// Session finished, so the return page can say "paid" or re-open the form.
///
/// # THIS ENDPOINT NEVER CREDITS, AND NOTHING IT RETURNS IS TRUSTED AS PAYMENT
///
/// It is **display only**. Crediting stays exactly where it was: the
/// `checkout.session.completed` webhook, behind an HMAC, behind the two
/// corroborations documented at the top of this module. That separation is the
/// whole point of the split — the browser is told what to render, and the
/// ledger is moved by an event Stripe signed. A customer who calls this
/// endpoint, or who edits its response, changes what their own screen says and
/// nothing else; a customer who never loads the return page at all is still
/// credited, because the webhook does not depend on them coming back.
///
/// It performs no writes of any kind. There is no code path from here into
/// [`billing::credit_purchase`].
///
/// # Whose session this is
///
/// `session_id` is supplied by the client, so it is not trusted as an
/// authorization: the session is first looked up in `stripe_checkout_intents`
/// — the row this deployment wrote when it priced the session — and the
/// authenticated user must be the one it was priced for. Anything else reads
/// as not-found, so this cannot be used to enumerate sessions or to observe
/// another customer's purchase. Stripe is only consulted after that check
/// passes.
async fn checkout_status(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Query(params): Query<CheckoutStatusParams>,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    // Shape-check before anything else. A Checkout Session id always starts
    // `cs_`; without this a caller could send `%00` or an arbitrary string and
    // get a database round trip and an error log line out of it.
    if !params.session_id.starts_with(CHECKOUT_SESSION_ID_PREFIX) {
        return Err(StripeHttpError::SessionNotFound);
    }
    let intent = billing::checkout_intent(&ctx.pool, &params.session_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "checkout intent lookup failed");
            StripeHttpError::DatabaseUnavailable
        })?;
    // A session this deployment never priced, and a session priced for someone
    // else, are the same answer on purpose — distinguishing them would confirm
    // the existence of another user's session.
    let Some(intent) = intent.filter(|intent| intent.user_id == user.user_id) else {
        return Err(StripeHttpError::SessionNotFound);
    };

    // Serve a recent answer rather than re-asking Stripe. The ownership check
    // above has already run, so a cache hit is never a way to read someone
    // else's session.
    if let Ok(cache) = status_cache().lock()
        && let Some(entry) = cache.get(&intent.stripe_session_id)
        && entry.at.elapsed() < STATUS_CACHE_TTL
    {
        return Ok(Json(serde_json::json!({ "status": entry.status })));
    }

    let status = retrieve_session_status(stripe, &intent.stripe_session_id)
        .await
        .map_err(|_| StripeHttpError::StatusUnavailable)?;

    // A session that is no longer open must not be handed back to the next
    // purchase of the same amount.
    if status != "open" {
        forget_session(
            intent.user_id,
            intent.expected_amount_cents,
            &intent.stripe_session_id,
        );
    }
    if let Ok(mut cache) = status_cache().lock() {
        cache.insert(
            intent.stripe_session_id.clone(),
            StatusEntry {
                status: status.clone(),
                at: std::time::Instant::now(),
            },
        );
    }
    Ok(Json(serde_json::json!({ "status": status })))
}

/// The HTTP client shared by the two checkout calls.
///
/// One client, built once: `reqwest::Client` owns the connection pool, so a
/// fresh one per request means a fresh TLS handshake per request. The status
/// endpoint is customer-triggered and can be called in a loop, which made
/// per-call construction a way to burn connections against Stripe.
///
/// **Only the checkout create and status retrieve use this.** The autopay
/// paths keep their own [`stripe_client`] deliberately: this client pins
/// [`CHECKOUT_API_VERSION`] on every request, and autopay creates
/// PaymentIntents, Customers, and setup-mode sessions that have not been
/// audited against Dahlia's breaking Payments changes. None of them send
/// `ui_mode`, so none of them need the pin, and moving them onto a new API
/// version as a side effect of an embedded-checkout change would be exactly
/// the kind of silent money-path shift this repo does not do.
fn checkout_client() -> Result<&'static reqwest::Client, CheckoutError> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let mut headers = reqwest::header::HeaderMap::new();
            // Pinned as a DEFAULT header on the client rather than per call, so
            // a future request added to this client cannot forget it.
            headers.insert(
                reqwest::header::HeaderName::from_static(STRIPE_VERSION_HEADER),
                reqwest::header::HeaderValue::from_static(CHECKOUT_API_VERSION),
            );
            reqwest::Client::builder()
                .timeout(STRIPE_HTTP_TIMEOUT)
                .default_headers(headers)
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| {
            tracing::warn!("stripe HTTP client construction failed");
            CheckoutError::Client
        })
}

/// Read a Checkout Session's `status` (`open`, `complete`, or `expired`) from
/// Stripe. Only the status is parsed — no money field is read here, because no
/// decision made from this response involves money.
async fn retrieve_session_status(
    settings: &StripeSettings,
    session_id: &str,
) -> Result<String, CheckoutError> {
    let response = checkout_client()?
        .get(checkout_session_url(settings, session_id))
        .bearer_auth(&settings.secret_key)
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                timeout = error.is_timeout(),
                "stripe checkout session retrieval failed"
            );
            CheckoutError::Request
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            status = status.as_u16(),
            "stripe rejected the checkout session retrieval"
        );
        return Err(CheckoutError::Status);
    }
    let session: CheckoutSessionStatusResponse = response.json().await.map_err(|_| {
        tracing::warn!("stripe checkout session retrieval could not be parsed");
        CheckoutError::MalformedResponse
    })?;
    session.status.ok_or_else(|| {
        tracing::warn!("stripe checkout session retrieval is missing the status");
        CheckoutError::MalformedResponse
    })
}

#[derive(Debug, Deserialize)]
struct CheckoutSessionStatusResponse {
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuoteParams {
    /// The credit (net) amount to price. Same deserializer flexibility as
    /// `CheckoutRequest.amount_usd`: JSON/query string or number.
    credit: Decimal,
}

/// GET /api/billing/quote?credit=C — price a deposit server-side so the portal
/// can show "you pay $gross (includes $fee) → receive $credit" without ever
/// recomputing the fee in TypeScript. Returns `{credit, fee, gross}` from
/// [`deposit_fee_quote`], the same source of truth the charge paths consume.
async fn checkout_quote(
    State(ctx): State<WebCtx>,
    _user: PortalUser,
    Query(params): Query<QuoteParams>,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let credit_usd = params.credit;
    // The same bounds and whole-cent validation the checkout enforces, so a
    // quote never advertises a price the checkout would then refuse.
    validate_checkout_amount(credit_usd, stripe)?;
    let quote = deposit_fee_quote(credit_usd);
    usd_to_cents(quote.gross_usd).ok_or(StripeHttpError::InvalidAmount)?;
    Ok(Json(serde_json::json!({
        "credit": quote.credit_usd,
        "fee": quote.fee_usd,
        "gross": quote.gross_usd,
    })))
}

/// Validate a requested credit amount and convert it to integer cents.
///
/// Rejects amounts outside the configured `[min, max]` bounds and amounts
/// with more than two decimal places (Stripe charges integer cents; anything
/// finer would silently round money).
fn validate_checkout_amount(
    amount_usd: Decimal,
    settings: &StripeSettings,
) -> Result<i64, StripeHttpError> {
    if amount_usd < settings.checkout_min_usd || amount_usd > settings.checkout_max_usd {
        return Err(StripeHttpError::InvalidAmount);
    }
    usd_to_cents(amount_usd).ok_or(StripeHttpError::InvalidAmount)
}

/// Convert decimal USD to the integer smallest currency unit Stripe quotes,
/// collects, and reports in `amount_total`.
///
/// The single conversion in this module: the checkout quote and the webhook's
/// amount check both go through it, so "what we asked Stripe to collect" and
/// "what we require Stripe to have collected" agree by construction rather
/// than by two independently maintained expressions. Exact `Decimal`
/// arithmetic throughout — no float ever touches money.
///
/// `None` when the amount is finer than a cent (anything finer would silently
/// round money) or does not fit an `i64`.
fn usd_to_cents(amount_usd: Decimal) -> Option<i64> {
    if amount_usd.normalize().scale() > 2 {
        return None;
    }
    (amount_usd * Decimal::ONE_HUNDRED).normalize().to_i64()
}

// ---------------------------------------------------------------------------
// Deposit fee — ONE helper, the single source of truth for the fee math
// ---------------------------------------------------------------------------

/// The deposit-fee rate: 5.5% of the credit the user buys, collected on top as
/// a surcharge. `Decimal` literal 55 / 10^3 = 0.055; never a float.
const DEPOSIT_FEE_RATE: Decimal = Decimal::from_parts(55, 0, 0, false, 3);

/// The minimum fee, so a small deposit still covers Stripe's fixed per-charge
/// cost. Stripe US pricing is 2.9% + $0.30; on the smallest allowed deposit
/// ($5, charged as $5.80 gross) that is 0.029*5.80 + 0.30 = $0.468, leaving
/// ZeroRouter $0.80 - $0.468 = $0.332 above water after granting the $5 credit.
/// Below this floor the percentage fee ($0.28 on $5) would not clear the $0.30
/// fixed component and every small deposit would lose money. `Decimal` literal
/// 80 / 10^2 = 0.80.
///
/// **Stripe Tax narrows that headroom and this number has NOT been re-sized for
/// it.** Two costs move once tax is collected: the percentage card fee applies
/// to the taxed total rather than the gross, and Stripe Tax bills roughly 0.5%
/// per transaction in jurisdictions where the seller is registered. On the same
/// smallest deposit at Massachusetts' 6.25% — $5.80 gross, $0.36 tax, $6.16
/// charged — that is 0.029*6.16 + 0.30 + 0.005*6.16 = $0.510, leaving $0.290
/// rather than $0.332. Still above water, so nothing here changes; re-pricing
/// the floor is an owner decision, not a side effect of enabling tax. Note the
/// erosion is bounded to jurisdictions with an active registration, because
/// Stripe Tax only bills where it actually calculates.
const DEPOSIT_FEE_FLOOR_USD: Decimal = Decimal::from_parts(80, 0, 0, false, 2);

/// A priced deposit: the credit the user picked, the fee charged on top, and
/// the gross Stripe collects. Every field is exact `Decimal`; `gross_usd` is a
/// whole number of cents by construction, so it survives [`usd_to_cents`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DepositFeeQuote {
    credit_usd: Decimal,
    fee_usd: Decimal,
    gross_usd: Decimal,
}

/// Price a deposit. The ONLY place the fee math lives — the checkout path, the
/// autopay charge path, the webhook corroborations, and the portal quote
/// endpoint all consume this, so "what we charge" and "what we require Stripe to
/// have collected" can never drift.
///
/// `fee = max(ceil_to_cent(rate * credit), floor)`, `gross = credit + fee`. The
/// fee is CEILED to the whole cent so ZeroRouter is never undercharged, and the
/// caller is expected to have already validated `credit_usd` to whole cents, so
/// `gross_usd` is whole cents too.
fn deposit_fee_quote(credit_usd: Decimal) -> DepositFeeQuote {
    let percentage_fee = (DEPOSIT_FEE_RATE * credit_usd)
        .round_dp_with_strategy(2, RoundingStrategy::ToPositiveInfinity);
    let fee_usd = percentage_fee.max(DEPOSIT_FEE_FLOOR_USD);
    DepositFeeQuote {
        credit_usd,
        fee_usd,
        gross_usd: credit_usd + fee_usd,
    }
}

struct CheckoutSessionParams<'a> {
    user_id: Uuid,
    customer_email: &'a str,
    /// The NET credit the session buys, stamped into `metadata[credit_usd]`.
    credit_usd: Decimal,
    /// The deposit fee, stamped into `metadata[fee_usd]` for reconciliation.
    fee_usd: Decimal,
    /// The GROSS Stripe collects, stamped into `metadata[gross_usd]`.
    gross_usd: Decimal,
    /// The gross, in cents — the `unit_amount` Stripe actually charges.
    unit_amount_cents: i64,
    /// Where Checkout sends the browser once the payment attempt finishes.
    /// Carries the `{CHECKOUT_SESSION_ID}` template variable, which Stripe
    /// substitutes before redirecting.
    return_url: String,
}

struct CheckoutSession {
    id: String,
    /// The value the browser mounts Embedded Checkout with. This is NOT a
    /// bearer credential for the session's money — it only lets Stripe.js
    /// render the payment form — but it is still session-scoped, so it is
    /// returned to the one authenticated user the session was priced for and
    /// never logged.
    client_secret: String,
}

#[derive(Debug, Deserialize)]
struct CheckoutSessionResponse {
    id: String,
    /// Present only for `ui_mode=embedded_page`. A `hosted_page` session
    /// returns `url` and a null `client_secret`; the reverse holds here, so a
    /// missing `client_secret` means the `ui_mode` did not take effect and the
    /// session is unusable to this integration.
    client_secret: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum CheckoutError {
    #[error("the Stripe HTTP client could not be constructed")]
    Client,
    #[error("the Stripe checkout session request failed")]
    Request,
    #[error("Stripe rejected the checkout session request")]
    Status,
    #[error("the Stripe checkout session response could not be parsed")]
    MalformedResponse,
}

impl From<CheckoutError> for StripeHttpError {
    fn from(_: CheckoutError) -> Self {
        Self::CheckoutFailed
    }
}

/// Create a Stripe Checkout Session over the form-encoded REST API.
///
/// Logs never include the secret key, the form body, or the raw response.
async fn create_checkout_session(
    settings: &StripeSettings,
    params: CheckoutSessionParams<'_>,
) -> Result<CheckoutSession, CheckoutError> {
    let client = checkout_client()?;
    let unit_amount = params.unit_amount_cents.to_string();
    let user_id = params.user_id.to_string();
    let credit_usd = params.credit_usd.to_string();
    let fee_usd = params.fee_usd.to_string();
    let gross_usd = params.gross_usd.to_string();
    // fee/gross ride alongside credit so the webhook can see the full price the
    // session was sold at without a database read; the corroboration still
    // RECOMPUTES gross from credit (deposit_fee_quote) rather than trusting
    // these attacker-writable fields.
    //
    // `unit_amount` remains the EX-TAX gross. `automatic_tax[enabled]` asks
    // Stripe to determine the tax from the buyer's address and the dashboard's
    // registrations and add it on top — so the card is charged more than
    // `unit_amount`, and the webhook compares against `amount_total` minus that
    // tax rather than against `amount_total`.
    //
    // `tax_id_collection[enabled]` makes the embedded form offer a VAT/tax-ID
    // field so a VAT-registered business buyer can be reverse-charged instead
    // of taxed as a consumer. See the module docs for what reverse charge does
    // and does not do; the short version is that it changes the TAX, never the
    // credit, so the webhook's ex-tax accounting is untouched by it.
    //
    // Deliberately NOT sent — each of these is a decision, not an oversight:
    //
    // - `product_data[tax_code]` and `price_data[tax_behavior]`. Omitting them
    //   is what puts the tax POLICY in Tax Settings, where the operator can
    //   revise it without a deploy; see the module docs for what to select
    //   there and why the classification is not ours to hardcode. Stripe falls
    //   back to the Tax Settings presets for exactly this reason.
    // - `tax_id_collection[required]`. Its default is `never`, which is the
    //   OPTIONAL mode and the one a self-serve product needs: the alternative,
    //   `if_supported`, makes a tax ID MANDATORY for every buyer in a supported
    //   billing country, so an EU consumer — who has no business tax ID to
    //   give — could not complete a purchase at all. That would be a checkout
    //   outage for exactly the buyers Stripe Tax was turned on for. Optional
    //   collection costs nothing: a consumer ignores the field and is taxed
    //   normally, a business fills it in and is reverse-charged.
    // - `customer_update[address]=auto` / `customer_update[name]=auto`. Both are
    //   only valid alongside a `customer`, and this session attaches none — it
    //   identifies the buyer by `customer_email` only. They exist to write the
    //   collected tax ID and legal business name BACK onto an existing Customer
    //   record; with no Customer attached there is nothing to write back to, and
    //   sending either makes Stripe reject the request. (The autopay path does
    //   keep a Stripe Customer per user, but checkout has never used it and
    //   attaching one here would change which address Checkout taxes against.)
    // - `customer_creation=always`. Tax ID collection does NOT require it.
    //   Stripe's own wording: if you configure `customer_creation` "Checkout
    //   saves any tax ID information collected during a session to that new
    //   Account or Customer. If not, the tax ID information is still available
    //   at `customer_details.tax_ids`." The tax ID reaches the completed session
    //   either way, which is the only place this integration would read it from.
    //   Setting it would create a SECOND, Checkout-owned customer on every
    //   purchase — `ensure_stripe_customer` already mints one per user for
    //   autopay — duplicating records for one human to buy nothing.
    // - `billing_address_collection=required`. Still deliberately absent, and
    //   the default `auto` is what makes the embedded form ask for an address
    //   at all: Stripe's own description of `auto` is that with `automatic_tax`
    //   enabled, "Checkout will collect the minimum number of fields required
    //   for tax calculation". Forcing `required` would collect a full address
    //   where a postal code would have done.
    //
    // `ui_mode=embedded_page` is what makes Stripe mint a `client_secret` and
    // render the form inside the portal instead of on a Stripe-hosted page.
    // It is mutually exclusive with the redirect urls: Stripe documents
    // `success_url` and `cancel_url` as "not allowed if ui_mode is
    // `embedded_page`", so both are gone and `return_url` replaces them. The
    // `{CHECKOUT_SESSION_ID}` template variable is substituted by Checkout on
    // the way back, which is how the return route knows which session to ask
    // about.
    let form: [(&str, &str); 14] = [
        ("mode", "payment"),
        ("ui_mode", CHECKOUT_UI_MODE),
        ("line_items[0][price_data][currency]", CHECKOUT_CURRENCY),
        ("line_items[0][price_data][unit_amount]", &unit_amount),
        (
            "line_items[0][price_data][product_data][name]",
            CHECKOUT_PRODUCT_NAME,
        ),
        ("line_items[0][quantity]", "1"),
        ("automatic_tax[enabled]", "true"),
        ("tax_id_collection[enabled]", "true"),
        ("metadata[user_id]", &user_id),
        ("metadata[credit_usd]", &credit_usd),
        ("metadata[fee_usd]", &fee_usd),
        ("metadata[gross_usd]", &gross_usd),
        ("customer_email", params.customer_email),
        ("return_url", &params.return_url),
    ];
    let response = client
        .post(checkout_sessions_url(settings))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                timeout = error.is_timeout(),
                "stripe checkout session request failed"
            );
            CheckoutError::Request
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            status = status.as_u16(),
            "stripe rejected the checkout session request"
        );
        return Err(CheckoutError::Status);
    }
    let session: CheckoutSessionResponse = response.json().await.map_err(|_| {
        tracing::warn!("stripe checkout session response could not be parsed");
        CheckoutError::MalformedResponse
    })?;
    let Some(client_secret) = session.client_secret else {
        tracing::warn!("stripe checkout session response is missing the client secret");
        return Err(CheckoutError::MalformedResponse);
    };
    Ok(CheckoutSession {
        id: session.id,
        client_secret,
    })
}

// ---------------------------------------------------------------------------
// POST /webhooks/stripe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StripeEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: StripeEventData,
}

#[derive(Debug, Deserialize)]
struct StripeEventData {
    object: Value,
}

async fn stripe_webhook(
    State(ctx): State<WebCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let Some(signature) = headers
        .get(STRIPE_SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::warn!("stripe webhook rejected: missing stripe-signature header");
        return Err(StripeHttpError::InvalidSignature);
    };
    // Verify BEFORE parsing: unauthenticated bytes never reach the JSON
    // parser, and the raw payload is never logged.
    if let Err(reason) = verify_webhook_signature(
        &body,
        signature,
        &stripe.webhook_secret,
        WEBHOOK_TOLERANCE,
        Utc::now().timestamp(),
    ) {
        tracing::warn!(%reason, "stripe webhook rejected: signature verification failed");
        return Err(StripeHttpError::InvalidSignature);
    }
    let event: StripeEvent = serde_json::from_slice(&body).map_err(|_| {
        tracing::warn!("stripe webhook rejected: event is not valid JSON");
        StripeHttpError::MalformedEvent
    })?;
    if event.event_type == "payment_intent.succeeded"
        || event.event_type == "payment_intent.payment_failed"
    {
        return handle_autopay_intent_event(&ctx, &event).await;
    }
    if event.event_type == DISPUTE_CREATED_EVENT || event.event_type == CHARGE_REFUNDED_EVENT {
        return handle_reversal_event(&ctx, &event).await;
    }
    if event.event_type != CHECKOUT_COMPLETED_EVENT
        && event.event_type != CHECKOUT_ASYNC_SUCCEEDED_EVENT
    {
        // Acknowledged without action so Stripe does not retry event types
        // this deployment does not consume.
        return Ok(received());
    }
    let object = &event.data.object;
    if object.get("payment_status").and_then(Value::as_str) != Some("paid") {
        // Completed but not yet paid (asynchronous payment methods): nothing
        // to credit; a later `paid` event will carry the money.
        return Ok(received());
    }
    let Some(session_id) = object.get("id").and_then(Value::as_str) else {
        tracing::warn!("stripe webhook rejected: paid session is missing its id");
        return Err(StripeHttpError::MalformedEvent);
    };
    let metadata = object.get("metadata");
    let user_id = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok());
    let credit_usd = metadata
        .and_then(|metadata| metadata.get("credit_usd"))
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<Decimal>().ok());
    let (Some(user_id), Some(credit_usd)) = (user_id, credit_usd) else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: paid session has missing or malformed metadata"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    if credit_usd <= Decimal::ZERO {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: paid session metadata carries a non-positive credit"
        );
        return Err(StripeHttpError::MalformedEvent);
    }

    // --- Layer 1: the event must corroborate itself ------------------------
    //
    // `metadata` is chosen by whoever created the session; `amount_total` and
    // `currency` are what Stripe actually collected. Requiring them to agree
    // means forged metadata on a session we did create cannot inflate the
    // credit beyond the money that moved.
    let amount_total_cents = object.get("amount_total").and_then(Value::as_i64);
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let (Some(amount_total_cents), Some(currency)) = (amount_total_cents, currency) else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: paid session is missing amount_total or currency"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    // Sales tax rides ON TOP of the price (`tax_behavior=exclusive`), so
    // `amount_total` is no longer the price ZeroRouter sold — it is the price
    // plus whatever Stripe Tax added. Strip the tax back off before comparing
    // against anything ZeroRouter quoted; see [`collected_ex_tax_cents`].
    let Some(collected) = collected_ex_tax_cents(object, amount_total_cents) else {
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            amount_total_cents,
            reported_tax = ?object.get("total_details").and_then(|details| details.get("amount_tax")),
            "stripe webhook rejected: paid session's tax breakdown does not reconcile with the \
             money collected; crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    };
    // The metadata credit is NET; `collected.ex_tax_cents` is the GROSS Stripe
    // collected for the line item. Recompute the gross the fee formula demands
    // for this credit and require Stripe to have collected exactly it.
    // Recomputing from `credit_usd` via the one fee helper — NOT trusting the
    // attacker-writable `metadata[gross_usd]` — keeps this a self-check on the
    // event: forged metadata on a session we did create cannot make the money
    // collected agree with an inflated credit. This is independent of the intent
    // row Layer 2 checks; both must hold.
    let expected_gross = deposit_fee_quote(credit_usd).gross_usd;
    let Some(expected_gross_cents) = usd_to_cents(expected_gross) else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: metadata credit prices a gross finer than a cent or out of range"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    if expected_gross_cents != collected.ex_tax_cents || currency != CHECKOUT_CURRENCY {
        // Loud and detailed: this is the shape of a credit-minting attempt,
        // not a transient fault. Everything logged here is already public to
        // whoever produced the event.
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            metadata_credit_usd = %credit_usd,
            expected_gross_cents,
            collected_ex_tax_cents = collected.ex_tax_cents,
            tax_cents = collected.tax_cents,
            amount_total_cents,
            %currency,
            expected_currency = CHECKOUT_CURRENCY,
            "stripe webhook rejected: paid session does not corroborate its own metadata; \
             crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    }

    // --- Layer 2: ZeroRouter must have priced this session ------------------
    //
    // Layer 1 still assumes every paid session in the Stripe account is ours.
    // A session created by anything else — another integration, a leaked
    // restricted key — can be internally consistent and still not be a
    // purchase this deployment sold.
    let intent = match billing::checkout_intent(&ctx.pool, session_id).await {
        Ok(intent) => intent,
        Err(_) => {
            // Retryable: the record may well exist and be unreadable right
            // now. Fail closed and let Stripe redeliver.
            tracing::warn!(
                stripe_session_id = %session_id,
                "stripe webhook deferred: pending purchase record could not be read; \
                 stripe will retry"
            );
            return Err(StripeHttpError::DatabaseUnavailable);
        }
    };
    let Some(intent) = intent else {
        // POLICY — pre-existing sessions: sessions created before migration
        // 0005 also land here, and are rejected rather than credited. Failing
        // closed is the point of the record; a "credit it anyway and warn"
        // fallback would leave the original hole open behind a log line. The
        // exposure is bounded — Checkout Sessions expire after 24h, so at most
        // one day of in-flight purchases — and each is reconcilable by hand
        // from the Stripe dashboard with an 'adjustment' ledger entry.
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            metadata_credit_usd = %credit_usd,
            amount_total_cents,
            "stripe webhook rejected: paid session has no pending purchase record; \
             crediting nothing (reconcile by hand if this predates migration 0005)"
        );
        return Err(StripeHttpError::UnknownSession);
    };
    // `expected_amount_cents` is the EX-TAX gross ZeroRouter quoted, so it is
    // compared against the ex-tax money collected, not against `amount_total`.
    if intent.user_id != user_id
        || intent.expected_amount_cents != collected.ex_tax_cents
        || intent.currency != currency
    {
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            recorded_user_id = %intent.user_id,
            collected_ex_tax_cents = collected.ex_tax_cents,
            tax_cents = collected.tax_cents,
            amount_total_cents,
            recorded_amount_cents = intent.expected_amount_cents,
            %currency,
            recorded_currency = %intent.currency,
            "stripe webhook rejected: paid session contradicts its pending purchase record; \
             crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    }

    // Both the recipient and the dollars come from ZeroRouter's own record.
    // The metadata has now been checked against it and is not used again.
    let user_id = intent.user_id;
    let credit_usd = intent.expected_credit_usd;
    let payment_intent = object.get("payment_intent").and_then(Value::as_str);
    match billing::credit_purchase(&ctx.pool, user_id, credit_usd, session_id, payment_intent).await
    {
        Ok(outcome) => {
            if matches!(outcome, CreditOutcome::AlreadyApplied) {
                tracing::info!(
                    stripe_session_id = %session_id,
                    "stripe webhook replayed: purchase already applied"
                );
            } else {
                tracing::info!(
                    stripe_session_id = %session_id,
                    user_id = %user_id,
                    amount_usd = %credit_usd,
                    "applied stripe purchase credit"
                );
            }
            // Stamped only after the credit has committed, and deliberately
            // not fatal: idempotence belongs to the unique index on
            // `credit_ledger.stripe_session_id`, so a lost marker costs a
            // reconciliation query, while stamping first would risk a
            // settled-but-uncredited session on a retry.
            if let Err(error) = billing::settle_checkout_intent(&ctx.pool, session_id).await {
                tracing::warn!(
                    stripe_session_id = %session_id,
                    %error,
                    "stripe purchase credited but its pending record could not be marked settled"
                );
            }
            Ok(received())
        }
        Err(error) if is_foreign_key_violation(&error) => {
            tracing::warn!(
                stripe_session_id = %session_id,
                "stripe webhook rejected: metadata references an unknown user"
            );
            Err(StripeHttpError::UnknownUser)
        }
        Err(_) => {
            tracing::warn!(
                stripe_session_id = %session_id,
                "stripe webhook credit application failed; stripe will retry"
            );
            Err(StripeHttpError::DatabaseUnavailable)
        }
    }
}

fn received() -> Json<Value> {
    Json(serde_json::json!({ "received": true }))
}

/// What a paid Checkout Session actually collected, split into the price
/// ZeroRouter sold and the sales tax Stripe added to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollectedAmounts {
    /// The money collected for the line item itself — the figure every
    /// ZeroRouter-side amount is compared against.
    ex_tax_cents: i64,
    /// Sales tax, collected on ZeroRouter's behalf and owed to a taxing
    /// jurisdiction. Never revenue, never credit; carried only so it can be
    /// logged.
    tax_cents: i64,
}

/// Take the sales tax back off what Stripe collected.
///
/// # Why this exists
///
/// Before Stripe Tax, `amount_total` WAS the price: the session collected
/// exactly the gross ZeroRouter quoted, so the two could be compared directly.
/// With `automatic_tax` and `tax_behavior=exclusive` the card is charged
/// `gross + tax`, and every ZeroRouter-side figure — the fee formula's
/// recomputed gross, the `stripe_checkout_intents.expected_amount_cents` quote
/// — is still ex-tax. Comparing them against `amount_total` would reject every
/// taxed purchase, taking the customer's money and crediting nothing.
///
/// # Why it subtracts rather than reading `amount_subtotal`
///
/// Stripe also reports `amount_subtotal`, the line-item total BEFORE discounts
/// and taxes. Using it would be wrong in the direction that costs money: a
/// session carrying a coupon has `amount_subtotal` above what the customer
/// actually paid, so ZeroRouter would credit against money it never received.
/// `amount_total - amount_tax` is by construction "the money that moved, less
/// the part that is not ours", which is exactly the quantity the corroborations
/// need. Anything that makes the two differ — a discount, a shipping line, tax
/// carved out of the price by an accidental `tax_behavior=inclusive` — lands as
/// a short payment and is refused by the caller's equality check, which is the
/// correct outcome for all three.
///
/// # Fail-closed reading
///
/// `total_details` absent, or present without `amount_tax`, reads as zero tax:
/// that is the pre-Stripe-Tax shape and the conservative direction, since a
/// session that really did collect tax then fails the caller's equality check
/// rather than passing it. Anything present but unusable — a negative tax, a
/// tax that is not an integer number of cents, a subtraction that would
/// overflow — returns `None` and credits nothing.
fn collected_ex_tax_cents(object: &Value, amount_total_cents: i64) -> Option<CollectedAmounts> {
    let tax_cents = match object
        .get("total_details")
        .and_then(|details| details.get("amount_tax"))
    {
        None | Some(Value::Null) => 0,
        Some(reported) => reported.as_i64().filter(|cents| *cents >= 0)?,
    };
    Some(CollectedAmounts {
        ex_tax_cents: amount_total_cents.checked_sub(tax_cents)?,
        tax_cents,
    })
}

// ---------------------------------------------------------------------------
// Refunds and chargebacks (migration 0009)
// ---------------------------------------------------------------------------

/// The `charge.refunded` / `charge.dispute.created` arm: take the credit back,
/// and — for a dispute only — freeze the account.
///
/// # Why a dispute freezes and a refund does not
///
/// A refund is ZeroRouter or Stripe support giving money back deliberately;
/// taking the credit with it is the whole correction. A dispute is a customer
/// telling their bank the charge was not legitimate. The money is already gone
/// from the ZeroRouter balance, the customer may have consumed the inference it
/// bought, and nothing about the account can be trusted until a human looks —
/// so the account stops spending. Its history stays readable: the freeze blocks
/// spend, not visibility.
///
/// # What this trusts
///
/// Only Stripe's own fields, and only after the HMAC has been verified. The
/// charge/dispute is mapped back to a user through its `payment_intent` — a
/// Stripe-generated id — joined against ZeroRouter's OWN ledger
/// ([`billing::credited_purchase`]). `metadata` is never read here, so the
/// co-tenant problem the checkout and autopay arms have to defend against
/// (anyone able to create objects in the Stripe account can write metadata)
/// does not arise: an event naming an intent this deployment never credited
/// matches nothing and moves nothing.
///
/// # What it deliberately does not do
///
/// Reverse a PARTIAL refund or a partial dispute. The reversal takes back
/// exactly what was credited, so it only runs when the reversed amount covers
/// that credit in full; anything less is logged for an operator instead of
/// guessed at. A dispute still freezes in that case, which is the half that
/// cannot wait. Partial refunds of prepaid credit are not something this
/// deployment issues today, and inventing an apportioning rule for money
/// without an operator asking for one is exactly the kind of quiet decision
/// this module exists to avoid.
async fn handle_reversal_event(
    ctx: &WebCtx,
    event: &StripeEvent,
) -> Result<Json<Value>, StripeHttpError> {
    let object = &event.data.object;
    let is_dispute = event.event_type == DISPUTE_CREATED_EVENT;

    // The Stripe object this reversal is anchored to: the dispute id for a
    // chargeback, the charge id for a refund. It becomes the reversal's
    // `credit_ledger.stripe_session_id`, so a redelivery deduplicates against
    // the same unique index a replayed purchase does.
    let Some(object_id) = object.get("id").and_then(Value::as_str) else {
        tracing::warn!(
            event_type = %event.event_type,
            "stripe webhook rejected: reversal event is missing its object id"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    // Present on both shapes: a Dispute carries the intent it disputes, a
    // Charge the intent that created it.
    let payment_intent = object.get("payment_intent").and_then(Value::as_str);
    let Some(payment_intent) = payment_intent else {
        // Unattributable. Acknowledged so Stripe stops retrying something no
        // redelivery can fix, but logged at error level: if this is ours, a
        // human has to reconcile it by hand.
        tracing::error!(
            event_type = %event.event_type,
            stripe_object_id = %object_id,
            "stripe reversal event names no payment intent; it cannot be attributed to a user \
             and nothing was reversed or frozen — reconcile by hand if this charge is ours"
        );
        return Ok(received());
    };

    // The reversed amount and its currency, read once and used both for the
    // tombstone (when no credit exists yet) and the coverage check (when one
    // does). For a dispute the disputed `amount` is the money withdrawn; for a
    // refund `amount_refunded` is the cumulative refunded total on the charge.
    let reversed_cents = if is_dispute {
        object.get("amount").and_then(Value::as_i64)
    } else {
        object.get("amount_refunded").and_then(Value::as_i64)
    };
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);

    // FIX A (HIGH-1 round 2): resolve the reversal against the credit ledger
    // UNDER the PaymentIntent lock, so the lookup and — when no credit exists
    // yet — the tombstone write are ONE atomic step a racing credit for the same
    // intent cannot interleave with. Previously these were two autocommit
    // statements (`credited_purchase` then `record_observed_reversal`), and a
    // credit committing between them was seen by neither: refunded money stayed
    // spendable and a dispute stayed unfrozen.
    //
    // A reversal for a charge this deployment has NOT yet credited is no longer
    // acknowledged-and-forgotten. It may be a foreign charge (another
    // integration in the same Stripe account), OR our own credit that simply has
    // not landed yet — Stripe delivers events out of order, and the credit path
    // returns 503 and leans on redelivery, so there are real windows where the
    // purchase/autopay row does not exist yet. We cannot tell the two apart here
    // (with no credit row there is no user to freeze), so the reversal is durably
    // recorded keyed on its object id. When a credit for this intent lands,
    // `credit_purchase` / `settle_autopay_intent` — taking the SAME intent lock —
    // consume the tombstone and converge to the reversed (and, for a dispute,
    // frozen) end state; a foreign charge never gets a credit, so its tombstone
    // stays inert. Still HTTP 200 — the reversal is now durable, so Stripe need
    // not retry.
    let Some(credited) = billing::resolve_reversal_against_credit(
        &ctx.pool,
        object_id,
        payment_intent,
        is_dispute,
        reversed_cents,
        currency.as_deref(),
    )
    .await
    .map_err(|error| {
        tracing::warn!(
            event_type = %event.event_type,
            stripe_object_id = %object_id,
            %error,
            "stripe reversal deferred: the credit ledger could not be read or the tombstone written; stripe will retry"
        );
        StripeHttpError::DatabaseUnavailable
    })?
    else {
        tracing::info!(
            event_type = %event.event_type,
            stripe_object_id = %object_id,
            payment_intent = %payment_intent,
            is_dispute,
            "stripe reversal observed before any matching credit; recorded, to be applied if the credit lands"
        );
        return Ok(received());
    };

    // Freeze FIRST, and independently of whether the reversal can be computed:
    // the account must stop spending even when the money question needs a
    // human. Idempotent, so a redelivered dispute does not restamp it.
    if is_dispute {
        let froze = billing::freeze_account(
            &ctx.pool,
            credited.user_id,
            billing::FreezeReason::Dispute,
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                stripe_object_id = %object_id,
                %error,
                "stripe dispute deferred: the account could not be frozen; stripe will retry"
            );
            StripeHttpError::DatabaseUnavailable
        })?;
        tracing::error!(
            user_id = %credited.user_id,
            stripe_dispute_id = %object_id,
            payment_intent = %payment_intent,
            credited_usd = %credited.amount_usd,
            newly_frozen = froze,
            "stripe chargeback: account frozen"
        );
    }

    // Only a reversal that covers the whole credit is applied automatically
    // (`reversed_cents` and `currency` were read above, before the credit
    // lookup, so the tombstone and this check see identical values).
    let credited_cents = usd_to_cents(credited.amount_usd);
    let covers_the_credit = match (reversed_cents, currency.as_deref(), credited_cents) {
        (Some(reversed), Some(currency), Some(credited_cents)) => {
            // Currency is checked independently of the amount for the same
            // reason the checkout arm checks it: the smallest unit of a
            // zero-decimal currency numerically matches a cents amount while
            // being worth a fraction of it.
            currency == CHECKOUT_CURRENCY && reversed >= credited_cents
        }
        _ => false,
    };
    if !covers_the_credit {
        tracing::error!(
            event_type = %event.event_type,
            user_id = %credited.user_id,
            stripe_object_id = %object_id,
            payment_intent = %payment_intent,
            credited_usd = %credited.amount_usd,
            ?reversed_cents,
            ?currency,
            frozen = is_dispute,
            "stripe reversal does not cover the full credit (partial, foreign-currency, or \
             missing amount); NOTHING was reversed — an operator must reconcile this by hand"
        );
        return Ok(received());
    }

    let note = if is_dispute {
        format!("chargeback reversal ({object_id})")
    } else {
        format!("refund reversal ({object_id})")
    };
    let outcome = billing::reverse_purchase(&ctx.pool, payment_intent, object_id, &note)
        .await
        .map_err(|error| {
            tracing::warn!(
                stripe_object_id = %object_id,
                %error,
                "stripe reversal failed to apply; stripe will retry"
            );
            StripeHttpError::DatabaseUnavailable
        })?;
    // FIX 2' (round 4): did the reversal actually land? Only then may we discharge
    // a stale tombstone (below). `matches!` reads `outcome` without moving it, so
    // the `match` that logs it still owns it.
    let reversal_landed = matches!(
        outcome,
        billing::ReversalOutcome::Reversed { .. } | billing::ReversalOutcome::AlreadyReversed
    );
    match outcome {
        billing::ReversalOutcome::Reversed {
            amount_usd,
            balance_after,
        } => tracing::warn!(
            event_type = %event.event_type,
            user_id = %credited.user_id,
            stripe_object_id = %object_id,
            reversed_usd = %amount_usd,
            balance_after = %balance_after,
            // A negative balance is not an error state: it IS the receivable,
            // and saying so here is what makes it findable later.
            receivable = balance_after < Decimal::ZERO,
            "reversed a stripe credit"
        ),
        billing::ReversalOutcome::AlreadyReversed => tracing::info!(
            stripe_object_id = %object_id,
            "stripe reversal replayed: this purchase was already reversed"
        ),
        // Unreachable in practice: the credit was just read above. Logged
        // rather than errored, because a retry cannot improve it.
        billing::ReversalOutcome::UnknownPurchase => tracing::error!(
            stripe_object_id = %object_id,
            payment_intent = %payment_intent,
            "stripe reversal found no credit for an intent that had one moments earlier"
        ),
    }
    // FIX 2' (round 4): the reversal has now actually landed (and, for a dispute,
    // the freeze above committed), so it is finally safe to stamp any stale
    // non-covering tombstone for this intent applied. This runs ONLY on a
    // successful reversal — a `reverse_purchase` that failed returned early via
    // `?` above and never reaches here, so its tombstone stays unapplied and
    // operator-visible. Stamping is per-intent idempotent and moves no money.
    if reversal_landed {
        billing::mark_intent_reversals_applied(&ctx.pool, payment_intent)
            .await
            .map_err(|error| {
                tracing::warn!(
                    stripe_object_id = %object_id,
                    %error,
                    "stripe reversal applied but stamping its stale tombstone failed; stripe will retry"
                );
                StripeHttpError::DatabaseUnavailable
            })?;
    }
    Ok(received())
}

fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .is_some_and(|code| code == PG_FOREIGN_KEY_VIOLATION)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> StripeSettings {
        StripeSettings {
            secret_key: "sk_test_unused".to_owned(),
            publishable_key: "pk_test_unused".to_owned(),
            webhook_secret: "whsec_unused".to_owned(),
            checkout_min_usd: Decimal::from(5),
            checkout_max_usd: Decimal::from(1000),
            api_base: "https://api.stripe.com".to_owned(),
        }
    }

    fn decimal(raw: &str) -> Decimal {
        raw.parse().expect("test literal must parse")
    }

    #[test]
    fn checkout_amounts_convert_to_integer_cents() {
        let settings = settings();
        assert_eq!(
            validate_checkout_amount(decimal("25.00"), &settings).ok(),
            Some(2500)
        );
        assert_eq!(
            validate_checkout_amount(decimal("25"), &settings).ok(),
            Some(2500)
        );
        assert_eq!(
            validate_checkout_amount(decimal("5.01"), &settings).ok(),
            Some(501)
        );
        assert_eq!(
            validate_checkout_amount(decimal("1000"), &settings).ok(),
            Some(100_000)
        );
        // Trailing zeros beyond two places still describe a whole cent.
        assert_eq!(
            validate_checkout_amount(decimal("25.1000"), &settings).ok(),
            Some(2510)
        );
    }

    #[test]
    fn checkout_amounts_out_of_policy_are_rejected() {
        let settings = settings();
        for raw in ["4.99", "1000.01", "25.001", "0", "-25.00"] {
            assert!(
                validate_checkout_amount(decimal(raw), &settings).is_err(),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn cents_conversion_is_exact_in_both_directions() {
        // The quote and the webhook's amount check share this function, so a
        // value that can be quoted must verify against the amount Stripe then
        // reports, and nothing finer than a cent survives either way.
        let settings = settings();
        for raw in ["5", "25.00", "25.1000", "999.99", "1000"] {
            let amount = decimal(raw);
            assert_eq!(
                validate_checkout_amount(amount, &settings).ok(),
                usd_to_cents(amount),
                "{raw} must quote and verify to the same cents"
            );
        }
        assert_eq!(usd_to_cents(decimal("0.01")), Some(1));
        assert_eq!(usd_to_cents(decimal("1000")), Some(100_000));
        // Sub-cent amounts cannot be represented as an `amount_total`, so a
        // metadata credit claiming one can never corroborate a real payment.
        assert_eq!(usd_to_cents(decimal("25.001")), None);
        assert_eq!(usd_to_cents(decimal("0.005")), None);
    }

    #[test]
    fn deposit_fee_is_five_point_five_percent_above_the_floor() {
        // $100: 0.055 * 100 = 5.50 exactly, well above the $0.80 floor.
        let quote = deposit_fee_quote(decimal("100"));
        assert_eq!(quote.fee_usd, decimal("5.50"));
        assert_eq!(quote.gross_usd, decimal("105.50"));
        assert_eq!(quote.credit_usd, decimal("100"));
    }

    #[test]
    fn deposit_fee_ceils_to_the_whole_cent() {
        // $25: 0.055 * 25 = 1.375 — a sub-cent fee ZeroRouter must never round
        // DOWN. It is ceiled to $1.38, so the gross is a whole $26.38.
        let quote = deposit_fee_quote(decimal("25"));
        assert_eq!(quote.fee_usd, decimal("1.38"));
        assert_eq!(quote.gross_usd, decimal("26.38"));
        // Any half-cent product ceils up rather than banker-rounds: $45 gives
        // 0.055 * 45 = 2.475 -> 2.48.
        assert_eq!(deposit_fee_quote(decimal("45")).fee_usd, decimal("2.48"));
    }

    #[test]
    fn deposit_fee_floor_covers_the_smallest_deposit() {
        // $5 is the smallest allowed deposit. 0.055 * 5 = 0.275 -> ceils to
        // $0.28, which would not clear Stripe's fixed $0.30, so the $0.80 floor
        // takes over and the user pays $5.80 gross.
        let quote = deposit_fee_quote(decimal("5"));
        assert_eq!(quote.fee_usd, decimal("0.80"));
        assert_eq!(quote.gross_usd, decimal("5.80"));
    }

    #[test]
    fn deposit_fee_crossover_from_floor_to_percentage() {
        // The floor wins until the percentage fee reaches $0.80, at
        // credit = 0.80 / 0.055 = 14.5454...  At $14.54 the percentage is
        // 0.055 * 14.54 = 0.7997 -> ceils to 0.80, tying the floor. At $14.55
        // it is 0.800250 -> ceils to 0.81 and overtakes the floor.
        assert_eq!(deposit_fee_quote(decimal("14.54")).fee_usd, decimal("0.80"));
        assert_eq!(deposit_fee_quote(decimal("14.55")).fee_usd, decimal("0.81"));
    }

    #[test]
    fn deposit_fee_gross_is_always_whole_cents() {
        // The charge path feeds gross straight into usd_to_cents, which rejects
        // anything finer than a cent. Every credit that is itself whole cents
        // must therefore price a whole-cent gross.
        for raw in ["5", "5.01", "14.54", "14.55", "25", "99.99", "100", "1000"] {
            let quote = deposit_fee_quote(decimal(raw));
            assert!(
                usd_to_cents(quote.gross_usd).is_some(),
                "gross for {raw} ({}) must be whole cents",
                quote.gross_usd
            );
            assert_eq!(
                quote.gross_usd,
                quote.credit_usd + quote.fee_usd,
                "gross is exactly credit + fee for {raw}"
            );
        }
    }

    fn session_with_tax(reported_tax: Value) -> Value {
        serde_json::json!({ "total_details": { "amount_tax": reported_tax } })
    }

    #[test]
    fn tax_is_taken_back_off_what_stripe_collected() {
        // No tax reported at all — every session created before Stripe Tax was
        // enabled — reads as the whole amount being price.
        assert_eq!(
            collected_ex_tax_cents(&serde_json::json!({}), 2_638),
            Some(CollectedAmounts {
                ex_tax_cents: 2_638,
                tax_cents: 0
            })
        );
        // `total_details` present but silent about tax reads the same way.
        assert_eq!(
            collected_ex_tax_cents(&serde_json::json!({ "total_details": {} }), 2_638),
            Some(CollectedAmounts {
                ex_tax_cents: 2_638,
                tax_cents: 0
            })
        );
        // Exclusive tax: the card was charged $28.03 for a $26.38 price.
        assert_eq!(
            collected_ex_tax_cents(&session_with_tax(serde_json::json!(165)), 2_803),
            Some(CollectedAmounts {
                ex_tax_cents: 2_638,
                tax_cents: 165
            })
        );
        // A zero tax line is not the same as no tax line, but it reads the same.
        assert_eq!(
            collected_ex_tax_cents(&session_with_tax(serde_json::json!(0)), 2_638),
            Some(CollectedAmounts {
                ex_tax_cents: 2_638,
                tax_cents: 0
            })
        );
    }

    #[test]
    fn an_unusable_tax_figure_yields_nothing_to_compare() {
        // Every one of these would otherwise be read as some number of cents
        // and silently change what counts as a matching payment.
        for reported in [
            serde_json::json!(-1),
            serde_json::json!("165"),
            serde_json::json!(165.5),
            serde_json::json!(true),
            serde_json::json!([165]),
        ] {
            assert_eq!(
                collected_ex_tax_cents(&session_with_tax(reported.clone()), 2_803),
                None,
                "{reported} must not be readable as tax"
            );
        }
        // A tax larger than the total, or a subtraction that would wrap: the
        // event is incoherent, so there is nothing to corroborate against.
        assert_eq!(
            collected_ex_tax_cents(&session_with_tax(serde_json::json!(i64::MAX)), i64::MIN),
            None
        );
        // A tax exceeding the total is arithmetically fine but describes money
        // that cannot have moved; it survives here and is refused by the
        // caller's equality check against the quoted gross.
        assert_eq!(
            collected_ex_tax_cents(&session_with_tax(serde_json::json!(5_000)), 2_638),
            Some(CollectedAmounts {
                ex_tax_cents: -2_362,
                tax_cents: 5_000
            })
        );
    }

    #[test]
    fn signature_headers_parse_strictly() {
        // Candidates must be the length of a hex SHA-256 digest; anything
        // else cannot match, so it is dropped rather than decoded.
        let first = "a".repeat(SIGNATURE_HEX_LEN);
        let second = "c".repeat(SIGNATURE_HEX_LEN);
        let header = format!("t=1700000000,v1={first},v0=bb,v1={second},v1=tooshort");
        let parsed = parse_signature_header(&header).expect("well-formed header must parse");
        assert_eq!(parsed.timestamp, 1_700_000_000);
        assert_eq!(parsed.timestamp_raw, "1700000000");
        assert_eq!(
            parsed.candidates,
            vec![first.as_str(), second.as_str()],
            "v0 and malformed-length candidates are ignored"
        );

        // An unbounded candidate list is a work amplifier: the endpoint is
        // public, and every extra candidate used to mean another full HMAC
        // over the whole body. Both the count and the per-candidate cost are
        // now capped (the digest is computed once).
        let flood = std::iter::repeat_n(format!("v1={first}"), 5_000)
            .collect::<Vec<_>>()
            .join(",");
        let flooded_header = format!("t=1700000000,{flood}");
        let parsed =
            parse_signature_header(&flooded_header).expect("a flooded header still parses");
        assert_eq!(parsed.candidates.len(), MAX_SIGNATURE_CANDIDATES);

        let valid = "a".repeat(SIGNATURE_HEX_LEN);
        for header in [
            String::new(),
            "garbage".to_owned(),
            format!("t=notanumber,v1={valid}"),
            format!("v1={valid}"),
            "t=1700000000".to_owned(),
            // Present but unusable: every candidate is the wrong length.
            "t=1700000000,v1=aa,v1=bb".to_owned(),
        ] {
            assert_eq!(
                parse_signature_header(&header).err(),
                Some(WebhookVerifyError::MalformedHeader),
                "{header:?} should be malformed"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Autopay tax location (migration 0021)
    // -----------------------------------------------------------------------

    fn card_with(address: serde_json::Value) -> Value {
        serde_json::json!({ "id": "pm_test", "billing_details": { "address": address } })
    }

    #[test]
    fn a_complete_billing_address_locates_the_buyer_for_tax() {
        let card = card_with(serde_json::json!({
            "line1": "1 Broadway",
            "city": "Cambridge",
            "state": "MA",
            "postal_code": "02142",
            "country": "US",
        }));
        assert_eq!(
            autopay_tax_address(&card),
            Ok(TaxAddress {
                country: "US".to_owned(),
                postal_code: Some("02142".to_owned()),
                state: Some("MA".to_owned()),
                city: Some("Cambridge".to_owned()),
                line1: Some("1 Broadway".to_owned()),
            })
        );
    }

    #[test]
    fn a_non_us_buyer_needs_only_a_country() {
        // Stripe rates most countries from the country code alone; only the US
        // (and Canada, via postal code or province) needs more. Demanding a
        // postal code everywhere would push EU buyers onto the untaxed
        // fallback for no reason.
        let card = card_with(serde_json::json!({ "country": "IE" }));
        assert_eq!(
            autopay_tax_address(&card).map(|address| address.country),
            Ok("IE".to_owned())
        );
    }

    #[test]
    fn a_card_without_an_address_falls_back_rather_than_inventing_one() {
        // Stripe only captures billing details on the setup session if the
        // account is configured to, so this is the ordinary state of a card
        // saved before that was switched on — not an error.
        for card in [
            serde_json::json!({ "id": "pm_test" }),
            serde_json::json!({ "id": "pm_test", "billing_details": {} }),
            serde_json::json!({ "id": "pm_test", "billing_details": { "address": null } }),
            // Present but not an object: refused rather than coerced.
            serde_json::json!({ "id": "pm_test", "billing_details": { "address": "MA" } }),
        ] {
            assert_eq!(
                autopay_tax_address(&card),
                Err(TaxFallback::NoBillingAddress),
                "{card} should yield no billing address"
            );
        }
    }

    #[test]
    fn an_address_too_thin_to_rate_falls_back_before_calling_stripe() {
        for address in [
            // No country at all: the API requires one.
            serde_json::json!({ "postal_code": "02142" }),
            serde_json::json!({ "country": null, "postal_code": "02142" }),
            // Blank and whitespace-only are absent, not a location.
            serde_json::json!({ "country": "", "postal_code": "02142" }),
            serde_json::json!({ "country": "   ", "postal_code": "02142" }),
            // US without a postal code. Stripe cannot rate a US buyer from a
            // country code alone, NOR from country plus state, so this would be
            // a guaranteed round trip and error.
            serde_json::json!({ "country": "US" }),
            serde_json::json!({ "country": "US", "state": "MA" }),
            serde_json::json!({ "country": "US", "postal_code": "  " }),
            // Case must not smuggle a US address past the postal-code rule.
            serde_json::json!({ "country": "us", "state": "MA" }),
        ] {
            assert_eq!(
                autopay_tax_address(&card_with(address.clone())),
                Err(TaxFallback::IncompleteAddress),
                "{address} should be incomplete"
            );
        }
    }

    #[test]
    fn fallback_reasons_have_stable_log_values() {
        // These strings are what an operator alerts on ("is autopay silently
        // charging untaxed?"), so they are part of the contract, not prose.
        assert_eq!(TaxFallback::NoBillingAddress.as_str(), "no_billing_address");
        assert_eq!(
            TaxFallback::IncompleteAddress.as_str(),
            "incomplete_address"
        );
        assert_eq!(
            TaxFallback::CalculationRejected.as_str(),
            "calculation_rejected"
        );
        assert_eq!(
            TaxFallback::CalculationUnavailable.as_str(),
            "calculation_unavailable"
        );
        assert_eq!(TAX_FALLBACK_FIELD, "autopay_tax_fallback");
    }

    #[test]
    fn the_tax_metadata_keys_are_the_pinned_wire_contract() {
        // The form parameter and the key the webhook reads back must stay in
        // lockstep: they are written by one binary and read by another during
        // a rollout, and a rename that touched only one side would make every
        // taxed charge look like a short payment and credit nothing.
        assert_eq!(
            AUTOPAY_TAX_CENTS_PARAM,
            format!("metadata[{AUTOPAY_TAX_CENTS_KEY}]")
        );
        assert_eq!(
            AUTOPAY_TAX_CALCULATION_PARAM,
            format!("metadata[{AUTOPAY_TAX_CALCULATION_KEY}]")
        );
    }
}

// ---------------------------------------------------------------------------
// Autopay (migration 0008): saved-card auto-recharge.
//
// AUTOPAY IS TAXED (migration 0021), through the Tax Calculation API rather
// than `automatic_tax`. See [`calculate_autopay_tax`] for the mechanism and
// [`autopay_tax_address`] for where the buyer's location comes from.
//
// It remains true that `POST /v1/payment_intents` has no `automatic_tax`
// parameter — that was checked against the endpoint's full parameter list, not
// assumed. What is NOT true, and was the operative claim here before, is that
// this closes off the subject. Two other routes exist, and the one chosen
// matters enough to record:
//
//   1. `hooks[inputs][tax][calculation]` on the PaymentIntent (Stripe's
//      "simplified" Tax API integration, GA in `2025-11-17.clover`). Stripe
//      then creates the tax transaction on success AND reverses it on refund,
//      with no extra call from us. Genuinely nicer bookkeeping — and REJECTED,
//      because it puts a version-gated parameter on the money path. This
//      account's default API version is demonstrably older than
//      `2026-03-25.dahlia` (that is why [`CHECKOUT_API_VERSION`] exists at
//      all), so whether it accepts `hooks` is unknown, and an unknown-parameter
//      rejection would fail the CHARGE, not merely the tax. Reaching the
//      capability would mean pinning the PaymentIntent request to a non-default
//      API version — moving the money path onto a new API train as a side
//      effect of a tax change, which is exactly what pinning was declined for
//      on this client. A top-up must never fail because tax could not be
//      computed; making the charge itself depend on a tax feature inverts that.
//
//   2. Invoices with `automatic_tax`. Rejected for three independent reasons.
//      It replaces one POST-under-one-idempotency-key with a multi-call
//      orchestration (invoice item, invoice, finalize), and the entire
//      exactly-once story here — the `local_<key>` claim, the replay, the
//      20-hour window — is built on there being exactly one request to replay.
//      Stripe's own dunning would then retry a failed invoice on ITS schedule,
//      outside [`billing::AUTOPAY_ELIGIBILITY_PREDICATE`], which is an
//      uncontrolled second charge path aimed straight at the invariant that a
//      frozen or indebted account is never charged. And it would compute zero
//      tax for everyone anyway until a Customer write is added, because Stripe
//      resolves an invoice's location from the Customer's shipping or billing
//      address or a DEFAULT payment method, none of which this deployment sets.
//
// So: the Tax Calculation API, an unchanged PaymentIntent request shape, and an
// explicit tax transaction afterwards. Every version-gated parameter stays off
// the money path; the charge differs from the untaxed one only in `amount`.
//
// The ledger invariant is unchanged and is the reason the accounting below
// barely moves: `amount_usd` is still the NET credit, `charge_amount_usd` is
// still the EX-TAX gross, and tax is a third quantity in its own column that is
// neither credited nor counted as revenue. What DID have to change is the
// corroboration — `amount_received` is no longer the gross — and it changes the
// same way checkout's did, by subtracting the tax back off before comparing.
// ---------------------------------------------------------------------------

const AUTOPAY_PURPOSE: &str = "zerorouter_autopay";
/// The `line_items[0][reference]` on an autopay tax calculation. Stripe only
/// requires it to be present; it surfaces as the line's label in the Tax
/// Transactions view, so it says what was sold rather than repeating an id.
const AUTOPAY_TAX_LINE_REFERENCE: &str = "ZeroRouter credits (autopay)";
/// Metadata key carrying the tax collected on top of the ex-tax gross, in
/// cents. The webhook subtracts it from `amount_received` to recover the
/// ex-tax figure its corroboration is denominated in — the autopay twin of
/// [`collected_ex_tax_cents`].
const AUTOPAY_TAX_CENTS_KEY: &str = "tax_cents";
/// Metadata key carrying the Tax Calculation id, so the webhook can record the
/// tax transaction without a database round trip.
const AUTOPAY_TAX_CALCULATION_KEY: &str = "tax_calculation";
/// The same two keys as form parameters. Spelled out rather than built with
/// `format!` per charge: the form takes `&str`, and allocating (or worse,
/// leaking) a constant string on every off-session charge would be a slow leak
/// on the one path that runs unattended in a loop.
const AUTOPAY_TAX_CENTS_PARAM: &str = "metadata[tax_cents]";
const AUTOPAY_TAX_CALCULATION_PARAM: &str = "metadata[tax_calculation]";
const AUTOPAY_SWEEP_BATCH: i64 = 16;
/// Pending intents older than this are reconciled against Stripe directly.
const AUTOPAY_RECONCILE_AFTER_MINUTES: i32 = 30;
/// Oldest claim the sweep will replay. Stripe caches an idempotency key's
/// result for at least 24 hours and may prune it afterwards; a "replay"
/// past that window is a new request to Stripe, which means a second
/// charge. Twenty hours keeps a margin inside the guarantee (sol review).
const AUTOPAY_REPLAY_MAX_AGE_MINUTES: i32 = 20 * 60;

/// Provenance mark carried in PaymentIntent metadata: an HMAC over the
/// money-bearing fields, keyed by the webhook secret. The webhook's
/// metadata-recovery path only trusts events that carry it, so another
/// integration in the same Stripe account writing our metadata SHAPE
/// cannot mint credits — it does not hold the key (review finding).
/// Length-guarded constant-time comparison, same shape as
/// `auth::constant_time_eq`: XOR-fold every byte so a mismatch position is
/// not observable through timing.
fn constant_time_str_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (l, r)| acc | (l ^ r))
        == 0
}

fn autopay_provenance(settings: &StripeSettings, user_id: Uuid, credit_usd: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(settings.webhook_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(format!("{AUTOPAY_PURPOSE}|{user_id}|{credit_usd}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Ensure the user has a Stripe Customer, creating one on first use. The
/// stored id wins any race: a concurrent creation that loses the UPDATE
/// leaves an orphan customer at Stripe, which is inert.
async fn ensure_stripe_customer(
    ctx: &WebCtx,
    settings: &StripeSettings,
    user_id: Uuid,
    email: &str,
) -> Result<String, StripeHttpError> {
    if let Some(existing) = sqlx::query_scalar::<_, Option<String>>(
        "SELECT stripe_customer_id FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?
        && !existing.is_empty()
    {
        return Ok(existing);
    }
    let client = stripe_client()?;
    let user_id_text = user_id.to_string();
    let form: [(&str, &str); 2] = [("email", email), ("metadata[user_id]", &user_id_text)];
    let response = client
        .post(format!("{}/v1/customers", settings.api_base))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "stripe customer creation rejected"
        );
        return Err(StripeHttpError::CheckoutFailed);
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    let Some(customer_id) = body.get("id").and_then(Value::as_str) else {
        return Err(StripeHttpError::CheckoutFailed);
    };
    sqlx::query(
        "UPDATE users SET stripe_customer_id = $2 WHERE id = $1 AND stripe_customer_id IS NULL",
    )
    .bind(user_id)
    .bind(customer_id)
    .execute(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    let stored = sqlx::query_scalar::<_, Option<String>>(
        "SELECT stripe_customer_id FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    Ok(stored.unwrap_or_else(|| customer_id.to_owned()))
}

fn stripe_client() -> Result<reqwest::Client, StripeHttpError> {
    reqwest::Client::builder()
        .timeout(STRIPE_HTTP_TIMEOUT)
        .build()
        .map_err(|_| StripeHttpError::CheckoutFailed)
}

// ---------------------------------------------------------------------------
// Autopay sales tax (migration 0021)
// ---------------------------------------------------------------------------

/// The one structured log field every untaxed-fallback site sets, so "how often
/// is autopay charging untaxed, and why?" is one query rather than a grep for
/// prose. The value is one of the [`TaxFallback`] reasons.
const TAX_FALLBACK_FIELD: &str = "autopay_tax_fallback";

/// Why an autopay charge went out without tax. Every variant is a DEGRADED BUT
/// SUCCESSFUL top-up: the charge still happens, for the untaxed gross, exactly
/// as it did before migration 0021. A dead top-up is worse than an untaxed one
/// — the customer's inference stops — so nothing in the tax path is allowed to
/// fail the charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaxFallback {
    /// The saved card carries no billing address at all. Stripe collects
    /// billing details on the setup Checkout Session only if the account's
    /// settings ask for them, so this is the expected state for cards saved
    /// before that was configured.
    NoBillingAddress,
    /// An address is present but cannot locate a buyer for tax: no country, or
    /// a US address with no postal code (Stripe cannot rate a US buyer from a
    /// country code alone, nor from country plus state).
    IncompleteAddress,
    /// Stripe understood the request and refused to price it — in practice
    /// `customer_tax_location_invalid`, an address that looks complete but does
    /// not resolve.
    CalculationRejected,
    /// The Tax API could not be reached, or answered in a shape this code
    /// cannot read. Distinct from `CalculationRejected` because it is an
    /// availability problem, not a data problem: the same buyer will probably
    /// be taxed correctly on the next sweep.
    CalculationUnavailable,
}

impl TaxFallback {
    /// Stable log values. These are grepped and alerted on; do not rename one
    /// without meaning to break whatever is watching it.
    fn as_str(self) -> &'static str {
        match self {
            TaxFallback::NoBillingAddress => "no_billing_address",
            TaxFallback::IncompleteAddress => "incomplete_address",
            TaxFallback::CalculationRejected => "calculation_rejected",
            TaxFallback::CalculationUnavailable => "calculation_unavailable",
        }
    }
}

/// A buyer location good enough for Stripe Tax, lifted from the saved card.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaxAddress {
    country: String,
    postal_code: Option<String>,
    state: Option<String>,
    city: Option<String>,
    line1: Option<String>,
}

/// Read a usable tax location off a PaymentMethod object.
///
/// # Where this address comes from, and why there is no other candidate
///
/// [`ensure_stripe_customer`] creates the Stripe Customer with an email and a
/// metadata user id and NOTHING ELSE — no `address`, no `shipping`, no
/// `tax[validate_location]`. So the Customer object holds no location, and
/// passing `customer` to a tax calculation (which copies the Customer's address
/// into `customer_details`) would copy nothing. The saved card's
/// `billing_details.address`, captured by the `mode=setup` Checkout Session, is
/// the only address Stripe has for an autopay buyer.
///
/// The Tax API will not go and find it: "the address provided in the API
/// request is used directly for tax calculations. There is no fallback to other
/// address sources such as shipping address, billing address, payment method,
/// or IP addresses." (That fallback chain exists for Invoices and
/// Subscriptions, and even there it reaches a payment method only through a
/// *default* payment method pointer this deployment never sets.) So the address
/// has to be passed explicitly, and this is where it is read.
///
/// It costs no extra API call: `replay_charge` already lists the customer's
/// payment methods to find the card to charge, and `billing_details` rides
/// along in that same response.
///
/// # What is deliberately NOT done
///
/// **No address is invented.** There is no default country, no head-office
/// fallback, no guess from the card's issuing country. An absent or unusable
/// address means an untaxed charge and a `no_billing_address` /
/// `incomplete_address` log line, never a plausible-looking address that would
/// bill a real jurisdiction for a buyer who might not be in it.
///
/// **The address is not copied onto the Customer.** It would be a write to
/// shared Stripe state on the money path, with no idempotency key, racing
/// concurrent sweeps, to feed an API that does not read it — and it would
/// silently change the tax behaviour of any future invoice or subscription
/// surface, which is the operator's decision to make deliberately rather than
/// inherit from a top-up.
///
/// # The completeness rules
///
/// `country` is required by the API. Beyond that this mirrors Stripe's
/// documented minimums rather than sending a request that is bound to fail: a
/// US address needs a postal code, because Stripe cannot calculate US tax from
/// a country code alone *or* from country plus state. Checking locally turns a
/// guaranteed round trip and error into an immediate, cheaper fallback; the
/// remote `customer_tax_location_invalid` case is still handled, because an
/// address can pass these checks and still not resolve.
fn autopay_tax_address(payment_method: &Value) -> Result<TaxAddress, TaxFallback> {
    /// Stripe renders unset address components as JSON null; treat a
    /// whitespace-only string the same as absent so a blank form field cannot
    /// masquerade as a location.
    fn field(address: &Value, key: &str) -> Option<String> {
        let value = address.get(key)?.as_str()?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    let address = payment_method
        .get("billing_details")
        .and_then(|details| details.get("address"))
        .filter(|address| address.is_object())
        .ok_or(TaxFallback::NoBillingAddress)?;

    let country = field(address, "country").ok_or(TaxFallback::IncompleteAddress)?;
    let postal_code = field(address, "postal_code");
    if country.eq_ignore_ascii_case("US") && postal_code.is_none() {
        return Err(TaxFallback::IncompleteAddress);
    }
    Ok(TaxAddress {
        country,
        postal_code,
        state: field(address, "state"),
        city: field(address, "city"),
        line1: field(address, "line1"),
    })
}

/// A priced tax: what to add to the charge, and the calculation that says so.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaxQuote {
    tax_usd: Decimal,
    /// `None` only on the untaxed fallback, where the tax is zero and there is
    /// no calculation to record a transaction from.
    calculation_id: Option<String>,
}

impl TaxQuote {
    /// The untaxed fallback: charge exactly what this path charged before
    /// migration 0021.
    fn untaxed() -> Self {
        TaxQuote {
            tax_usd: Decimal::ZERO,
            calculation_id: None,
        }
    }
}

/// Price the sales tax on one autopay top-up.
///
/// The line item is the EX-TAX GROSS — credit plus deposit fee — because that
/// is the whole consideration the customer pays for the credits, and tax is due
/// on the consideration, not on the part of it ZeroRouter keeps. It sends no
/// `tax_code` and no `tax_behavior`, exactly as the checkout path does not:
/// both come from Tax Settings so the operator can revise a contested
/// classification without a deploy, and so checkout and autopay cannot drift
/// into taxing the same product two different ways.
///
/// # This function cannot fail the charge
///
/// Every error path returns the untaxed fallback and logs it with
/// [`TAX_FALLBACK_FIELD`]. That is the binding constraint on this whole
/// feature: with no tax registrations Stripe computes zero tax for everyone
/// anyway, so an untaxed top-up is today indistinguishable from a taxed one —
/// but a top-up that FAILED because a tax service was unreachable would stop a
/// customer's inference over a figure that is currently always zero.
///
/// # No API version pin
///
/// Unlike [`checkout_client`], this sends nothing version-gated: the endpoint
/// and the three response fields read here (`id`, `amount_total`,
/// `tax_amount_exclusive`) are long-stable, and no enum value introduced by a
/// later API version appears in the request. The checkout pin exists because
/// being wrong there is a total checkout outage; being wrong here is a logged
/// fallback. Pinning would also freeze tax computation at one version, when
/// tracking current rates and rules is the entire point of Stripe Tax.
async fn calculate_autopay_tax(
    settings: &StripeSettings,
    client: &reqwest::Client,
    user_id: Uuid,
    payment_method: &Value,
    gross_cents: i64,
) -> TaxQuote {
    let address = match autopay_tax_address(payment_method) {
        Ok(address) => address,
        Err(reason) => {
            tracing::warn!(
                %user_id,
                { TAX_FALLBACK_FIELD } = reason.as_str(),
                "autopay top-up charged without tax: the saved card has no usable billing address"
            );
            return TaxQuote::untaxed();
        }
    };

    let gross = gross_cents.to_string();
    let mut form: Vec<(&str, &str)> = vec![
        ("currency", CHECKOUT_CURRENCY),
        ("line_items[0][amount]", &gross),
        ("line_items[0][reference]", AUTOPAY_TAX_LINE_REFERENCE),
        ("customer_details[address][country]", &address.country),
        // Required whenever an address is given. `billing` is the truth: this
        // is the card's billing address, not a shipping destination.
        ("customer_details[address_source]", "billing"),
    ];
    for (key, value) in [
        (
            "customer_details[address][postal_code]",
            &address.postal_code,
        ),
        ("customer_details[address][state]", &address.state),
        ("customer_details[address][city]", &address.city),
        ("customer_details[address][line1]", &address.line1),
    ] {
        if let Some(value) = value {
            form.push((key, value));
        }
    }

    let response = client
        .post(format!("{}/v1/tax/calculations", settings.api_base))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(
                %user_id,
                { TAX_FALLBACK_FIELD } = TaxFallback::CalculationUnavailable.as_str(),
                "autopay top-up charged without tax: the Stripe Tax API could not be reached"
            );
            return TaxQuote::untaxed();
        }
    };
    let status = response.status();
    if !status.is_success() {
        // A 4xx is the buyer's address (`customer_tax_location_invalid` and
        // friends); a 5xx is Stripe. Both fall back, but they are different
        // operational problems and the log says which.
        let reason = if status.is_client_error() {
            TaxFallback::CalculationRejected
        } else {
            TaxFallback::CalculationUnavailable
        };
        tracing::warn!(
            %user_id,
            status = status.as_u16(),
            { TAX_FALLBACK_FIELD } = reason.as_str(),
            "autopay top-up charged without tax: Stripe refused to calculate it"
        );
        return TaxQuote::untaxed();
    }
    let Ok(body) = response.json::<Value>().await else {
        tracing::warn!(
            %user_id,
            { TAX_FALLBACK_FIELD } = TaxFallback::CalculationUnavailable.as_str(),
            "autopay top-up charged without tax: the tax calculation response did not parse"
        );
        return TaxQuote::untaxed();
    };

    // `tax_amount_exclusive` is "the amount of tax to be collected on top of
    // the line item prices" — the exclusive figure, which is the only one that
    // can be right here: the ToS prices credits exclusive of tax and the fee
    // margin assumes the gross arrives intact. A calculation that came back
    // INCLUSIVE would mean Tax Settings is on `Inclusive`, which is the same
    // misconfiguration that breaks checkout; refusing to read it keeps autopay
    // charging the correct (untaxed) amount instead of quietly collecting a tax
    // carved out of ZeroRouter's own margin.
    let inclusive = body
        .get("tax_amount_inclusive")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let exclusive = body.get("tax_amount_exclusive").and_then(Value::as_i64);
    let calculation_id = body.get("id").and_then(Value::as_str);
    let (Some(tax_cents), Some(calculation_id)) = (exclusive, calculation_id) else {
        tracing::warn!(
            %user_id,
            { TAX_FALLBACK_FIELD } = TaxFallback::CalculationUnavailable.as_str(),
            "autopay top-up charged without tax: the tax calculation omitted its id or exclusive tax"
        );
        return TaxQuote::untaxed();
    };
    if tax_cents < 0 || inclusive != 0 {
        tracing::error!(
            %user_id,
            tax_cents,
            inclusive,
            { TAX_FALLBACK_FIELD } = TaxFallback::CalculationRejected.as_str(),
            "autopay top-up charged without tax: the calculation was negative or tax-inclusive, which would carve tax out of the deposit fee — check Tax Settings' default tax behavior"
        );
        return TaxQuote::untaxed();
    }
    TaxQuote {
        tax_usd: Decimal::from(tax_cents) / Decimal::ONE_HUNDRED,
        calculation_id: Some(calculation_id.to_owned()),
    }
}

/// Turn the calculation that priced a settled charge into a recorded tax
/// TRANSACTION, which is what actually reaches Stripe's tax reports and the
/// filing export. A calculation alone is only a quote: "the Tax Transactions
/// page only includes transactions and not calculations".
///
/// # Why this is safe to call more than once
///
/// Stripe requires the `reference` to be "unique across all transactions,
/// including reversals", so passing the PaymentIntent id makes the endpoint
/// itself the deduplicator: the first call records, and a second call for the
/// same charge is refused by Stripe rather than double-reporting the tax. That
/// is belt and braces — the caller only reaches here when
/// [`billing::settle_autopay_intent`] reported `Credited`, which the
/// pending→succeeded transition already guarantees happens exactly once per
/// intent — but it means a redelivered event can never inflate a tax return
/// even if that guard were ever weakened.
///
/// # Why a failure here is logged rather than propagated
///
/// The money is already correct at this point: the card was charged and the
/// balance was credited. A failure to record leaves the collected tax missing
/// from the filing report, which is a reporting defect an operator must fix,
/// not a reason to fail a webhook that would then be redelivered and find
/// nothing left to settle. The log carries the calculation id precisely so it
/// can be replayed by hand.
async fn record_autopay_tax_transaction(
    settings: &StripeSettings,
    client: &reqwest::Client,
    payment_intent_id: &str,
    calculation_id: &str,
) {
    let form: [(&str, &str); 2] = [
        ("calculation", calculation_id),
        ("reference", payment_intent_id),
    ];
    let response = client
        .post(format!(
            "{}/v1/tax/transactions/create_from_calculation",
            settings.api_base
        ))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            tracing::info!(
                payment_intent = %payment_intent_id,
                tax_calculation = %calculation_id,
                "autopay tax transaction recorded"
            );
        }
        Ok(response) => {
            tracing::error!(
                payment_intent = %payment_intent_id,
                tax_calculation = %calculation_id,
                status = response.status().as_u16(),
                "autopay tax was COLLECTED but its tax transaction was not recorded; it will be missing from the Stripe Tax filing report until an operator creates it from this calculation"
            );
        }
        Err(_) => {
            tracing::error!(
                payment_intent = %payment_intent_id,
                tax_calculation = %calculation_id,
                "autopay tax was COLLECTED but the tax transaction call could not be sent; it will be missing from the Stripe Tax filing report until an operator creates it from this calculation"
            );
        }
    }
}

// POST /api/billing/autopay/setup — a Checkout session in `setup` mode that
// saves a card to the user's Stripe customer for off-session charging.
async fn create_autopay_setup(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let customer = ensure_stripe_customer(&ctx, stripe, user.user_id, &user.email).await?;
    let client = stripe_client()?;
    let success_url = ctx.config.absolute_url("/credits?autopay=saved");
    let cancel_url = ctx.config.absolute_url("/credits?autopay=cancelled");
    let form: [(&str, &str); 4] = [
        ("mode", "setup"),
        ("customer", &customer),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
    ];
    let response = client
        .post(checkout_sessions_url(stripe))
        .bearer_auth(&stripe.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "stripe setup session rejected"
        );
        return Err(StripeHttpError::CheckoutFailed);
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    let Some(url) = body.get("url").and_then(Value::as_str) else {
        return Err(StripeHttpError::CheckoutFailed);
    };
    Ok(Json(serde_json::json!({ "url": url })))
}

#[derive(Debug, serde::Serialize)]
struct AutopayStatus {
    enabled: bool,
    threshold_usd: Option<Decimal>,
    topup_usd: Option<Decimal>,
    consecutive_failures: i32,
    card_setup_started: bool,
}

// GET /api/billing/autopay
async fn get_autopay(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<Json<AutopayStatus>, StripeHttpError> {
    let row = sqlx::query_as::<_, (bool, Option<Decimal>, Option<Decimal>, i32, Option<String>)>(
        r#"
        SELECT autopay_enabled, autopay_threshold_usd, autopay_topup_usd,
               autopay_consecutive_failures, stripe_customer_id
        FROM users WHERE id = $1
        "#,
    )
    .bind(user.user_id)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    Ok(Json(AutopayStatus {
        enabled: row.0,
        threshold_usd: row.1,
        topup_usd: row.2,
        consecutive_failures: row.3,
        card_setup_started: row.4.is_some(),
    }))
}

#[derive(Debug, Deserialize)]
struct AutopayUpdate {
    enabled: bool,
    threshold_usd: Option<Decimal>,
    topup_usd: Option<Decimal>,
}

// PUT /api/billing/autopay
async fn put_autopay(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Json(update): Json<AutopayUpdate>,
) -> Result<Json<AutopayStatus>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    if update.enabled {
        let (Some(threshold), Some(topup)) = (update.threshold_usd, update.topup_usd) else {
            return Err(StripeHttpError::MalformedEvent);
        };
        // The top-up buys credits exactly like a manual checkout, so it
        // lives inside the same bounds; the threshold only needs to be a
        // sane non-negative trigger below the ceiling.
        validate_checkout_amount(topup, stripe).map_err(|_| StripeHttpError::MalformedEvent)?;
        if threshold < Decimal::ZERO || threshold > stripe.checkout_max_usd {
            return Err(StripeHttpError::MalformedEvent);
        }
        // A Stripe customer exists the moment setup STARTS; only a saved
        // card proves it finished. Enabling without one would burn the
        // three-strikes budget on a card that was never there (review
        // finding), so verify against Stripe at enable time.
        let customer = sqlx::query_scalar::<_, Option<String>>(
            "SELECT stripe_customer_id FROM users WHERE id = $1",
        )
        .bind(user.user_id)
        .fetch_one(&ctx.pool)
        .await
        .map_err(|_| StripeHttpError::BillingUnavailable)?;
        let Some(customer) = customer else {
            return Err(StripeHttpError::MalformedEvent);
        };
        let client = stripe_client()?;
        let methods: Value = client
            .get(format!(
                "{}/v1/customers/{customer}/payment_methods",
                stripe.api_base
            ))
            .query(&[("type", "card"), ("limit", "1")])
            .bearer_auth(&stripe.secret_key)
            .send()
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?
            .json()
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
        let has_card = methods
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| !data.is_empty());
        if !has_card {
            return Err(StripeHttpError::MalformedEvent);
        }
        let updated = sqlx::query(
            r#"
            UPDATE users
            SET autopay_enabled = TRUE,
                autopay_threshold_usd = $2,
                autopay_topup_usd = $3,
                autopay_consecutive_failures = 0
            WHERE id = $1 AND stripe_customer_id IS NOT NULL
            "#,
        )
        .bind(user.user_id)
        .bind(threshold)
        .bind(topup)
        .execute(&ctx.pool)
        .await
        .map_err(|_| StripeHttpError::BillingUnavailable)?
        .rows_affected();
        if updated == 0 {
            // No Stripe customer yet: card setup has not even started.
            return Err(StripeHttpError::MalformedEvent);
        }
    } else {
        sqlx::query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
            .bind(user.user_id)
            .execute(&ctx.pool)
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
    }
    get_autopay(State(ctx), user).await
}

/// `payment_intent.*` webhook arm. Only intents this router purposed as
/// autopay are consumed; everything else is acknowledged untouched. The
/// corroboration bar matches the checkout arm: the credited amount is the
/// money Stripe says it collected, and metadata must agree with it.
async fn handle_autopay_intent_event(
    ctx: &WebCtx,
    event: &StripeEvent,
) -> Result<Json<Value>, StripeHttpError> {
    let object = &event.data.object;
    let Some(intent_id) = object.get("id").and_then(Value::as_str) else {
        return Err(StripeHttpError::MalformedEvent);
    };
    let metadata = object.get("metadata");
    let purposed = metadata
        .and_then(|metadata| metadata.get("purpose"))
        .and_then(Value::as_str)
        == Some(AUTOPAY_PURPOSE);
    if !purposed {
        return Ok(received());
    }

    let stripe = ctx
        .config
        .stripe
        .as_ref()
        .ok_or(StripeHttpError::BillingUnavailable)?;
    let user_id_raw = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str);
    let credit_usd_raw = metadata
        .and_then(|metadata| metadata.get("credit_usd"))
        .and_then(Value::as_str);
    let provenance = metadata
        .and_then(|metadata| metadata.get("provenance"))
        .and_then(Value::as_str);
    let provenance_ok = match (user_id_raw, credit_usd_raw, provenance) {
        (Some(user), Some(credit), Some(mark)) => Uuid::parse_str(user).is_ok_and(|user| {
            constant_time_str_eq(mark, &autopay_provenance(stripe, user, credit))
        }),
        _ => false,
    };
    if !provenance_ok {
        // Purposed like ours but not provably ours: acknowledged untouched.
        // Metadata is writable by any integration sharing the Stripe
        // account; the HMAC is not.
        tracing::warn!(payment_intent = %intent_id, "autopay-shaped event without valid provenance; ignoring");
        return Ok(received());
    }

    if event.event_type == "payment_intent.payment_failed" {
        // Recovery applies to failures too: a decline whose sweep-side
        // record was lost must still exist as a terminal row, or the claim
        // slot and strike ledger drift from Stripe's reality.
        let user_id = user_id_raw.and_then(|raw| Uuid::parse_str(raw).ok());
        let credit_usd = credit_usd_raw.and_then(|raw| raw.parse::<Decimal>().ok());
        if let (Some(user_id), Some(credit_usd)) = (user_id, credit_usd) {
            // A terminal (failed) row is never credited, but the table now
            // records the gross charge beside the net credit; derive it from the
            // same fee helper so the charge >= credit CHECK holds.
            let gross_usd = deposit_fee_quote(credit_usd).gross_usd;
            billing::record_autopay_charge(&ctx.pool, intent_id, user_id, credit_usd, gross_usd)
                .await
                .map_err(|_| StripeHttpError::BillingUnavailable)?;
        }
        let handled = billing::fail_autopay_intent(&ctx.pool, intent_id)
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
        tracing::warn!(payment_intent = %intent_id, handled, "autopay charge failed");
        return Ok(received());
    }

    // payment_intent.succeeded
    let user_id = user_id_raw.and_then(|raw| Uuid::parse_str(raw).ok());
    let credit_usd = credit_usd_raw.and_then(|raw| raw.parse::<Decimal>().ok());
    let amount_received = object.get("amount_received").and_then(Value::as_i64);
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let (Some(user_id), Some(credit_usd), Some(amount_received), Some(currency)) =
        (user_id, credit_usd, amount_received, currency)
    else {
        tracing::warn!(payment_intent = %intent_id, "autopay success event missing corroboration fields");
        return Err(StripeHttpError::MalformedEvent);
    };
    // The metadata credit is NET; `amount_received` is what Stripe actually
    // collected, which since migration 0021 is the GROSS PLUS TAX. Recompute
    // the gross the fee formula demands for this credit and require Stripe to
    // have collected exactly it once the tax is taken back off — the autopay
    // twin of the checkout Layer-1 self-check, and the exact same manoeuvre
    // [`collected_ex_tax_cents`] performs there. Recomputed from the net
    // credit, not trusting metadata[gross_usd].
    let expected_gross = deposit_fee_quote(credit_usd).gross_usd;
    let Some(expected_gross_cents) = usd_to_cents(expected_gross) else {
        return Err(StripeHttpError::MalformedEvent);
    };
    // An ABSENT key is a pre-0021 intent: nothing in this path could add tax
    // then, so `amount_received` is the bare gross and zero is the truthful
    // reading. A PRESENT key must parse to a non-negative integer — anything
    // else is refused outright rather than coerced to zero, because coercing
    // would turn a garbled tax into a short payment credited in full.
    //
    // Note the direction of trust. `tax_cents` is not covered by the provenance
    // HMAC, and it does not need to be: it is SUBTRACTED from what Stripe says
    // it collected, so raising it only raises the bar for `amount_received`.
    // There is no value — the non-negative guard forecloses the negative ones —
    // that lets a smaller collection satisfy a larger credit.
    let reported_tax = object
        .get("metadata")
        .and_then(|metadata| metadata.get(AUTOPAY_TAX_CENTS_KEY));
    let tax_cents = match reported_tax {
        None | Some(Value::Null) => 0,
        Some(reported) => {
            let parsed = reported
                .as_str()
                .and_then(|raw| raw.parse::<i64>().ok())
                .filter(|cents| *cents >= 0);
            let Some(parsed) = parsed else {
                tracing::error!(
                    payment_intent = %intent_id,
                    metadata_user_id = %user_id,
                    "autopay success event carries an unusable tax figure; crediting nothing"
                );
                return Err(StripeHttpError::AmountMismatch);
            };
            parsed
        }
    };
    let collected_ex_tax = amount_received.checked_sub(tax_cents);
    if collected_ex_tax != Some(expected_gross_cents) || currency != CHECKOUT_CURRENCY {
        tracing::error!(
            payment_intent = %intent_id,
            metadata_user_id = %user_id,
            expected_gross_cents,
            amount_received,
            tax_cents,
            %currency,
            "autopay success event does not corroborate its metadata; crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    }
    // Pass the NET credit (what settle applies) and the GROSS (what the stored
    // row's charge must match) separately.
    let outcome = billing::settle_autopay_intent(
        &ctx.pool,
        intent_id,
        Some((user_id, credit_usd, expected_gross)),
    )
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    tracing::info!(payment_intent = %intent_id, ?outcome, "autopay charge settled");

    // Put the sale into Stripe's tax reports, and do it exactly once.
    //
    // The gate is `Credited`, not "the event said succeeded": the
    // pending→succeeded transition inside `settle_autopay_intent` fires once per
    // intent, so a redelivered event — Stripe retries, and the sweep's inline
    // settle races the webhook on every fast charge — comes back
    // `AlreadySettled` and records nothing. A `Withheld` outcome records nothing
    // either, and deliberately: that money is queued for an operator refund, so
    // reporting tax on it would have to be reversed again.
    //
    // A ZERO-tax calculation is recorded too, on the same terms as a positive
    // one. With no registrations every calculation comes back zero, so a
    // `tax_cents > 0` gate would record nothing at all today — and the
    // zero-rated transactions are exactly the ones that evidence sales volume
    // per jurisdiction, which is what says when a registration becomes
    // required. The condition is therefore "we asked Stripe Tax and it
    // answered" (a calculation id exists), not "the answer was nonzero".
    if outcome == billing::AutopayOutcome::Credited
        && let Some(calculation_id) = object
            .get("metadata")
            .and_then(|metadata| metadata.get(AUTOPAY_TAX_CALCULATION_KEY))
            .and_then(Value::as_str)
        && let Ok(client) = stripe_client()
    {
        record_autopay_tax_transaction(stripe, &client, intent_id, calculation_id).await;
    }
    Ok(received())
}

/// One sweep pass: reconcile stale pending charges, then find users under
/// their threshold and charge their saved card off-session. Public and
/// synchronous so tests drive the exact code production loops.
pub async fn run_autopay_sweep_once(pool: &crate::sqlx::PgPool, settings: &StripeSettings) {
    reconcile_stale_intents(pool, settings).await;
    let candidates = match billing::autopay_candidates(pool, AUTOPAY_SWEEP_BATCH).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(%error, "autopay sweep could not list candidates");
            return;
        }
    };
    for candidate in candidates {
        if let Err(error) = charge_candidate(pool, settings, &candidate).await {
            tracing::warn!(
                user_id = %candidate.user_id,
                %error,
                "autopay charge attempt failed"
            );
        }
    }
}

/// Pending rows older than the cutoff mean a webhook or a Stripe response
/// was lost. Local claims are retried against Stripe under their original
/// idempotency key (the same PaymentIntent answers, never a second
/// charge); real intents are queried by id and settled or failed by what
/// Stripe says actually happened. Without this pass, one lost message
/// wedges the user's one-pending-per-user slot forever.
async fn reconcile_stale_intents(pool: &crate::sqlx::PgPool, settings: &StripeSettings) {
    // Claims too old to replay safely are money in an unknown state: logged
    // loudly for an operator, never retried automatically.
    match billing::overdue_autopay_intents(pool, AUTOPAY_REPLAY_MAX_AGE_MINUTES).await {
        Ok(overdue) if !overdue.is_empty() => tracing::error!(
            count = overdue.len(),
            "autopay claims are older than the idempotency-retention window and need operator reconciliation"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not list overdue autopay claims"),
    }
    // Charges collected at Stripe whose credit was WITHHELD at settlement (FIX 1)
    // are money owed back to a frozen / indebted account. Surface them loudly on
    // every pass until an operator refunds them out of band; automation must not
    // credit them, and deliberately does not refund inline.
    match billing::withheld_autopay_intents(pool).await {
        Ok(withheld) if !withheld.is_empty() => tracing::error!(
            count = withheld.len(),
            "autopay charges were collected but withheld from a frozen / indebted account and need an operator refund"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not list withheld autopay charges"),
    }
    let stale = match billing::stale_autopay_intents(
        pool,
        AUTOPAY_RECONCILE_AFTER_MINUTES,
        AUTOPAY_REPLAY_MAX_AGE_MINUTES,
    )
    .await
    {
        Ok(stale) => stale,
        Err(error) => {
            tracing::warn!(%error, "autopay reconciliation could not list stale intents");
            return;
        }
    };
    for (intent_id, user_id, amount_usd, charge_amount_usd) in stale {
        let outcome = if let Some(idempotency_key) = intent_id.strip_prefix("local_") {
            // Up to half an hour has passed since the claim was taken. If
            // the user has turned autopay off in the meantime, the claim is
            // released rather than replayed: nobody who has opted out gets
            // charged by a message we lost (sol review).
            match billing::autopay_still_armed(pool, user_id).await {
                Ok(false) => {
                    // Deliberately NOT deleted: a stranded claim may already
                    // have been charged, and its key is the only durable
                    // handle on that charge. Dropping it would lose the
                    // credit and free the slot for a second one (sol
                    // review). Stop replaying; leave it for reconciliation.
                    tracing::warn!(
                        %user_id,
                        "not replaying a stranded autopay claim: the user has opted out (the claim is kept — it may already have been charged)"
                    );
                    Ok(())
                }
                Ok(true) => {
                    replay_charge(pool, settings, user_id, amount_usd, idempotency_key).await
                }
                Err(error) => Err(anyhow::Error::from(error)),
            }
        } else {
            reconcile_real_intent(
                pool,
                settings,
                &intent_id,
                user_id,
                amount_usd,
                charge_amount_usd,
            )
            .await
        };
        if let Err(error) = outcome {
            tracing::warn!(payment_intent = %intent_id, %error, "autopay reconciliation failed");
        }
    }
}

async fn reconcile_real_intent(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    intent_id: &str,
    user_id: Uuid,
    amount_usd: Decimal,
    charge_amount_usd: Decimal,
) -> anyhow::Result<()> {
    let client = stripe_client().map_err(|_| anyhow::anyhow!("stripe client"))?;
    let response = client
        .get(format!(
            "{}/v1/payment_intents/{intent_id}",
            settings.api_base
        ))
        .bearer_auth(&settings.secret_key)
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("stripe lookup rejected (HTTP {})", response.status());
    }
    let body: Value = response.json().await?;
    match body.get("status").and_then(Value::as_str) {
        Some("succeeded") => {
            billing::settle_autopay_intent(
                pool,
                intent_id,
                Some((user_id, amount_usd, charge_amount_usd)),
            )
            .await?;
        }
        Some("processing") => {}
        _ => {
            billing::fail_autopay_intent(pool, intent_id).await?;
        }
    }
    Ok(())
}

async fn charge_candidate(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    candidate: &billing::AutopayCandidate,
) -> anyhow::Result<()> {
    // Claim BEFORE any money can move: the one-pending-per-user index makes
    // this exclusive, so overlapping sweeps cannot double-charge, and the
    // idempotency key survives in the claim row so a lost response is
    // replayed against the SAME PaymentIntent (review findings).
    let idempotency_key = Uuid::new_v4().simple().to_string();
    // Price the deposit once so the claim records both the NET credit the user
    // wants and the GROSS charge Stripe will collect. `replay_charge` re-prices
    // from the same net topup, so the two agree by construction.
    let quote = deposit_fee_quote(candidate.topup_usd);
    if !billing::claim_autopay_attempt(
        pool,
        candidate.user_id,
        candidate.topup_usd,
        quote.gross_usd,
        &idempotency_key,
    )
    .await?
    {
        anyhow::bail!("user already has a charge in flight");
    }
    replay_charge(
        pool,
        settings,
        candidate.user_id,
        candidate.topup_usd,
        &idempotency_key,
    )
    .await
}

/// Create (or idempotently re-create) the off-session charge for a claim.
/// Shared by the first attempt and reconciliation replays; Stripe's
/// idempotency layer guarantees both paths observe one PaymentIntent.
async fn replay_charge(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    user_id: Uuid,
    topup_usd: Decimal,
    idempotency_key: &str,
) -> anyhow::Result<()> {
    let client = stripe_client().map_err(|_| anyhow::anyhow!("stripe client"))?;
    let Some(customer) = sqlx::query_scalar::<_, Option<String>>(
        "SELECT stripe_customer_id FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?
    else {
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("user has no stripe customer");
    };

    let response = client
        .get(format!(
            "{}/v1/customers/{customer}/payment_methods",
            settings.api_base
        ))
        .query(&[("type", "card"), ("limit", "1")])
        .bearer_auth(&settings.secret_key)
        .send()
        .await?;
    // A non-2xx here is Stripe failing to answer, NOT the user having no
    // card. Reading it as "no saved card" terminal-failed the claim and
    // freed the slot on a transient blip (sol review).
    if !response.status().is_success() {
        anyhow::bail!(
            "stripe could not list payment methods (HTTP {}); holding the claim",
            response.status()
        );
    }
    let methods: Value = response.json().await?;
    let Some(card) = methods
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
    else {
        // No saved card: the claim itself becomes the terminal failed
        // intent, which both counts the strike and releases the slot —
        // exactly once even under racing sweeps, because only the claim
        // holder reaches here.
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("no saved card payment method");
    };
    let Some(payment_method) = card.get("id").and_then(Value::as_str) else {
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("no saved card payment method");
    };

    // `topup_usd` is the NET credit the user wants; the fee rides on top and
    // Stripe collects the gross.
    let quote = deposit_fee_quote(topup_usd);
    let Some(gross_cents) = usd_to_cents(quote.gross_usd) else {
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("top-up gross is not a whole cent");
    };

    // ---- Sales tax (migration 0021) --------------------------------------
    //
    // Priced BEFORE the POST and frozen onto the claim row, in that order,
    // because Stripe's idempotency layer compares the parameters of a replay
    // against the original request and rejects a mismatch. A reconciliation
    // replay of this same claim must therefore send the SAME `amount`, which it
    // can only do if the tax is a stored fact rather than a fresh computation.
    //
    // Reusing an already-frozen tax also skips the Stripe Tax call entirely on
    // a replay — Stripe bills per calculation, and a second answer would be
    // discarded anyway.
    let claim_id = format!("local_{idempotency_key}");
    let frozen = billing::autopay_tax(pool, &claim_id).await?;
    let tax = match frozen {
        Some(tax) => TaxQuote {
            tax_usd: tax.tax_usd,
            calculation_id: tax.calculation_id,
        },
        None => {
            let priced = calculate_autopay_tax(settings, &client, user_id, card, gross_cents).await;
            // The freeze is what makes this attempt's figure authoritative —
            // or adopts a concurrent racer's, if one got here first. Either
            // way both attempts POST the same amount. A `None` means the claim
            // is no longer pending (settled or failed underneath us), in which
            // case there is nothing left to charge.
            match billing::freeze_autopay_tax(
                pool,
                &claim_id,
                priced.tax_usd,
                priced.calculation_id.as_deref(),
            )
            .await?
            {
                Some(frozen) => TaxQuote {
                    tax_usd: frozen.tax_usd,
                    calculation_id: frozen.calculation_id,
                },
                None => anyhow::bail!("autopay claim is no longer pending; not charging"),
            }
        }
    };
    let Some(tax_cents) = usd_to_cents(tax.tax_usd).filter(|cents| *cents >= 0) else {
        // Unreachable through `calculate_autopay_tax`, which only ever yields
        // whole non-negative cents; reachable through a hand-edited row. Refuse
        // rather than charge an amount nobody can reconstruct.
        billing::fail_autopay_intent(pool, &claim_id).await?;
        anyhow::bail!("frozen autopay tax is not a whole non-negative cent");
    };
    let Some(amount_cents) = gross_cents.checked_add(tax_cents) else {
        billing::fail_autopay_intent(pool, &claim_id).await?;
        anyhow::bail!("taxed autopay total overflows");
    };

    let amount = amount_cents.to_string();
    let user_id_text = user_id.to_string();
    let credit_usd = topup_usd.to_string();
    let fee_usd = quote.fee_usd.to_string();
    let gross_usd = quote.gross_usd.to_string();
    let tax_cents_text = tax_cents.to_string();
    // Provenance HMACs the NET credit, unchanged — the webhook recomputes it
    // from metadata[credit_usd] exactly as before. fee/gross are informational,
    // and the webhook re-derives the gross it corroborates from the net credit.
    //
    // The tax is deliberately NOT added to the HMAC input, for two reasons.
    // Rollout: an intent created by the pre-0021 binary carries a mark over
    // `purpose|user_id|credit_usd`, and widening the input would make this
    // binary reject its success webhook as unprovenanced and never credit a
    // charge that already took the customer's money. Safety: it buys nothing.
    // The webhook requires `amount_received - tax_cents == expected_gross` with
    // `tax_cents >= 0`, so a forged tax can only ever demand MORE money, never
    // less — there is no value of it that credits a purchase Stripe did not
    // collect in full.
    let provenance = autopay_provenance(settings, user_id, &credit_usd);
    let mut form: Vec<(&str, &str)> = vec![
        ("amount", &amount),
        ("currency", CHECKOUT_CURRENCY),
        ("customer", &customer),
        ("payment_method", payment_method),
        ("off_session", "true"),
        ("confirm", "true"),
        ("metadata[purpose]", AUTOPAY_PURPOSE),
        ("metadata[user_id]", &user_id_text),
        ("metadata[credit_usd]", &credit_usd),
        ("metadata[fee_usd]", &fee_usd),
        ("metadata[gross_usd]", &gross_usd),
        ("metadata[provenance]", &provenance),
    ];
    // Always sent, including as "0": its ABSENCE is what tells the webhook it
    // is looking at a pre-0021 intent whose `amount_received` is the bare
    // gross, so an explicit zero and a missing key must stay distinguishable.
    form.push((AUTOPAY_TAX_CENTS_PARAM, &tax_cents_text));
    if let Some(calculation_id) = tax.calculation_id.as_deref() {
        form.push((AUTOPAY_TAX_CALCULATION_PARAM, calculation_id));
    }

    // HIGH-2: the last line of defense, immediately before money moves. A
    // dispute-freeze — and the balance reversal that drives the account into a
    // receivable — can commit AFTER candidate selection and the claim,
    // including during the payment-methods round-trip just above, and neither
    // autopay_candidates nor claim_autopay_attempt re-runs at this instant.
    // Off-session charging the saved card of a customer who just disputed is
    // the exact catastrophe migration 0009 exists to prevent, so re-assert the
    // shared eligibility predicate here — AND `autopay_enabled`, so an opt-out
    // that commits in this same window (the portal's off switch) refuses the
    // charge too, not just a freeze / receivable. This is the same
    // `autopay_enabled AND (…)` gate the claim and still-armed checks apply.
    //
    // A residual window remains between this SELECT and the `.send()` below — an
    // inherent local-check-vs-external-side-effect gap. It is NOT closed by
    // holding a DB advisory lock across the Stripe POST: `send().await` spans
    // pool acquisition, DNS/TCP/TLS, and request transmission under a 15s HTTP
    // timeout, and pinning a pooled connection (and blocking the freeze webhook)
    // for that long is the anti-pattern we refuse. So the CHARGE here is
    // best-effort: if a freeze commits inside that window and the POST still
    // lands, a card may rarely be charged. What is NOT best-effort is the CREDIT.
    // `settle_autopay_intent` re-checks this SAME eligibility predicate under the
    // per-user advisory lock before crediting (FIX 1) and WITHHOLDS the credit
    // for an account frozen / indebted mid-charge, moving the intent to the
    // `withheld` (needs-refund) state for an operator to refund out of band. A
    // frozen / indebted account is therefore never CREDITED by autopay, even in
    // the rare case its card was charged in this race.
    let eligible = sqlx::query_scalar::<_, bool>(&format!(
        "SELECT (autopay_enabled AND ({})) FROM users WHERE id = $1",
        billing::AUTOPAY_ELIGIBILITY_PREDICATE
    ))
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    if !eligible {
        // FIX 2: HOLD the claim, never release it here. Round 2's FIX D deleted
        // a claim it believed was fresh-and-unsubmitted, but that judgment came
        // from `AutopayClaimOrigin` — the CALLER's local history, not the claim's
        // GLOBAL submission state. A genuinely fresh claim that paused past the
        // stale threshold can be picked up and POSTed by a reconciliation replay
        // on another instance, so deleting the pending row here can drop the only
        // durable idempotency handle on a charge that may already have happened —
        // reintroducing the known "opt-out deletes an ambiguous autopay claim
        // that may already be charged" bug. A safe hold beats an unsafe delete:
        // bail without deleting and leave the pending row for reconciliation. The
        // resulting block-until-resolved (one-pending-per-user wedged until an
        // operator acts) is intentionally deferred to the operator-resolution
        // feature (`v2-overdue-autopay-claims-have-no-resolution-path`), not
        // closed by a delete that can lose a customer's money.
        tracing::warn!(
            %user_id,
            "not charging a frozen / indebted / max-failed account; holding the autopay claim for reconciliation (never deleted — it may already have been submitted)"
        );
        anyhow::bail!("account is no longer eligible for autopay; holding the claim");
    }

    let response = client
        .post(format!("{}/v1/payment_intents", settings.api_base))
        .header("Idempotency-Key", idempotency_key)
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();

    if status.is_success() {
        let intent_id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("payment intent response missing id"))?;
        billing::attach_autopay_intent(pool, idempotency_key, intent_id).await?;
        if body.get("status").and_then(Value::as_str) == Some("succeeded") {
            let outcome = billing::settle_autopay_intent(
                pool,
                intent_id,
                Some((user_id, topup_usd, quote.gross_usd)),
            )
            .await?;
            // Record the tax ONLY on the settlement that actually credited.
            // `settle_autopay_intent` makes the pending→succeeded transition
            // exactly once, so exactly one of this inline path and the
            // `payment_intent.succeeded` webhook — whichever gets there first —
            // sees `Credited`; the other sees `AlreadySettled` and records
            // nothing. That is what keeps a sale from being reported twice to a
            // tax authority when both fire, which they routinely do.
            if outcome == billing::AutopayOutcome::Credited
                && let Some(calculation_id) = tax.calculation_id.as_deref()
            {
                record_autopay_tax_transaction(settings, &client, intent_id, calculation_id).await;
            }
        }
        return Ok(());
    }

    // Whether the outcome is KNOWN is decided before anything is marked
    // failed — including when the body names a PaymentIntent.
    //
    // Stripe documents 5xx outcomes as indeterminate: a 500 naming an intent
    // may still be reported succeeded later, so failing that row leaves the
    // eventual webhook unable to credit it AND frees the slot for a second
    // charge. A 409 means a concurrent replay of the same idempotency key is
    // executing right now — the peer may be charging. Neither is terminal
    // (sol review of the first version of this fix, which got both wrong).
    let named_intent = body
        .get("error")
        .and_then(|error| error.get("payment_intent"))
        .and_then(|intent| intent.get("id"))
        .and_then(Value::as_str);
    let indeterminate = status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::CONFLICT;
    if indeterminate {
        // Attaching a named intent is still worth doing: its id is what lets
        // reconciliation ask Stripe what actually happened. The row stays
        // pending, so the slot stays held.
        if let Some(intent_id) = named_intent {
            billing::attach_autopay_intent(pool, idempotency_key, intent_id).await?;
        }
        tracing::warn!(
            %status,
            attached = named_intent.is_some(),
            "autopay charge outcome is indeterminate; the claim is held for reconciliation"
        );
        anyhow::bail!("stripe returned an indeterminate autopay outcome (HTTP {status})")
    }

    // A definitive rejection: Stripe understood the request and refused it.
    // Declines carry the created (failed) intent, so the strike counts
    // against a real intent and the slot frees.
    if let Some(intent_id) = named_intent {
        billing::attach_autopay_intent(pool, idempotency_key, intent_id).await?;
        billing::fail_autopay_intent(pool, intent_id).await?;
        anyhow::bail!("stripe declined the off-session charge (HTTP {status})")
    }

    // Rejected with no intent named: nothing was created, so the claim
    // itself becomes the terminal failure — the strike counts and the slot
    // frees.
    billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
    anyhow::bail!("stripe rejected the off-session charge (HTTP {status})")
}
