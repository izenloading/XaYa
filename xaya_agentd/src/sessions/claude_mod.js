// Installed into this launch's private plugin directory; authenticated admission and observation.
import { endpoint, token } from './config.js'

let sessionId = null
let sequence = 0
let missed = 0
let tail = Promise.resolve()
let activeTurn = null
let polling = false
// What a restarted service needs to rejoin this TUI (see sync). Each changes in the same
// synchronous step that assigns the event's sequence, so a snapshot is always consistent with
// the sequence it reports; tools count only once their admission was delivered.
const tools = new Map()
const tasks = new Set()
let interrupted = false
const SYNC_REQUIRED = 428

async function control($, route, value) {
  const response = await $.http.fetch(endpoint + '/control/' + route, {
    method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Xaya-Token': token },
    body: JSON.stringify({ sessionId, ...value }),
  })
  if (response.status === SYNC_REQUIRED) {
    tail = tail.then(() => sync($))
    throw new Error('XaYa service restarted; rejoining')
  }
  if (!response.ok) throw new Error('XaYa control transport unavailable')
  return JSON.parse(response.text)
}

// A restarted service answers 428 until it has this snapshot. Events up to `sequence` are
// superseded by it: queued ones are dropped (their hooks already failed closed) and losses
// while the service was down no longer count. Runs on the delivery chain, never beside flush.
async function sync($) {
  const snapshot = { sessionId, sequence, activeTurn, interrupted,
    tools: [...tools.values()].filter(tool => tool.admitted).map(tool => tool.identity),
    tasks: [...tasks], xayaGateVersion: 1, xayaControlVersion: 1, xayaSyncVersion: 1 }
  try {
    const response = await $.http.fetch(endpoint + '/sync', {
      method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Xaya-Token': token },
      body: JSON.stringify(snapshot),
    })
    if (!response.ok) return
  } catch { return }
  queue = queue.filter(item => item.event.sequence > snapshot.sequence)
  missed = 0
}

// Submitting may not resolve until the turn it starts has finished, so it never holds the
// poll loop: cancellation must still arrive. Delivery is judged by the service from the turn
// that carries this text; the acknowledgement only reports what submit() returned.
async function send($, command) {
  let ack = { status: 'unknown' }
  try {
    const result = await $.prompt.submit({ text: command.text, asUser: true })
    ack = typeof result?.drop === 'string'
      ? { status: 'dropped', reason: result.drop.slice(0, 200) }
      : { status: 'submitted' }
  } catch { /* Native execution or its reply may be uncertain. */ }
  try {
    await control($, 'ack', { command_id: command.command_id, ...ack })
  } catch { /* A lost or late acknowledgement is settled by server receipts. */ }
}

async function cancel($, command) {
  let status = 'failed'
  if (command.turn_id === activeTurn) {
    // The native API checks this ID again, atomically with cancellation. No fallback
    // to "whatever turn is active" and no retry when the acknowledgement is lost.
    status = 'unknown'
    try {
      const result = await $.turn.abort({ turnId: command.turn_id })
      if (result === undefined) status = 'succeeded'
    } catch { /* Native execution or its reply may be uncertain. */ }
  }
  await control($, 'ack', { command_id: command.command_id, turn_id: command.turn_id, status })
}

async function poll($) {
  if (polling) return
  polling = true
  try {
    const { command } = await control($, 'poll', {})
    if (!command || typeof command.command_id !== 'string') return
    if (command.operation === 'send' && typeof command.text === 'string') {
      void send($, command)
    } else if (command.operation === 'cancel' && typeof command.turn_id === 'string') {
      await cancel($, command)
    }
  } catch { /* Poll/ack loss is handled by server receipts, not the observation sequence. */ }
  finally { polling = false }
}

// Delivery queue. A turn abort (native Esc) cancels every $.http.fetch started inside that
// turn's hooks ("user-cancel"), so a failed delivery is not yet a lost observation: it stays
// queued, in order, and is resent by the next hook or clock tick. Only after GIVE_UP_MS (or
// when the queue overflows) is it counted as missed, which the service treats as a real loss.
const GIVE_UP_MS = 5000
let queue = []

