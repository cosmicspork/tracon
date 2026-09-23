// What stands between this node itself and a session it can start. Home uses
// actual target eligibility to choose between this checklist and the composer.
// A ready peer is a separate task path; this checklist describes local setup.

export interface SetupStep {
  href: string
  title: string
  detail: string
  done: boolean
}

export function setupSteps(s: {
  anyProviderConnected: boolean
  modelOffered: boolean
  anyChannel: boolean
  memberChannel: boolean
  boundaryReady: boolean
}): SetupStep[] {
  return [
    {
      href: '/settings#maintenance',
      title: 'Prepare this node',
      detail: 'Start the isolated runtime and verify its boundary before connecting a model.',
      done: s.boundaryReady,
    },
    {
      href: '/settings#connections',
      title: 'Connect a provider',
      detail: 'Sessions run here need a model credential on this node.',
      done: s.anyProviderConnected,
    },
    {
      href: '/settings#channels',
      title: 'Name a channel',
      detail: 'Work, credentials, and ceilings are scoped to it.',
      done: s.anyChannel,
    },
    {
      href: '/settings#channels',
      title: 'Make this node a channel member',
      detail: 'Create a channel here, or give this node membership in an existing open channel.',
      done: s.memberChannel,
    },
    {
      href: '/settings#connections',
      title: 'Offer a model',
      detail:
        s.anyProviderConnected && !s.modelOffered
          ? 'Declare models for the provider and check its channel scope. Refresh only if a declared model has not appeared.'
          : 'A connected provider must offer a model to a channel this node belongs to.',
      done: s.modelOffered,
    },
  ]
}

/** The first step still to do, for pointing the operator at one thing. */
export function nextStep(steps: SetupStep[]): SetupStep {
  return steps.find((s) => !s.done) ?? steps[steps.length - 1]
}
