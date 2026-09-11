// What stands between this node and a session it can start, derived from
// state the interface already holds. Not a history: the card comes back if a
// provider is disconnected later, because the node cannot start a session then
// either. A node with a hundred sessions and no credential needs this more
// than a fresh one does.

export interface SetupStep {
  href: string
  title: string
  detail: string
  done: boolean
}

export function setupSteps(s: {
  anyProviderConnected: boolean
  anyChannel: boolean
  boundaryReady: boolean
}): SetupStep[] | null {
  if (s.boundaryReady && s.anyProviderConnected && s.anyChannel) return null
  return [
    {
      href: '/settings#boundary',
      title: 'Prepare this node',
      detail: 'Start the isolated runtime and verify its boundary before connecting a model.',
      done: s.boundaryReady,
    },
    {
      href: '/nodes',
      title: 'Connect a provider',
      detail: 'Sessions cannot start without a model credential.',
      done: s.anyProviderConnected,
    },
    {
      href: '/settings',
      title: 'Name a channel',
      detail: 'Work, credentials, and ceilings are scoped to it. One is plenty to start.',
      done: s.anyChannel,
    },
  ]
}

/** The first step still to do, for pointing the operator at one thing. */
export function nextStep(steps: SetupStep[]): SetupStep {
  return steps.find((s) => !s.done) ?? steps[steps.length - 1]
}
