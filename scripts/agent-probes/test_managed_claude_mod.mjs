import assert from 'node:assert/strict'
import { mkdtemp, writeFile, copyFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

async function plugin(t, fetch) {
  const directory = await mkdtemp(path.join(tmpdir(), 'xaya-managed-mod-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await writeFile(path.join(directory, 'package.json'), '{"type":"module"}')
  await writeFile(path.join(directory, 'config.js'), 'export const endpoint="http://localhost"; export const token="fake";')
  await copyFile(new URL('../../xaya_agentd/src/sessions/claude_mod.js', import.meta.url), path.join(directory, 'register.js'))
  const { register } = await import(pathToFileURL(path.join(directory, 'register.js')).href)
  const hooks = new Map()
  const timers = []
  register((name, handler) => hooks.set(name, handler))
  return { hooks, timers, api: { http: { fetch }, session: { id: async () => 'native' },
    clock: { every: (_interval, handler) => timers.push(handler) } } }
}

test('production Mods dispatch a turn-bound cancellation once and acknowledge without inferring completion', async t => {
  const requests = []
  let offered = false
  const command = { command_id: 'cancel-one', operation: 'cancel', turn_id: 'main' }
  const { hooks, timers, api } = await plugin(t, async (url, request) => {
    requests.push({ url, body: JSON.parse(request.body) })
    const response = url.endsWith('/control/poll') && !offered ? { command } : {}
    if (url.endsWith('/control/poll')) offered = true
    return { ok: true, text: JSON.stringify(response) }
  })
  const aborted = []
  api.turn = { abort: async value => { aborted.push(value) } }
  await hooks.get('session.start')(api, {}, value => value)
  await hooks.get('turn.start')(api, { turnId: 'main' }, value => value)
  assert.equal(timers.length, 1, 'control polling must be installed outside native turn hooks')
  await timers[0]()
  await timers[0]()
  assert.deepEqual(aborted, [{ turnId: 'main' }])
  const ack = requests.find(request => request.url.endsWith('/control/ack'))?.body
  assert.deepEqual(ack, { sessionId: 'native', command_id: 'cancel-one', turn_id: 'main', status: 'succeeded' })
  assert(!requests.some(request => request.body.type === 'turn.complete'), 'only the engine ends a turn')
})

test('production Mods reject stale cancellation and preserve unknown native outcomes', async t => {
  for (const outcome of ['stale', 'throw', 'malformed']) {
    let ack
    const { hooks, timers, api } = await plugin(t, async (url, request) => {
      if (url.endsWith('/control/ack')) ack = JSON.parse(request.body)
      return { ok: true, text: JSON.stringify({ command: {
        command_id: 'cancel-one', operation: 'cancel', turn_id: outcome === 'stale' ? 'old' : 'main',
      } }) }
    })
    api.turn = { abort: async () => {
      assert.notEqual(outcome, 'stale', 'stale cancellation must not reach the native API')
      if (outcome === 'throw') throw new Error('transport lost')
      return { unexpected: true }
    } }
    await hooks.get('session.start')(api, {}, value => value)
    await hooks.get('turn.start')(api, { turnId: 'main' }, value => value)
    assert.equal(timers.length, 1)
    await timers[0]()
    assert.equal(ack?.status, outcome === 'stale' ? 'failed' : 'unknown')
  }
})

test('concurrent polls and lost acknowledgements do not replay cancellation or poison observations', async t => {
  let polls = 0
  let aborts = 0
  let release
  const observations = []
  const held = new Promise(resolve => { release = resolve })
  const { hooks, timers, api } = await plugin(t, async (url, request) => {
    if (url.endsWith('/events')) observations.push(JSON.parse(request.body))
    if (url.endsWith('/control/ack')) throw new Error('ack lost')
    if (url.endsWith('/control/poll')) {
      polls++
      await held
      return { ok: true, text: JSON.stringify(polls === 1 ? { command: {
        command_id: 'cancel-once', operation: 'cancel', turn_id: 'main',
      } } : {}) }
    }
    return { ok: true, text: '{}' }
  })
  api.turn = { abort: async () => { aborts++ } }
  await hooks.get('session.start')(api, {}, value => value)
  await hooks.get('turn.start')(api, { turnId: 'main' }, value => value)
  const first = timers[0]()
  await timers[0]()
  assert.equal(polls, 1)
  release()
  await first
  await timers[0]()
  await hooks.get('turn.complete')(api, { turnId: 'main' }, value => value)
  assert.equal(aborts, 1)
  assert(observations.every(event => event.missed === 0))
  assert.deepEqual(observations.map(event => event.sequence), [1, 2, 3])
})

test('child boundaries and late completions cannot change the cancellation target', async t => {
  const aborted = []
  const { hooks, timers, api } = await plugin(t, async url => ({ ok: true,
    text: JSON.stringify(url.endsWith('/control/poll') ? { command: {
      command_id: 'cancel-main', operation: 'cancel', turn_id: 'main',
    } } : {}),
  }))
  api.turn = { abort: async value => { aborted.push(value) } }
  await hooks.get('session.start')(api, {}, value => value)
  for (const [name, event] of [
    ['turn.start', { turnId: 'main' }], ['turn.start', { turnId: 'child', agentId: 'child' }],
    ['turn.complete', { turnId: 'child', agentId: 'child' }], ['turn.complete', { turnId: 'old' }],
  ]) await hooks.get(name)(api, event, value => value)
  await timers[0]()
  assert.deepEqual(aborted, [{ turnId: 'main' }])
})

test('production Mods authenticate observations and preserve native identity and order', async t => {
  const received = []
  const { hooks, api } = await plugin(t, async (url, request) => {
    assert.equal(url, 'http://localhost/events')
    assert.equal(request.headers['X-Xaya-Token'], 'fake')
    const value = JSON.parse(request.body)
    if (value.sequence === 1) await new Promise(resolve => setTimeout(resolve, 20))
    received.push(value)
    return { ok: true }
  })
  await hooks.get('session.start')(api, {}, value => value)
  await Promise.all([
    hooks.get('turn.start')(api, { turnId: 'main' }, value => value),
    hooks.get('turn.complete')(api, { turnId: 'child', agentId: 'agent' }, value => value),
  ])
  assert.deepEqual(received.map(v => v.sequence), [1, 2, 3])
  assert(received.every(v => v.sessionId === 'native'))
  assert.equal(received[2].data.agentId, 'agent')
})

test('production Mods redeliver after a transient transport loss without changing native result', async t => {
  const received = []
  let attempts = 0
  const { hooks, api } = await plugin(t, async (_url, request) => {
    if (++attempts === 1) throw new Error('offline')
    received.push(JSON.parse(request.body))
    return { ok: true }
  })
  const result = { native: true }
  assert.strictEqual(await hooks.get('session.start')(api, {}, () => result), result)
  await hooks.get('turn.start')(api, { turnId: 'main' }, value => value)
  // A failed attempt is not yet a loss; the start is resent first, then the turn.
  assert.deepEqual(received.map(event => [event.sequence, event.missed]), [[1, 0], [2, 0]])
})

test('production Mods keep chunk identity and generator return, observing only text', async t => {
  const received = []
  const { hooks, api } = await plugin(t, async (_url, request) => {
    received.push(JSON.parse(request.body))
    return { ok: true }
  })
  const chunk = { kind: 'text', text: '界🙂'.repeat(4000) }
  const engine = { kind: 'engine' }
  const final = { usage: 4 }
  const stream = hooks.get('turn.step')(api, { turnId: 'main', agentId: 'child' }, async function* () {
    yield chunk
    yield engine
    return final
  })
  assert.strictEqual((await stream.next()).value, chunk)
  assert.strictEqual((await stream.next()).value, engine)
  assert.deepEqual(await stream.next(), { value: final, done: true })
  assert.equal(received.length, 1)
  assert.equal(received[0].data.agentId, 'child')
  assert.equal(received[0].data.chunk.text, chunk.text)
})

test('production Mods cancel underlying stream when consumer exits', async t => {
  let closed = false
  const { hooks, api } = await plugin(t, async () => ({ ok: false }))
  const stream = hooks.get('turn.step')(api, { turnId: 'main' }, async function* () {
    try { yield { kind: 'text', text: 'one' }; yield { kind: 'text', text: 'two' } }
    finally { closed = true }
  })
  await stream.next()
  await stream.return()
  assert.equal(closed, true)
})

test('production Mods preserve tool result and native exceptions', async t => {
  const { hooks, api } = await plugin(t, async () => ({ ok: true }))
  const result = { native: 'result' }
  assert.strictEqual(await hooks.get('tool.call')(api, {}, async () => result), result)
  const failure = new Error('native tool failed')
  await assert.rejects(hooks.get('tool.call')(api, {}, async () => { throw failure }), error => error === failure)
})


test('production Mods reject native and program prompts when admission fails', async t => {
  for (const offline of [false, true]) {
    const { hooks, api } = await plugin(t, async () => {
      if (offline) throw new Error('offline')
      return { ok: false }
    })
    assert(hooks.has('prompt.submit'), 'native and program prompts need the same guard')
    let called = false
    const result = await hooks.get('prompt.submit')(api, { text: 'write' }, () => { called = true })
    assert.equal(called, false)
    assert.equal(typeof result.drop, 'string')
    assert(result.drop.includes('XaYa'))
  }
})

test('production Mods deny tools before execution when service is unavailable', async t => {
  const { hooks, api } = await plugin(t, async () => { throw new Error('offline') })
  let called = false
  const result = await hooks.get('tool.call')(api,
    { tool: 'Bash', tool_use_id: 'tool-one', agentId: 'child' }, () => { called = true })
  assert.equal(called, false)
  assert.equal(typeof result.deny, 'string')
})

test('production Mods register actual tool identity before execution and retain native task results', async t => {
  const received = []
  const { hooks, api } = await plugin(t, async (_url, request) => {
    received.push(JSON.parse(request.body))
    return { ok: true }
  })
  const result = { result: { backgroundTaskId: 'task-real' } }
  await hooks.get('tool.call')(api, { tool: 'Bash', tool_use_id: 'tool-real', agentId: 'child' }, () => {
    assert.equal(received.at(-1).type, 'tool.start')
    assert.equal(received.at(-1).data.tool_use_id, 'tool-real')
    return result
  })
  assert.equal(received.at(-1).data.result.result.backgroundTaskId, 'task-real')
  assert.equal(received.at(-1).data.agentId, 'child')
})


test('admission denials keep observation sequence healthy for draining tools', async t => {
  const received = []
  const { hooks, api } = await plugin(t, async (_url, request) => {
    const event = JSON.parse(request.body)
    received.push(event)
    return event.type === 'input.start' ? { ok: false, status: 409 } : { ok: true }
  })
  const result = await hooks.get('prompt.submit')(api, { text: 'closed' }, () => assert.fail('denied'))
  assert(result.drop)
  await hooks.get('turn.complete')(api, { turnId: 'old' }, value => value)
  assert.equal(received[1].missed, 0)
  assert.equal(received[1].sequence, 2)
})

test('lifecycle frames exclude large tool contents while native values remain unchanged', async t => {
  const frames = []
  const { hooks, api } = await plugin(t, async (_url, request) => {
    frames.push(request.body)
    return { ok: true }
  })
  const content = 'x'.repeat(100000)
  const result = { result: { content, backgroundTaskId: 'task-real' }, text: content }
  const returned = await hooks.get('tool.call')(api,
    { tool: 'Write', tool_use_id: 'large', content }, () => result)
  assert.strictEqual(returned, result)
  assert(frames.every(frame => frame.length < 2048), 'lifecycle must fit the control transport')
  assert.equal(JSON.parse(frames[1]).data.result.result.backgroundTaskId, 'task-real')
})

async function sendPlugin(t, submit) {
  const acks = []
  const offered = []
  const { hooks, timers, api } = await plugin(t, async (url, request) => {
    if (url.endsWith('/control/ack')) acks.push(JSON.parse(request.body))
    const command = url.endsWith('/control/poll') ? offered.shift() : undefined
    return { ok: true, text: JSON.stringify(command ? { command } : {}) }
  })
  api.prompt = { submit }
  api.turn = { abort: async () => undefined }
  await hooks.get('session.start')(api, {}, value => value)
  return { acks, offered, timers }
}

const settle = () => new Promise(resolve => setImmediate(resolve))

test('production Mods submit a program message once as user input and report submission', async t => {
  const submitted = []
  const { acks, offered, timers } = await sendPlugin(t, async value => { submitted.push(value) })
  offered.push({ command_id: 'send-one', operation: 'send', text: 'program message' })
  await timers[0]()
  await settle()
  await timers[0]()
  await settle()
  assert.deepEqual(submitted, [{ text: 'program message', asUser: true }])
  assert.deepEqual(acks, [{ sessionId: 'native', command_id: 'send-one', status: 'submitted' }])
})

test('production Mods report a dropped or failed submit and never retry it', async t => {
  for (const [outcome, expected] of [['drop', 'dropped'], ['throw', 'unknown']]) {
    let calls = 0
    const { acks, offered, timers } = await sendPlugin(t, async () => {
      calls++
      if (outcome === 'throw') throw new Error('native submit failed')
      return { drop: 'Claude stopped; the prompt was not submitted' }
    })
    offered.push({ command_id: 'send-one', operation: 'send', text: 'hello' })
    await timers[0]()
    await settle()
    await timers[0]()
    await settle()
    assert.equal(calls, 1)
    assert.equal(acks.length, 1)
    assert.equal(acks[0].status, expected)
    if (expected === 'dropped') assert.equal(acks[0].reason, 'Claude stopped; the prompt was not submitted')
  }
})

test('a pending submit does not block polling, so cancellation still arrives', async t => {
  let release
  const held = new Promise(resolve => { release = resolve })
  const { acks, offered, timers } = await sendPlugin(t, () => held)
  offered.push({ command_id: 'send-one', operation: 'send', text: 'long' })
  await timers[0]()
  offered.push({ command_id: 'cancel-one', operation: 'cancel', turn_id: 'none' })
  await timers[0]()
  assert.equal(offered.length, 0, 'the second poll ran while submit was pending')
  assert.deepEqual(acks.map(ack => ack.command_id), ['cancel-one'])
  release()
  await settle()
  assert.deepEqual(acks.map(ack => ack.command_id), ['cancel-one', 'send-one'])
})

test('malformed send commands never reach the native prompt', async t => {
  const { acks, offered, timers } = await sendPlugin(t, async () => assert.fail('must not submit'))
  offered.push({ command_id: 'bad', operation: 'send', text: 7 })
  await timers[0]()
  await settle()
  assert.deepEqual(acks, [])
})

test('observations cancelled with an aborted turn are redelivered in order, never lost', async t => {
  // Real Claude 2.1.326: Esc rejects every $.http.fetch started inside the aborted turn's
  // hooks with "user-cancel", whichever $ it goes through. Later hooks and clock ticks work.
  const received = []
  const { hooks, timers, api } = await plugin(t, async (_url, request) => {
    const body = JSON.parse(request.body)
    if (body.type) received.push(body)
    return { ok: true, text: '{}' }
  })
  const cancelled = { ...api, http: { fetch: async () => { throw new Error('$.http.fetch failed: user-cancel') } } }
  await hooks.get('session.start')(api, {}, value => value)
  await hooks.get('turn.start')(cancelled, { turnId: 'aborted' }, value => value)
  let ran = false
  const dropped = await hooks.get('prompt.submit')(cancelled, { text: 'lost with the turn' }, () => { ran = true })
  assert.equal(ran, false, 'an undelivered admission fails closed in its own hook')
  assert.equal(typeof dropped.drop, 'string')
  await timers[0]()
  await hooks.get('turn.complete')(api, { turnId: 'aborted', isAborted: true }, value => value)
  await hooks.get('prompt.submit')(api, { text: 'next' }, () => { ran = true })
  assert.equal(ran, true)
  assert.deepEqual(received.map(event => [event.type, event.sequence, event.missed, event.late ?? false]), [
    ['session.start', 1, 0, false], ['turn.start', 2, 0, true], ['input.start', 3, 0, true],
    ['turn.complete', 4, 0, false], ['input.start', 5, 0, false],
  ])
})

test('a hook delivers queued observations first, then its own, preserving order', async t => {
  const received = []
  const { hooks, api } = await plugin(t, async (_url, request) => {
    received.push(JSON.parse(request.body).sequence)
    return { ok: true, text: '{}' }
  })
  const cancelled = { ...api, http: { fetch: async () => { throw new Error('user-cancel') } } }
  await hooks.get('session.start')(api, {}, value => value)
  await hooks.get('turn.start')(cancelled, { turnId: 'one' }, value => value)
  await hooks.get('turn.complete')(api, { turnId: 'one' }, value => value)
  assert.deepEqual(received, [1, 2, 3])
})

test('observations that stay undeliverable are finally reported as missed', async t => {
  let offline = false
  const received = []
  const { hooks, timers, api } = await plugin(t, async (_url, request) => {
    const body = JSON.parse(request.body)
    if (offline && body.type) throw new Error('connection refused')
    if (body.type) received.push(body)
    return { ok: true, text: '{}' }
  })
  await hooks.get('session.start')(api, {}, value => value)
  offline = true
  const now = Date.now
  t.after(() => { Date.now = now })
  await hooks.get('turn.start')(api, { turnId: 'one' }, value => value)
  Date.now = () => now() + 6000
  await timers[0]()
  offline = false
  await hooks.get('turn.complete')(api, { turnId: 'one' }, value => value)
  assert.deepEqual(received.map(event => [event.type, event.missed]), [['session.start', 0], ['turn.complete', 1]])
})

test('a restarted service gets one consistent snapshot, then the stream continues after it', async t => {
  let phase = 'up'
  const syncs = []
  const delivered = []
  const { hooks, timers, api } = await plugin(t, async (url, request) => {
    const body = JSON.parse(request.body)
    if (phase === 'down') throw new Error('service down')
    if (url.endsWith('/sync')) {
      syncs.push(body)
      const first = phase === 'restarted'
      phase = 'joined'
      return first ? { ok: true, status: 200, text: '{}' } : { ok: false, status: 409, text: '{}' }
    }
    if (phase === 'restarted') return { ok: false, status: 428, text: '{}' }
    if (url.endsWith('/events')) delivered.push(body)
    return { ok: true, status: 200, text: '{}' }
  })
  await hooks.get('session.start')(api, {}, value => value)
  let finish
  const crossing = hooks.get('tool.call')(api, { tool: 'Bash', tool_use_id: 'crossing' },
    () => new Promise(resolve => { finish = resolve }))
  await hooks.get('turn.start')(api, { turnId: 'main' }, value => value)
  phase = 'down'
  const denied = await hooks.get('tool.call')(api, { tool: 'Bash', tool_use_id: 'while-down' },
    () => assert.fail('no service: the tool must not run'))
  assert.equal(typeof denied.deny, 'string')
  phase = 'restarted'
  await timers[0]()
  await new Promise(resolve => setTimeout(resolve, 10))
  assert.equal(syncs.length >= 1, true)
  assert.deepEqual(syncs[0], { sessionId: 'native', sequence: 4, activeTurn: 'main', interrupted: false,
    tools: [{ tool: 'Bash', tool_use_id: 'crossing' }], tasks: [],
    xayaGateVersion: 1, xayaControlVersion: 1, xayaSyncVersion: 1 })
  // The undeliverable denial is superseded, not resent; the stream continues after sequence 4.
  delivered.length = 0
  finish({ result: { backgroundTaskId: 'task-after' } })
  await crossing
  await hooks.get('turn.complete')(api, { turnId: 'main' }, value => value)
  assert.deepEqual(delivered.map(event => [event.sequence, event.type, event.missed]),
    [[5, 'tool.complete', 0], [6, 'turn.complete', 0]])
})

test('a snapshot counts only admitted, unfinished tools and launched background tasks', async t => {
  let restarted = false
  const syncs = []
  const { hooks, timers, api } = await plugin(t, async (url, request) => {
    const body = JSON.parse(request.body)
    if (url.endsWith('/sync')) { syncs.push(body); return { ok: true, status: 200, text: '{}' } }
    if (restarted && syncs.length === 0) return { ok: false, status: 428, text: '{}' }
    if (body.type === 'tool.start' && body.data.tool_use_id === 'refused') return { ok: false, status: 409 }
    return { ok: true, status: 200, text: '{}' }
  })
  await hooks.get('session.start')(api, {}, value => value)
  await hooks.get('tool.call')(api, { tool: 'Bash', tool_use_id: 'refused' }, () => assert.fail('refused'))
  await hooks.get('tool.call')(api, { tool: 'Bash', tool_use_id: 'done' },
    () => ({ result: { backgroundTaskId: 'bash-1' } }))
  await hooks.get('tool.call')(api, { tool: 'Agent', tool_use_id: 'agent' },
    () => ({ result: { status: 'async_launched', agentId: 'agent-1' } }))
  restarted = true
  await timers[0]()
  await new Promise(resolve => setTimeout(resolve, 10))
  assert.equal(syncs.length, 1)
  assert.deepEqual(syncs[0].tools, [])
  assert.deepEqual(syncs[0].tasks.sort(), ['agent-1', 'bash-1'])
})
