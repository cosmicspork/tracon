import { describe, expect, test } from 'bun:test'

import { tokenFromHash } from './auth'
import { decoderChoice, FRAME_MAX, frameSize } from './qr-decoder'
import { cameraProblem, canScan, classifyScan, MAX_SCAN, scanRefusal } from './scan'
import { loginUrl } from './settings'

const here = { origin: 'https://tracon.example.org' }
const id = 'abcDEF_-0123456789xyz'
const token = 'trc1.AbCdEf-_0123456789'

describe('classifyScan', () => {
  test('a same-origin approval address is an approval', () => {
    expect(classifyScan(`https://tracon.example.org/#approve=${id}`, here)).toEqual({ kind: 'approve', id })
    // The node's approve_url has no trailing slash trouble either way.
    expect(classifyScan(`https://tracon.example.org#approve=${id}`, here)).toEqual({ kind: 'approve', id })
  })

  test('an approval ID too short, too long or with other characters is not one', () => {
    for (const bad of ['short', 'a'.repeat(15), 'a'.repeat(65), 'abc.def/ghi+jkl=mnop', '%2E%2E%2Fxxxxxxxxxxxxxx']) {
      expect(classifyScan(`https://tracon.example.org/#approve=${bad}`, here)).toEqual({ kind: 'unknown' })
    }
  })

  test('a same-origin login address is a login, read as the login QR prints it', () => {
    expect(classifyScan(loginUrl('https://tracon.example.org/', token), here)).toEqual({ kind: 'login', token })
    expect(tokenFromHash(`#token=${token}`)).toBe(token)
    // Only a tracon operator token.
    expect(classifyScan('https://tracon.example.org/#token=ghp_notours', here)).toEqual({ kind: 'unknown' })
  })

  test('a mesh invitation is recognised whatever its origin', () => {
    expect(classifyScan('https://hub.example/#enroll=7KQ4M2XA', here)).toEqual({ kind: 'enroll' })
    expect(classifyScan('https://tracon.example.org/#enroll=7KQ4M2XA', here)).toEqual({ kind: 'enroll' })
    expect(classifyScan('https://hub.example/#enroll=', here)).toEqual({ kind: 'unknown' })
  })

  test('a known code for another origin, scheme or port is for elsewhere', () => {
    for (const origin of ['https://other.example.org', 'http://tracon.example.org', 'https://tracon.example.org:8443']) {
      expect(classifyScan(`${origin}/#approve=${id}`, here)).toEqual({ kind: 'elsewhere', origin })
      expect(classifyScan(`${origin}/#token=${token}`, here)).toEqual({ kind: 'elsewhere', origin })
    }
  })

  test('an extra path, a query or extra fragment parameters are unknown', () => {
    for (const text of [
      `https://tracon.example.org/approve#approve=${id}`,
      `https://tracon.example.org/x/#approve=${id}`,
      `https://tracon.example.org/?next=1#approve=${id}`,
      `https://tracon.example.org/#approve=${id}&x=1`,
      `https://tracon.example.org/#token=${token}&x=1`,
      `https://user:pw@tracon.example.org/#approve=${id}`,
      'https://tracon.example.org/',
      'https://tracon.example.org/#settings',
    ]) {
      expect(classifyScan(text, here)).toEqual({ kind: 'unknown' })
    }
  })

  test('other schemes, bare text, and empty or oversized input are unknown', () => {
    for (const text of [
      `javascript:alert(1)//#approve=${id}`,
      `data:text/html,<a href="#approve=${id}">`,
      `intent://tracon.example.org/#approve=${id}`,
      `ftp://tracon.example.org/#approve=${id}`,
      'BCDF-GHJK',
      id,
      token,
      '',
      '   ',
      `https://tracon.example.org/#approve=${id}`.padEnd(MAX_SCAN + 1, ' ') + 'x',
    ]) {
      expect(classifyScan(text, here)).toEqual({ kind: 'unknown' })
    }
  })

  test('surrounding whitespace is trimmed', () => {
    expect(classifyScan(`  https://tracon.example.org/#approve=${id}\n`, here)).toEqual({ kind: 'approve', id })
  })
})

describe('scanRefusal', () => {
  test('says where a code belongs, as text, and never echoes an unknown one', () => {
    expect(scanRefusal({ kind: 'elsewhere', origin: 'https://other.example.org' })).toBe(
      'This code is for https://other.example.org. Open tracon there to approve it.',
    )
    expect(scanRefusal({ kind: 'enroll' })).toContain('Settings > Mesh')
    expect(scanRefusal({ kind: 'login', token })).toBe("That's a sign-in code. This app is already signed in.")
    expect(scanRefusal({ kind: 'login', token })).not.toContain(token)
    expect(scanRefusal({ kind: 'unknown' })).toBe("That QR code isn't a tracon code.")
  })
})

describe('canScan', () => {
  test('only outside the desktop app, in a secure context, with a camera API', () => {
    expect(canScan({ tauri: false, secure: true, camera: true })).toBe(true)
    expect(canScan({ tauri: true, secure: true, camera: true })).toBe(false)
    expect(canScan({ tauri: false, secure: false, camera: true })).toBe(false)
    expect(canScan({ tauri: false, secure: true, camera: false })).toBe(false)
  })
})

describe('cameraProblem', () => {
  const named = (name: string) => Object.assign(new Error('x'), { name })
  test('words each refusal by its name', () => {
    expect(cameraProblem(named('NotAllowedError'))).toContain('blocked')
    expect(cameraProblem(named('NotFoundError'))).toBe('No camera was found on this device.')
    expect(cameraProblem(named('OverconstrainedError'))).toBe('No camera was found on this device.')
    expect(cameraProblem(named('NotReadableError'))).toContain('in use by another app')
    expect(cameraProblem(new Error('other'))).toBe('The camera could not be opened.')
  })
})

describe('decoderChoice', () => {
  const detector = (formats: string[] | Error) =>
    Object.assign(function () {}, {
      getSupportedFormats: async () => {
        if (formats instanceof Error) throw formats
        return formats
      },
    })

  test('the browser’s own BarcodeDetector when it reads QR codes', async () => {
    expect(await decoderChoice({ BarcodeDetector: detector(['ean_13', 'qr_code']) })).toBe('native')
  })

  test('jsQR when there is no BarcodeDetector', async () => {
    expect(await decoderChoice({})).toBe('jsqr')
  })

  test('jsQR when the BarcodeDetector cannot read QR codes, or will not say', async () => {
    expect(await decoderChoice({ BarcodeDetector: detector([]) })).toBe('jsqr')
    expect(await decoderChoice({ BarcodeDetector: detector(new Error('no')) })).toBe('jsqr')
    expect(await decoderChoice({ BarcodeDetector: function () {} })).toBe('jsqr')
  })
})

describe('frameSize', () => {
  test('scales the longest side down to the limit, never up', () => {
    expect(frameSize(1920, 1080)).toEqual({ width: FRAME_MAX, height: 405 })
    expect(frameSize(1080, 1920)).toEqual({ width: 405, height: FRAME_MAX })
    expect(frameSize(640, 480)).toEqual({ width: 640, height: 480 })
  })
})