async function flush($) {
  while (queue.length) {
    const item = queue[0]
    try {
      const response = await $.http.fetch(endpoint + '/events', {
        method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Xaya-Token': token },
        body: JSON.stringify({ ...item.event, missed, ...(item.late && { late: true }) }),
      })
      if (response.status === SYNC_REQUIRED) {
        await sync($)
        return
      }
      if (response.status !== 409 && !response.ok) throw new Error('XaYa service unavailable')
      item.allowed = response.status !== 409
      if (item.tool && item.allowed) item.tool.admitted = true
    } catch {
      if (Date.now() - item.since >= GIVE_UP_MS || queue.length > 256) {
        missed += queue.length
        queue = []
      }
      return
    }
    queue.shift()
    item.delivered = true
  }
}

async function observe($, type, data, tool) {
  const item = { event: { sessionId, sequence: ++sequence, type, data }, since: Date.now(),
    late: false, delivered: false, allowed: false, tool }
  queue.push(item)
  tail = tail.then(() => flush($))
  await tail
  // Its hook decides without it; a resend is evidence only, never a later admission.
  if (!item.delivered) item.late = true
  return item.allowed
}

function redeliver($) {
  if (queue.length) tail = tail.then(() => flush($))
}

export function register(on) {
  on('session.start', async ($, event, next) => {
    sessionId = await $.session.id()
    await observe($, 'session.start',
      { ...event, xayaGateVersion: 1, xayaControlVersion: 1, xayaSyncVersion: 1 })
    $.clock.every(100, () => { redeliver($); return poll($) })
    return next(event)
  })
  on('prompt.submit', async ($, event, next) => {
    if (!await observe($, 'input.start', {})) {
      return { drop: 'XaYa input gate is closed or unavailable; input was not sent.' }
    }
    return next(event)
  })
  on('turn.start', async ($, event, next) => {
    if (event.agentId == null) activeTurn = event.turnId
    await observe($, 'turn.start', event)
    return next(event)
  })
  on('turn.complete', async ($, event, next) => {
    if (event.agentId == null && activeTurn === event.turnId) activeTurn = null
    await observe($, 'turn.complete', event)
    return next(event)
  })
  on('turn.step', async function* ($, event, next) {
    const stream = next(event)
    let complete = false
    try {
      while (true) {
        const item = await stream.next()
        if (item.done) { complete = true; return item.value }
        if (item.value.kind === 'text') {
          await observe($, 'step.chunk', { turnId: event.turnId, agentId: event.agentId, chunk: item.value })
        }
        yield item.value
      }
    } finally {
      if (!complete) {
        try { await stream.return() } catch { missed++ }
      }
    }
  })
  on('tool.call', async ($, event, next) => {
    const identity = { tool: event.tool, tool_use_id: event.tool_use_id,
      agentId: event.agentId, task_id: event.task_id, shell_id: event.shell_id }
    const tool = { identity, admitted: false }
    const owned = typeof event.tool_use_id === 'string'
    if (owned) tools.set(event.tool_use_id, tool)
    if (!await observe($, 'tool.start', identity, tool)) {
      if (owned && tools.get(event.tool_use_id) === tool) tools.delete(event.tool_use_id)
      return { deny: 'XaYa input gate is closed or unavailable; tool was not run.' }
    }
    // A thrown call is deliberately left owned: interruption is not exit evidence.
    const result = await next(event)
    // Do not copy file contents or command output into the bounded control transport.
    const output = result?.result
    const lifecycle = { isError: result?.isError, deny: result?.deny === undefined ? undefined : 'native denial',
      result: { backgroundTaskId: output?.backgroundTaskId, status: output?.status,
        agentId: output?.agentId, interrupted: output?.interrupted } }
    if (tools.get(event.tool_use_id) === tool) tools.delete(event.tool_use_id)
    if (typeof output?.backgroundTaskId === 'string') tasks.add(output.backgroundTaskId)
    if (['async_launched', 'remote_launched'].includes(output?.status) && typeof output?.agentId === 'string') {
      tasks.add(output.agentId)
    }
    if (output?.interrupted === true) interrupted = true
    await observe($, 'tool.complete', { agentId: event.agentId, toolUseId: event.tool_use_id, result: lifecycle })
    return result
  })
}
