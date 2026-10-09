import { errorText, loadPhase, type LoadPhase, type LoadStatus } from './load'

/**
 * One list request a screen keeps current. The latest request wins, so a
 * slow answer for the channel just left never lands on the one now open. A
 * new key (another channel, another window) shows loading again; the same
 * key refreshing in the background keeps showing the last answer meanwhile,
 * and keeps it if the refresh fails.
 */
export class Loader<T> implements LoadStatus {
  value = $state<T | null>(null)
  loaded = $state(false)
  error = $state<string | null>(null)
  /** A request is in flight; a retry button waits on it. */
  pending = $state(false)
  #generation = 0
  #key: string | null = null
  #request: (() => Promise<T>) | null = null

  get phase(): LoadPhase {
    return loadPhase(this)
  }

  load(key: string, request: () => Promise<T>) {
    const fresh = key !== this.#key
    this.#key = key
    this.#request = request
    this.#run(fresh)
  }

  /** Ask again; rows already shown stay until the answer replaces them. */
  retry() {
    this.#run(!this.loaded)
  }

  #run(fresh: boolean) {
    const request = this.#request
    if (!request) return
    const generation = ++this.#generation
    if (fresh) {
      this.value = null
      this.loaded = false
      this.error = null
    }
    this.pending = true
    request().then(
      (value) => {
        if (generation !== this.#generation) return
        this.value = value
        this.loaded = true
        this.error = null
        this.pending = false
      },
      (e) => {
        if (generation !== this.#generation) return
        this.error = errorText(e)
        this.pending = false
      },
    )
  }
}
