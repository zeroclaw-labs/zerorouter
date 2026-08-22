// ZeroRouter Terms of Service — published. Substantive changes need Jordan's
// sign-off; keep the deposit-fee, pass-through-pricing, and chargeback-freeze
// descriptions in sync with the billing implementation.

export function Terms() {
  return (
    <article className="legal">
      <h1>Terms of Service</h1>
      <p className="legal-meta">Effective date: August 14, 2026 · Last updated: August 16, 2026</p>

      <h2>1. Agreement &amp; who we are</h2>
      <p>
        These Terms of Service (the “Terms”) are a binding agreement between you and{' '}
        ZeroClaw Labs PBC (“ZeroRouter”, “we”, “us”, or “our”), the operator of the
        ZeroRouter service (the “Service”). By creating an account, adding credits, or otherwise using
        the Service, you agree to these Terms. If you use the Service on behalf of an organization,
        you represent that you have authority to bind that organization, and “you” refers to that
        organization.
      </p>

      <h2>2. The Service</h2>
      <p>
        ZeroRouter is a prepaid, OpenAI-compatible gateway that provides access to third-party large
        language model (“LLM”) providers. You buy credits in advance and spend them on inference
        requests routed to the model provider you select. ZeroRouter exposes a standard
        OpenAI-compatible <span className="mono">/v1</span> API; you point any compatible client at it
        and request a model by its catalog id. ZeroRouter does not itself train or host the underlying
        models — it meters usage and routes each request to the selected upstream provider.
      </p>

      <h2>3. Eligibility &amp; account</h2>
      <p>
        You must be of the legal age required to form a binding contract in your jurisdiction to use
        the Service. You are responsible for your account and for all activity that occurs under it,
        including all usage under your API keys. You must keep your API keys secret: anyone holding a
        key can spend your credits. You are liable for all usage under your keys until you revoke them
        through the portal. Notify us promptly at support@zerorouter.ai if you suspect unauthorized
        use of your account or keys.
      </p>

      <h2>4. Credits, pricing &amp; payment</h2>
      <ul>
        <li>
          <strong>Prepaid credits.</strong> Credits are prepaid, denominated in US dollars, and
          purchased through our payment processor, Stripe. Credits are a prepaid balance usable only
          for inference on the Service; they are not a deposit account, e-money, or a general-purpose
          stored-value instrument, and they bear no interest.
        </li>
        <li>
          <strong>Deposit fee.</strong> A deposit fee of approximately 5.5% (subject to a $0.80
          minimum) applies at the time of purchase to cover payment-processing and handling costs. You
          are charged the gross amount and credited the net amount after the fee. The exact fee is
          shown before you confirm a purchase, and the amount charged at checkout is authoritative.
          Where stablecoin payment is offered, the deposit fee for that payment method is 5% with no
          minimum.
        </li>
        <li>
          <strong>Stablecoin payments.</strong> Where offered, credits may be purchased with
          supported stablecoins through Stripe. Such payments settle to us in US dollars; we do not
          hold, custody, transmit, or exchange digital assets, and we do not accept payment in any
          asset Stripe does not settle. Amounts are denominated and payable in US dollars, and the
          US dollar amount shown at checkout is authoritative. Stablecoin payments are subject to
          the payment processor’s limits, including a maximum per transaction. Because such
          payments are not reversible by the payment network, the chargeback and dispute provisions
          below do not apply to them; requests for adjustment are handled under Refunds above and
          remain at our discretion.
        </li>
        <li>
          <strong>Pass-through pricing.</strong> Inference is billed on a pass-through basis at the
          selected provider’s rates, metered by token usage. We do not mark up inference. The live
          catalog in the portal is the source of truth for current model prices, tiers, and
          availability.
        </li>
        <li>
          <strong>Price changes.</strong> Provider rates and the catalog may change at any time;
          changes apply to usage occurring after they take effect.
        </li>
        <li>
          <strong>Refunds.</strong> Credits are non-refundable except where required by applicable
          law. We may, in our sole discretion, refund unused credits (less the non-refundable deposit
          fee) as a one-time courtesy; any such refund is not an obligation and does not create a
          right to future refunds.
        </li>
        <li>
          <strong>Chargebacks &amp; disputes.</strong> Initiating a chargeback or payment dispute may
          freeze your account and remaining balance pending review. We may deduct disputed amounts,
          related fees, and reasonable costs from your balance.
        </li>
        <li>
          <strong>Taxes.</strong> Prices are exclusive of taxes. You are responsible for any taxes,
          duties, or levies associated with your purchases, other than taxes based on our net income.
        </li>
      </ul>

      <h2>5. Acceptable use</h2>
      <p>
        You may use the Service only for lawful purposes and in compliance with these Terms. Because
        requests are fulfilled by third-party model providers, your use is also subject to the usage
        policies and acceptable-use terms of the upstream provider you select, which flow through to
        you. You agree not to: use the Service unlawfully or to produce unlawful content; attempt to
        evade, tamper with, or circumvent metering, billing, rate limits, or quotas; probe, overload,
        or disrupt the Service or its infrastructure; resell or expose the Service in a manner that
        violates an upstream provider’s terms; or use the Service to infringe the rights of others.
      </p>

      <h2>6. AI outputs</h2>
      <p>
        Outputs generated by the underlying models are provided “as is”. AI systems can produce
        inaccurate, incomplete, offensive, or otherwise unreliable content, and similar outputs may be
        generated for other users. You are solely responsible for evaluating outputs and for how you
        use them, including any decisions or content you derive from them. Do not rely on outputs as a
        substitute for professional advice.
      </p>

      <h2>7. Availability &amp; changes</h2>
      <p>
        The Service is offered as a private beta and is provided on an “as is” and “as available”
        basis. Features, model availability, tiers, and pricing may change, and we may modify,
        suspend, or discontinue all or part of the Service at any time. We will use reasonable efforts
        to communicate material changes.
      </p>

      <h2>8. Suspension &amp; termination</h2>
      <p>
        We may suspend or terminate your access, in whole or in part, for actual or suspected abuse,
        non-payment, fraud, payment disputes, violation of these Terms or of an upstream provider’s
        policies, or as needed to comply with law or protect the Service or its users. You may stop
        using the Service at any time. On termination, unused credits are handled as described in
        Section 4 and subject to applicable law. Provisions that by their nature should survive
        termination — including disclaimers, limitation of liability, and indemnification — survive.
      </p>

      <h2>9. Disclaimers</h2>
      <p>
        To the maximum extent permitted by law, the Service is provided “AS IS” and “AS AVAILABLE”
        without warranties of any kind, whether express, implied, or statutory, including any implied
        warranties of merchantability, fitness for a particular purpose, title, and non-infringement.
        We do not warrant that the Service will be uninterrupted, secure, or error-free, or that
        outputs will be accurate or reliable.
      </p>

      <h2>10. Limitation of liability</h2>
      <p>
        To the maximum extent permitted by law, ZeroRouter and its affiliates, officers, employees,
        and suppliers will not be liable for any indirect, incidental, special, consequential,
        exemplary, or punitive damages, or for lost profits, revenues, data, or goodwill. Our total
        aggregate liability arising out of or relating to the Service or these Terms will not exceed
        the greater of the amounts you paid to us in the three (3) months preceding the event giving
        rise to the claim, or one hundred US dollars (US$100). Some jurisdictions do not allow certain
        limitations, so some of the above may not apply to you.
      </p>

      <h2>11. Indemnification</h2>
      <p>
        You agree to indemnify and hold harmless ZeroClaw Labs PBC and its affiliates
        from and against any claims, damages, liabilities, and expenses (including reasonable legal
        fees) arising out of your use of the Service, your inputs and content, your violation of these
        Terms or of an upstream provider’s policies, or your violation of any law or third-party
        right.
      </p>

      <h2>12. Governing law &amp; disputes</h2>
      <p>
        These Terms are governed by the laws of the Commonwealth of Massachusetts, United States, without regard to its
        conflict-of-laws rules. The courts located in the Commonwealth of Massachusetts, United States will have exclusive
        jurisdiction over any dispute arising out of or relating to these Terms or the Service, except
        where prohibited by applicable law.
      </p>

      <h2>13. Changes to these Terms</h2>
      <p>
        We may update these Terms from time to time. When we do, we will revise the “Last updated”
        date above and, for material changes, provide reasonable notice. Your continued use of the
        Service after changes take effect constitutes acceptance of the revised Terms.
      </p>

      <h2>14. Contact</h2>
      <p>
        Questions about these Terms can be sent to support@zerorouter.ai.
      </p>
    </article>
  )
}
