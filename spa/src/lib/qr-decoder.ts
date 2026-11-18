// Reading a QR code off a camera frame. A browser with its own
// `BarcodeDetector` that reads QR codes (Chromium on Android, ChromeOS and
// macOS) uses it; every other one (iOS, Firefox, Chromium on Windows and
// Linux) uses jsQR, bundled as its own chunk and served by the node, so
// nothing is fetched from elsewhere. Frames are read and dropped: never kept,
// logged or sent.

/** A decoder for one video frame: the text of a QR code in it, or null. */
export type FrameDecoder = (video: HTMLVideoElement) => Promise<string | null>

interface DetectorLike {
  detect(source: HTMLVideoElement): Promise<{ rawValue: string }[]>
}
interface DetectorClass {
  new (options: { formats: string[] }): DetectorLike
  getSupportedFormats?: () => Promise<string[]>
}

/** Which decoder this browser gets: its own `BarcodeDetector` when it has
    one that reads QR codes (Chromium on Android, ChromeOS and macOS), else
    jsQR, bundled. */
export async function decoderChoice(win: object): Promise<'native' | 'jsqr'> {
  const Detector = (win as { BarcodeDetector?: DetectorClass }).BarcodeDetector
  if (!Detector || typeof Detector.getSupportedFormats !== 'function') return 'jsqr'
  try {
    const formats = await Detector.getSupportedFormats()
    return formats.includes('qr_code') ? 'native' : 'jsqr'
  } catch {
    return 'jsqr'
  }
}

/** The longest side a frame is scaled to before jsQR reads it. */
export const FRAME_MAX = 720

/** A frame's size for decoding: the video's, scaled down to at most
    {@link FRAME_MAX} on its longest side. */
export function frameSize(width: number, height: number): { width: number; height: number } {
  const scale = Math.min(1, FRAME_MAX / Math.max(width, height, 1))
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) }
}

/** The decoder for this browser. jsQR is loaded only when needed, as its own
    chunk from this origin. */
export async function frameDecoder(win: Window = window): Promise<FrameDecoder> {
  if ((await decoderChoice(win)) === 'native') {
    const Detector = (win as unknown as { BarcodeDetector: DetectorClass }).BarcodeDetector
    const detector = new Detector({ formats: ['qr_code'] })
    return async (video) => {
      const codes = await detector.detect(video)
      return codes[0]?.rawValue ?? null
    }
  }
  const { default: jsQR } = await import('jsqr')
  const canvas = document.createElement('canvas')
  const ctx = canvas.getContext('2d', { willReadFrequently: true })
  return async (video) => {
    if (!ctx || !video.videoWidth || !video.videoHeight) return null
    const { width, height } = frameSize(video.videoWidth, video.videoHeight)
    if (canvas.width !== width) canvas.width = width
    if (canvas.height !== height) canvas.height = height
    ctx.drawImage(video, 0, 0, width, height)
    const frame = ctx.getImageData(0, 0, width, height)
    return jsQR(frame.data, width, height, { inversionAttempts: 'attemptBoth' })?.data ?? null
  }
}
