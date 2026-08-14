// Draft — pending legal review. Not legal advice.
//
// This file contains a DRAFT Privacy Policy. It is a working draft for internal
// review and must be reviewed and approved by qualified legal counsel before it
// is relied upon or published. The content-free-by-design statements below
// describe ZeroRouter's own systems and MUST NOT be edited to overclaim: the
// upstream nuance (requests are forwarded to the selected third-party provider,
// which processes content under its own terms) must remain. Bracketed
// [placeholders] mark details to be resolved by counsel.

export function Privacy() {
  return (
    <article className="legal">
      <div className="legal-draft" role="note">
        <strong>Draft — pending legal review. Not legal advice.</strong> This document is a working
        draft for internal review and must be reviewed and approved by qualified legal counsel before
        it is relied upon. Bracketed items are placeholders to be resolved before publication.
      </div>

      <h1>Privacy Policy</h1>
      <p className="legal-meta">Effective date: August 14, 2026 · Last updated: August 14, 2026</p>

      <h2>1. Overview &amp; scope</h2>
      <p>
        This Privacy Policy explains how ZeroClaw Labs PBC (“ZeroRouter”, “we”,
        “us”, or “our”) handles information in connection with the ZeroRouter service (the “Service”).
        It covers the data that ZeroRouter’s own systems collect and process. It does not govern the
        third-party model providers that fulfil your inference requests; each of those providers
        processes your content under its own terms and privacy policy, as described in
        “Our commitment: content-free by design” and “Third parties who process data” below.
      </p>

      <h2>2. Our commitment: content-free by design</h2>
      <p>
        ZeroRouter is <strong>content-free by design</strong>. We do not store, log, or train on the
        content of your prompts or completions. Our metering system records only the information
        needed to operate and bill the Service — <strong>token counts, model/tier, cost, timestamps,
        cryptographic hashes, and API-key identifiers</strong> — <strong>never the text of your
        requests or responses</strong>. Because ZeroRouter’s codebase is open source, this
        content-free boundary is independently auditable rather than a promise you have to take on
        faith.
      </p>
      <p>
        <strong>The honest upstream nuance.</strong> To actually fulfil a request, ZeroRouter{' '}
        <strong>forwards your prompt to the third-party model provider you selected</strong> (for
        example Anthropic, OpenAI, or another provider in the catalog). That provider receives and
        processes your request content in order to generate a response, under its own terms and
        privacy policy. ZeroRouter’s content-free guarantee covers ZeroRouter’s own systems; it does
        not — and cannot — promise how upstream providers handle your content. Where providers offer
        zero-data-retention or similar arrangements, we pursue them, but we do not claim that any
        given provider never sees or retains your content.
      </p>

      <h2>3. Information we collect</h2>
      <ul>
        <li>
          <strong>Account information.</strong> An email address or identifier provided when you sign
          in through our identity provider via single sign-on (SSO). We use this to identify your
          account.
        </li>
        <li>
          <strong>Usage metadata.</strong> For each request we record metadata such as token counts,
          model/tier, cost, timestamps, cryptographic request hashes, and API-key identifiers. This
          metadata does <strong>not</strong> include the content of your prompts or completions.
        </li>
        <li>
          <strong>Payment information.</strong> Purchases are handled by Stripe. We do not store your
          full card numbers. We keep purchase records such as amount, date, and Stripe references (for
          example a customer or charge identifier) for billing and accounting.
        </li>
        <li>
          <strong>Technical &amp; operational data.</strong> We process limited technical data such as
          IP addresses and server logs for security, abuse prevention, and operations. These logs are
          content-free — they do not capture prompt or completion text.
        </li>
      </ul>

      <h2>4. What we do NOT collect</h2>
      <p>
        We do not collect or retain the content of your prompts or completions, and we do not use your
        inputs or outputs to train models. Content passes through ZeroRouter to the selected provider
        to serve your request and is not written to our stores.
      </p>

      <h2>5. Third parties who process data</h2>
      <p>We rely on a small number of processors and providers:</p>
      <ul>
        <li>
          <strong>Stripe</strong> — payment processing.
        </li>
        <li>
          <strong>Upstream model providers</strong> — the provider you select receives your request
          content in order to generate a response and processes it under its own terms and privacy
          policy. This is the upstream nuance described in Section 2.
        </li>
        <li>
          <strong>Cloud infrastructure</strong> — Amazon Web Services (AWS) hosts our systems.
        </li>
        <li>
          <strong>Identity provider</strong> — Zitadel provides single sign-on for account login.
        </li>
      </ul>
      <p>We do not sell your personal data.</p>

      <h2>6. How we use information</h2>
      <p>
        We use the information described above to provide the Service and route your requests, meter
        usage and bill you, secure the Service and prevent abuse and fraud, comply with our legal and
        accounting obligations, and communicate with you about your account.
      </p>

      <h2>7. Retention</h2>
      <p>
        We retain account and billing records for as long as needed to operate your account and to
        meet legal, tax, and accounting requirements. We retain usage metadata as needed for billing
        and operations. We do <strong>not</strong> retain prompt or completion content. When records
        are no longer needed, we delete or anonymize them in the ordinary course.
      </p>

      <h2>8. Security</h2>
      <p>
        The strongest privacy control we offer is structural: because our systems are content-free by
        design, prompt and completion text is not present in our stores to be exposed. In addition, we
        use encryption in transit (TLS) and encryption at rest for the data we hold, together with
        access controls and standard operational safeguards. No method of transmission or storage is
        perfectly secure, but the content-free boundary meaningfully limits what could ever be at
        risk on our systems.
      </p>

      <h2>9. Your rights</h2>
      <p>
        Depending on where you live, you may have rights to access, correct, or delete the personal
        data we hold about you, and to request information about how it is processed. We intend to
        honor these rights and, where applicable, to support portability and objection. To make a
        request, contact us at support@zerorouter.ai; we may need to verify your identity before
        acting. This section describes the rights we intend to offer; specific statutory compliance
        commitments are pending review by counsel.
      </p>

      <h2>10. International transfers</h2>
      <p>
        We operate on cloud infrastructure and work with providers that may process data in countries
        other than yours. Where data is transferred across borders, we intend to rely on appropriate
        safeguards as required by applicable law. [International-transfer mechanisms TBD.]
      </p>

      <h2>11. Children</h2>
      <p>
        The Service is not directed to children and is intended for users who are of the legal age
        required to form a binding contract. We do not knowingly collect personal information from
        children under the age of 18 (or the age defined as a “child” in your jurisdiction). If you
        believe a child has provided us information, contact us and we will take appropriate steps.
      </p>

      <h2>12. Changes</h2>
      <p>
        We may update this Privacy Policy from time to time. We will revise the “Last updated” date
        above and, for material changes, provide reasonable notice.
      </p>

      <h2>13. Contact</h2>
      <p>
        Questions about this Privacy Policy, or requests to exercise your rights, can be sent to{' '}
        support@zerorouter.ai.
      </p>
    </article>
  )
}
