import { expect, test } from 'bun:test'
import { desktopUpdateAction, type UpdateStatus } from './desktop-update'

const running: UpdateStatus = { state: 'current', current_version: '0.9.1' }

test('current and failed statuses check for updates', () => {
  expect(desktopUpdateAction(running)).toEqual({
    command: 'check',
    label: 'Check again',
    disabled: false,
  })
  expect(
    desktopUpdateAction({
      state: 'failed',
      current_version: '0.9.1',
      message: 'GitHub is unavailable',
    }),
  ).toEqual({ command: 'check', label: 'Try again', disabled: false })
})

test('an available release installs through the one update action', () => {
  expect(
    desktopUpdateAction({
      state: 'available',
      current_version: '0.9.1',
      available_version: '0.9.2',
    }),
  ).toEqual({
    command: 'install',
    label: 'Update to v0.9.2 and restart',
    disabled: false,
  })
})

test('transitional states disable the update action', () => {
  expect(
    desktopUpdateAction({ state: 'checking', current_version: '0.9.1' }),
  ).toEqual({ command: null, label: 'Checking…', disabled: true })
  expect(
    desktopUpdateAction({
      state: 'downloading',
      current_version: '0.9.1',
      available_version: '0.9.2',
    }),
  ).toEqual({ command: null, label: 'Installing…', disabled: true })
})

test('package-managed installs offer no updater action', () => {
  expect(
    desktopUpdateAction({ state: 'unsupported', current_version: '0.9.1' }),
  ).toBeNull()
})
