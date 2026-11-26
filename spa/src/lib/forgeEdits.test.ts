import { describe, expect, test } from 'bun:test'
import { agentForge, fieldList, forgeEdits } from './forgeEdits'

const review = { title: 'Fix it', body: 'It is fixed.' }

describe('forge edits on a review', () => {
  test("the agent's fields are its title and body, and the description and comment it asked for", () => {
    expect(agentForge(review, {})).toEqual({
      title: 'Fix it',
      body: 'It is fixed.',
      describe: false,
      descTitle: '',
      descBody: '',
      commenting: false,
      comment: '',
    })
    expect(agentForge(review, { description: { title: 'T', body: 'B' }, comment: 'C' })).toMatchObject({
      describe: true,
      descTitle: 'T',
      descBody: 'B',
      commenting: true,
      comment: 'C',
    })
  })

  test("nothing is edited while every field is the agent's: no chip", () => {
    const agent = agentForge(review, { comment: 'C' })
    expect(forgeEdits({ ...agent }, agent)).toEqual([])
  })

  test('an edited title or body is named, with the text before and after', () => {
    const agent = agentForge(review, {})
    expect(forgeEdits({ ...agent, body: 'It is fixed properly.' }, agent)).toEqual([
      { field: 'body', before: 'It is fixed.', after: 'It is fixed properly.' },
    ])
    expect(fieldList(forgeEdits({ ...agent, title: 'Fix it now', body: 'x' }, agent))).toBe('title and body')
  })

  test("the description and comment are diffed against the intent's", () => {
    const agent = agentForge(review, { description: { title: 'T', body: 'B' } })
    expect(forgeEdits({ ...agent, descBody: 'B2' }, agent)).toEqual([{ field: 'description', before: 'B', after: 'B2' }])
    // Turned off, what was asked reads as taken out.
    expect(forgeEdits({ ...agent, describe: false }, agent).map((e) => e.field)).toEqual(['description title', 'description'])
  })

  test('text under an unticked box is not an edit; ticking an empty one is', () => {
    const agent = agentForge(review, {})
    expect(forgeEdits({ ...agent, comment: 'unsent' }, agent)).toEqual([])
    expect(forgeEdits({ ...agent, commenting: true }, agent)).toEqual([{ field: 'comment', before: '', after: '', turned: 'on' }])
  })

  test("clearing puts every field back to the agent's, which reads as no edits", () => {
    const agent = agentForge(review, { description: { title: 'T', body: 'B' } })
    const now = { ...agent, title: 'x', body: 'y', describe: false, commenting: true, comment: 'z' }
    expect(forgeEdits(now, agent).length).toBeGreaterThan(0)
    const cleared = { ...now, ...agent }
    expect(forgeEdits(cleared, agent)).toEqual([])
  })

  test('the field list reads as a sentence', () => {
    const e = (field: 'title' | 'body' | 'comment') => ({ field, before: '', after: 'x' })
    expect(fieldList([])).toBe('')
    expect(fieldList([e('title')])).toBe('title')
    expect(fieldList([e('title'), e('body'), e('comment')])).toBe('title, body and comment')
  })
})
