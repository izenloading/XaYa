// Injected only into an isolated P0 session; configuration is generated per launch.
import { endpoint, token } from './config.js'

let sessionId = null
let sequence = 0
let missed = 0
let polling = false
let eventTail = Promise.resolve()

async function post($, route, value) {
  const response = await $.http.fetch(endpoint + route, {
    method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Probe-Token': token },
    body: JSON.stringify(value),
  })
  if (!response.ok) throw new Error('Probe bridge HTTP ' + response.status)
  return JSON.parse(response.text)
}

async function observe($, type, data) {
  const event = { sessionId, sequence: ++sequence, type, data }
  eventTail = eventTail.then(async () => {
    try { await post($, '/events', { ...event, missed }) } catch { missed++ }
  })
  await eventTail
}

async function execute($, command) {
  let ack
  try {
    let result
    if (command.type === 'send' && typeof command.text === 'string') {
      result = await $.prompt.submit({ text: command.text, asUser: true })
    } else if (command.type === 'abort' && typeof command.turnId === 'string') {
      result = await $.turn.abort({ turnId: command.turnId })
    } else if (command.type === 'stop-task' && typeof command.taskId === 'string') {
      result = await $.tool.call({ tool: 'TaskStop', task_id: command.taskId })
    } else if (command.type === 'messages') {
      result = await $.session.messages()
    } else if (command.type === 'clear-draft') {
      result = await $.prompt.fill({ text: '', mode: 'replace' })
    } else {
      throw new Error('Unsupported or malformed probe command')
    }
    ack = { id: command.id, ok: true, result: result ?? null }
  } catch (error) {
    ack = { id: command.id, ok: false, error: String(error) }
  }
  try { await post($, '/acks', ack) } catch { missed++ }
}


/** Register transparent observation hooks and a small probe command channel. */
export function register(on) {
  on('session.start', async ($, event, next) => {
    sessionId = await $.session.id()
    await observe($, 'session.start', event)
    $.clock.every(100, async () => {
      if (polling) return
      polling = true
      try {
        const response = await post($, '/poll', {})
        // Sending may wait for a turn. Keep polling so abort/TaskStop can arrive.
        if (response.command) void execute($, response.command).catch(() => { missed++ })
      } catch { missed++ } finally { polling = false }
    })
    return next(event)
  })
  // Mods statically validates event names: each on() call needs a literal.
  on('turn.start', async ($, event, next) => {
    await observe($, 'turn.start', event)
    return next(event)
  })
  on('turn.complete', async ($, event, next) => {
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
        if (item.value.kind !== 'engine') {
          await observe($, 'step.chunk', { turnId: event.turnId, chunk: item.value })
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
    await observe($, 'tool.start', event)
    const result = await next(event)
    await observe($, 'tool.complete', { toolUseId: event.tool_use_id, result })
    return result
  })
}
