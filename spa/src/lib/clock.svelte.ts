// A single ticking clock. Ages and expiry countdowns read `clock.now` so they
// advance on their own — an expiry that says "expires 4m" is denied-by-default
// when it lapses, so it must not freeze until an unrelated frame re-renders it.

class Clock {
  now = $state(Date.now())

  constructor() {
    // One interval for the whole app, not one per card.
    setInterval(() => (this.now = Date.now()), 1000)
  }
}

export const clock = new Clock()
