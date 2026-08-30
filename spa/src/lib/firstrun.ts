// The three steps between a fresh node and its first session, derived from
// state the interface already holds. Gone for good once any session exists —
// the checklist is an on-ramp, not a status panel.

export interface FirstRunStep {
  href: string
  title: string
  detail: string
  done: boolean
}

export function firstRunSteps(s: {
  anyProviderConnected: boolean
  anyReadyWork: boolean
  anySession: boolean
}): FirstRunStep[] | null {
  if (s.anySession) return null
  return [
    {
      href: '/nodes',
      title: 'Connect a provider',
      detail: 'Sessions cannot start without a model credential.',
      done: s.anyProviderConnected,
    },
    {
      href: '/work',
      title: 'Add a work item',
      detail: 'A session is a phase of one item from the ready list.',
      done: s.anyReadyWork,
    },
    {
      href: '/new',
      title: 'Start a plan session',
      detail: 'It reads, thinks, and ends by writing the plan document.',
      done: false,
    },
  ]
}

/** The first step still to do, for pointing the operator at one thing. */
export function nextStep(steps: FirstRunStep[]): FirstRunStep {
  return steps.find((s) => !s.done) ?? steps[steps.length - 1]
}
