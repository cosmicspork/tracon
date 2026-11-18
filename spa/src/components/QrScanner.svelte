<script lang="ts">
  // A QR scanner inside the app: the installed PWA reads a code itself
  // instead of the phone's camera app opening it in the system browser, which
  // has neither this app's session nor its passkey (iOS keeps them apart).
  //
  // The button and the sheet are one component so the camera is asked for in
  // the tap itself, the user gesture iOS wants. Only a code `lib/scan.ts`
  // recognises, of a kind the parent accepts, is handed on; the parent opens
  // a view for it. Anything else gets a plain refusal. The camera is released
  // whenever the sheet is not showing: closed, hidden, navigated away from,
  // or unmounted. Frames and decoded text are never kept, logged or sent.
  import { onDestroy, tick } from 'svelte'

  import { frameDecoder, type FrameDecoder } from '../lib/qr-decoder'
  import { cameraProblem, classifyScan, scanRefusal, type ScanKind, type ScanResult } from '../lib/scan'

  let {
    label,
    accept,
    onresult,
  }: {
    label: string
    accept: ScanKind[]
    onresult: (result: ScanResult) => void
  } = $props()

  /** Between attempts to read a frame. */
  const EVERY_MS = 120

  let open = $state(false)
  let status = $state('')
  let problem = $state<string | null>(null)
  let refused = $state<string | null>(null)
  let video = $state<HTMLVideoElement>()

  let stream: MediaStream | null = null
  let timer: ReturnType<typeof setTimeout> | undefined
  /** Bumped by every start and stop, so work from an earlier one stops. */
  let run = 0

  function start() {
    stopCamera()
    const mine = run
    open = true
    problem = null
    refused = null
    status = 'Starting the camera…'
    let asked: Promise<MediaStream>
    try {
      // Called in the tap's own handler, before anything is awaited.
      asked = navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: 'environment' } }, audio: false })
    } catch (err) {
      problem = cameraProblem(err)
      return
    }
    asked.then(
      async (s) => {
        if (mine !== run) {
          for (const t of s.getTracks()) t.stop()
          return
        }
        stream = s
        await tick()
        if (mine !== run || !video) return stopCamera()
        video.srcObject = s
        void video.play().catch(() => {
          /* autoplay and muted: it plays without this on most browsers */
        })
        let decode: FrameDecoder
        try {
          decode = await frameDecoder()
        } catch {
          if (mine === run) {
            stopCamera()
            problem = 'The QR reader could not be loaded. Reload the page and try again.'
          }
          return
        }
        if (mine !== run) return
        status = 'Point the camera at the QR code.'
        next(decode, mine)
      },
      (err) => {
        if (mine === run) problem = cameraProblem(err)
      },
    )
  }

  function next(decode: FrameDecoder, mine: number) {
    timer = setTimeout(async () => {
      if (mine !== run || !video) return
      let text: string | null = null
      try {
        if (video.readyState >= 2) text = await decode(video)
      } catch {
        /* an unreadable frame: try the next */
      }
      if (mine !== run) return
      if (text === null) return next(decode, mine)
      const result = classifyScan(text, { origin: location.origin })
      stopCamera()
      if ((accept as string[]).includes(result.kind)) {
        open = false
        onresult(result)
      } else {
        refused = scanRefusal(result)
      }
    }, EVERY_MS)
  }

  function stopCamera() {
    run += 1
    clearTimeout(timer)
    timer = undefined
    if (stream) for (const t of stream.getTracks()) t.stop()
    stream = null
    if (video) video.srcObject = null
  }

  function close() {
    stopCamera()
    open = false
    problem = null
    refused = null
  }

  // Hidden or navigated away from: let go of the camera.
  $effect(() => {
    if (!open) return
    const onVisibility = () => {
      if (document.visibilityState === 'hidden') close()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    document.addEventListener('visibilitychange', onVisibility)
    window.addEventListener('popstate', close)
    window.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('visibilitychange', onVisibility)
      window.removeEventListener('popstate', close)
      window.removeEventListener('keydown', onKey)
    }
  })

  onDestroy(stopCamera)
</script>

<button class="btn scan" type="button" onclick={start} data-testid="qr-scan">{label}</button>

{#if open}
  <div class="sheet" role="dialog" aria-modal="true" aria-label={label} data-testid="qr-scanner">
    <div class="view">
      <!-- svelte-ignore a11y_media_has_caption -->
      <video bind:this={video} playsinline muted autoplay class:off={problem !== null || refused !== null}></video>
      {#if problem === null && refused === null}<span class="frame" aria-hidden="true"></span>{/if}
    </div>
    <div class="bar">
      {#if problem}
        <p class="err" role="alert" data-testid="qr-scan-problem">{problem}</p>
      {:else if refused}
        <p class="err" role="alert" data-testid="qr-scan-refusal">{refused}</p>
      {:else}
        <p aria-live="polite" data-testid="qr-scan-status">{status}</p>
      {/if}
      <div class="acts">
        {#if problem || refused}
          <button class="btn p" type="button" onclick={start}>Scan again</button>
        {/if}
        <button class="btn" type="button" onclick={close} data-testid="qr-scan-close">Close</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .scan {
    white-space: nowrap;
  }
  .sheet {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    grid-template-rows: minmax(0, 1fr) auto;
    background: #000;
    color: #fff;
    font: 14px/1.5 var(--sans);
    letter-spacing: 0;
  }
  .view {
    position: relative;
    min-height: 0;
    display: grid;
    place-items: center;
    overflow: hidden;
  }
  video {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  video.off {
    visibility: hidden;
  }
  .frame {
    position: absolute;
    width: min(64vw, 64vh, 320px);
    aspect-ratio: 1;
    border: 2px solid rgba(255, 255, 255, 0.85);
    border-radius: 12px;
    box-shadow: 0 0 0 100vmax rgba(0, 0, 0, 0.35);
  }
  .bar {
    display: grid;
    gap: 12px;
    padding: 16px 16px calc(16px + env(safe-area-inset-bottom));
    background: var(--s1);
    color: var(--ink);
  }
  p {
    margin: 0;
    color: var(--ink2);
    font-size: 14px;
  }
  p.err {
    color: var(--ink);
  }
  .acts {
    display: flex;
    gap: 10px;
    justify-content: flex-end;
  }
</style>
