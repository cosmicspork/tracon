// What stands between this node itself and a session it can start. A ready
// peer is a separate, valid task path and must not be hidden behind this local
// setup state. The card comes back if a local provider is disconnected later.

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
  boundaryReady: boolean
}): SetupStep[] | null {
  if (s.boundaryReady && s.anyProviderConnected && s.modelOffered && s.anyChannel) return null
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
      href: '/settings#connections',
      title: 'Offer a model',
      // Connected and still not offering is the confusing state, and it has two
      // causes: the catalogue was never probed, or the credential is not scoped
      // to a live channel. Refreshing fixes the first and says nothing about the
      // second, so name both rather than send the operator round one loop twice.
      detail:
        s.anyProviderConnected && !s.modelOffered
          ? 'Connected, but no model reaches a live channel. Refresh models, or check the credential’s channel scope.'
          : 'A connected provider must offer a model before this node can accept a task.',
      done: s.modelOffered,
    },
    {
      href: '/settings#channels',
      title: 'Name a channel',
      detail: 'Work, credentials, and ceilings are scoped to it. One is enough to start.',
      done: s.anyChannel,
    },
  ]
}

/** The first step still to do, for pointing the operator at one thing. */
export function nextStep(steps: SetupStep[]): SetupStep {
  return steps.find((s) => !s.done) ?? steps[steps.length - 1]
}
