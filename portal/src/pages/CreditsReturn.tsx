// The page Stripe returns the browser to after an embedded checkout attempt.
//
// Stripe substitutes the real session id into the `{CHECKOUT_SESSION_ID}`
// template variable of the `return_url` this deployment sent, so the id
// arrives as `?session_id=`. All this route does is ask the server what
// happened and hand the answer back to the Credits page.
//
// It does not, and must not, credit anything. `complete` here means "Stripe
// took the payment"; the balance moves only when the signed
// `checkout.session.completed` webhook is processed. A customer who never
// loads this page is still credited, and a customer who forges its answer
// changes nothing but their own screen.

import { useEffect } from 'react'
import { useNavigate, useSearchParams } from 'react-router'
import { api } from '../api'
import { Loading } from '../ui'

export function CreditsReturn() {
  const [searchParams] = useSearchParams()
  const navigate = useNavigate()
  const sessionId = searchParams.get('session_id')

  useEffect(() => {
    let active = true
    // Land back on /credits with the outcome; that page owns every purchase
    // notice, so there is exactly one place that renders them. `replace` keeps
    // the return url out of history — going Back should not re-run a
    // completed checkout.
    const settle = (outcome: string) => {
      if (active) navigate(`/credits?checkout=${outcome}`, { replace: true })
    }
    if (sessionId === null) {
      settle('unknown')
      return
    }
    api
      .checkoutStatus(sessionId)
      .then((result) => {
        if (result.status === 'complete') settle('success')
        else if (result.status === 'open') settle('retry')
        else if (result.status === 'expired') settle('expired')
        else settle('unknown')
      })
      // A failed status read says nothing about whether the payment
      // succeeded, so it must not be reported as either. 'unknown' points the
      // customer at the ledger, which is the actual record.
      .catch(() => settle('unknown'))
    return () => {
      active = false
    }
  }, [sessionId, navigate])

  return (
    <div className="page">
      <Loading label="Confirming your payment" />
    </div>
  )
}
