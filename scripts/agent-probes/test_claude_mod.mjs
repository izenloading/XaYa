import assert from 'node:assert/strict'
import { mkdtemp, writeFile, copyFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

async function plugin(t, fetch) {
  const directory = await mkdtemp(path.join(tmpdir(), 'xaya-mod-test-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await writeFile(path.join(directory, 'package.json'), '{"type":"module"}')
  await writeFile(path.join(directory, 'config.js'), 'export const endpoint="http://localhost"; export const token="fake";')
  await copyFile(new URL('./claude_mod.js', import.meta.url), path.join(directory, 'register.js'))
  const { register } = await import(pathToFileURL(path.join(directory, 'register.js')).href)
  const hooks = new Map()
  register((name, handler) => { hooks.set(name, handler) })
  const api = { http: { fetch } }
  return { hooks, api }
}

test('concurrent native events arrive in sequence order', async t => {
  const received = []
  const { hooks, api } = await plugin(t, async (_url, request) => {
    const event = JSON.parse(request.body)
    if (event.sequence === 1) await new Promise(resolve => setTimeout(resolve, 20))
    received.push(event.sequence)
    return { ok: true, text: '{}' }
  })
  await Promise.all([
    hooks.get('turn.start')(api, { turnId: 'one' }, value => value),
    hooks.get('turn.complete')(api, { turnId: 'one' }, value => value),
  ])
  assert.deepEqual(received, [1, 2])
})

test('stream keeps chunk identity and final return value', async t => {
  const { hooks, api } = await plugin(t, async () => ({ ok: true, text: '{}' }))
  const chunk = { kind: 'text', text: 'hello' }
  const final = { usage: 4 }
  const stream = hooks.get('turn.step')(api, { turnId: 'one' }, async function* () {
    yield chunk
    return final
  })
  assert.strictEqual((await stream.next()).value, chunk)
  assert.deepEqual(await stream.next(), { value: final, done: true })
})

test('one failed event does not poison the queue and the loss is visible', async t => {
  const received = []
  let attempts = 0
  const { hooks, api } = await plugin(t, async (_url, request) => {
    if (++attempts === 1) throw new Error('first event lost')
    received.push(JSON.parse(request.body))
    return { ok: true, text: '{}' }
  })
  await hooks.get('turn.start')(api, { turnId: 'one' }, value => value)
  await hooks.get('turn.complete')(api, { turnId: 'one' }, value => value)
  assert.equal(received[0].sequence, 2)
  assert.equal(received[0].missed, 1)
})

test('cancel closes underlying generator even when bridge observation fails', async t => {
  const { hooks, api } = await plugin(t, async () => { throw new Error('bridge offline') })
  let closed = false
  const stream = hooks.get('turn.step')(api, { turnId: 'one' }, async function* () {
    try { yield { kind: 'text', text: 'partial' } } finally { closed = true }
  })
  await stream.next()
  await stream.return('cancelled')
  assert.equal(closed, true)
})

test('pending submit does not block abort polling', async t => {
  const queue = [{ id: 'send', type: 'send', text: 'prompt' }, { id: 'abort', type: 'abort', turnId: 'one' }]
  const acks = []
  let tick, release, aborted = false
  const { hooks, api } = await plugin(t, async (url, request) => {
    if (url.endsWith('/acks')) acks.push(JSON.parse(request.body))
    return { ok: true, text: JSON.stringify(url.endsWith('/poll') ? { command: queue.shift() } : {}) }
  })
  api.session = { id: async () => 'native' }
  api.clock = { every: (_interval, callback) => { tick = callback } }
  api.prompt = { submit: () => new Promise(resolve => { release = resolve }) }
  api.turn = { abort: async () => { aborted = true } }
  await hooks.get('session.start')(api, {}, value => value)
  await tick()
  await tick()
  await new Promise(resolve => setTimeout(resolve, 0))
  assert.equal(aborted, true)
  assert.equal(acks.find(ack => ack.id === 'abort')?.ok, true)
  release({})
  await new Promise(resolve => setTimeout(resolve, 0))
})
