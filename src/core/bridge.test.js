// src/core/bridge.js 的单测：node --test，无 DOM、无框架、不联网。
// 桩掉 window.__TAURI_INTERNALS__（Tauri v2 的 IPC 与事件分发形状），在进程内
// 复刻「壳 emit → 面板监听回调」的投递：回调收到的是事件对象 {event, id, payload}。
// 重点钉住 aiEvents 的解包与 chatId 过滤——拿错一层（把事件对象当载荷读 chatId）
// 等于把 ai-chunk/ai-done/ai-error/ai-aborted 全部滤掉，助手每一轮都会永远转圈。
import { test } from 'node:test'
import assert from 'node:assert/strict'

// ── Tauri IPC 桩 ────────────────────────────────────────────────────
const callbacks = new Map()
const listenersByEvent = new Map()
let nextId = 1
let onChatRequest = null
let onClearLog = null

function resetIpc() {
  callbacks.clear()
  listenersByEvent.clear()
  nextId = 1
  onChatRequest = null
  onClearLog = null
}

globalThis.window = {
  requestAnimationFrame: cb => setTimeout(cb, 0),
  __TAURI_INTERNALS__: {
    transformCallback: (callback, once = false) => {
      const id = nextId++
      callbacks.set(id, { callback, once })
      return id
    },
    unregisterCallback: id => callbacks.delete(id),
    invoke: (cmd, args = {}) => {
      if (cmd === 'plugin:event|listen') {
        const entry = callbacks.get(args.handler)
        assert.ok(entry, 'listen 必须携带已注册的回调')
        const list = listenersByEvent.get(args.event) || []
        list.push(entry.callback)
        listenersByEvent.set(args.event, list)
        return Promise.resolve(1000 + nextId++)
      }
      if (cmd === 'plugin:event|unlisten') return Promise.resolve()
      if (cmd === 'ai_chat') {
        const chatId = args.payload?.chatId
        if (onChatRequest) onChatRequest(chatId)
        return Promise.resolve()
      }
      if (cmd === 'ai_abort') return Promise.resolve()
      if (cmd === 'clear_log') {
        if (!onClearLog) return Promise.reject(new Error('unexpected invoke: clear_log'))
        return Promise.resolve(onClearLog())
      }
      return Promise.reject(new Error(`unexpected invoke: ${cmd}`))
    },
  },
  // _unlisten 会触达事件插件的内部钩子（bridge.js 的 finally 里逐个退订）。
  __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener: () => {} },
}

// 与 Tauri v2 一致的投递形状：监听回调收到 {event, id, payload}。
function deliver(event, payload) {
  for (const callback of listenersByEvent.get(event) || []) {
    callback({ event, id: 0, payload })
  }
}

const { aiChat, clearLog } = await import('./bridge.js')

// 回归保险：Promise 若不落定，测试在 1.5s 内失败（而不是把整个套件挂死）。
function withTimeout(promise, label) {
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${label}：1.5s 内未落定`)), 1500)),
  ])
}

test('ai-done 按 Tauri v2 事件形状解包并结算为成功', async () => {
  resetIpc()
  onChatRequest = chatId => {
    setTimeout(() => deliver('ai-chunk', { chatId, text: '你' }), 5)
    setTimeout(
      () => deliver('ai-done', { chatId, content: '你好', toolCalls: [], finishReason: 'stop' }),
      10,
    )
  }
  const chunks = []
  const result = await withTimeout(
    aiChat(
      { selfHosted: true, model: 'WB · auto', messages: [{ role: 'user', content: 'hi' }] },
      text => chunks.push(text),
    ),
    'ai-done',
  )
  assert.equal(result.ok, true)
  assert.equal(result.content, '你好')
  assert.deepEqual(result.toolCalls, [])
  assert.deepEqual(chunks, ['你'])
})

test('chatId 不匹配的事件被过滤，只有匹配的才结算', async () => {
  resetIpc()
  onChatRequest = chatId => {
    // 先到一条别的会话的事件（壳串发多轮时可能发生），再到本轮的终态。
    setTimeout(() => deliver('ai-done', { chatId: 'ai-other', content: '别人的回复' }), 5)
    setTimeout(() => deliver('ai-done', { chatId, content: '本轮回复' }), 20)
  }
  const result = await withTimeout(
    aiChat(
      { selfHosted: true, model: 'WB · auto', messages: [{ role: 'user', content: 'hi' }] },
      () => {},
    ),
    'chatId 过滤',
  )
  assert.equal(result.ok, true)
  assert.equal(result.content, '本轮回复')
})

test('ai-error 解包后结算为失败并带原因', async () => {
  resetIpc()
  onChatRequest = chatId => {
    setTimeout(
      () => deliver('ai-error', { chatId, error: 'AI 端点在流中返回错误：boom', partial: '部分' }),
      5,
    )
  }
  const result = await withTimeout(
    aiChat(
      { selfHosted: true, model: 'WB · auto', messages: [{ role: 'user', content: 'hi' }] },
      () => {},
    ),
    'ai-error',
  )
  assert.equal(result.ok, false)
  assert.equal(result.error, 'AI 端点在流中返回错误：boom')
  assert.equal(result.partial, '部分')
})

test('clearLog 走壳命令 clear_log 并原样带回结果', async () => {
  resetIpc()
  onClearLog = () => ({ cleared: true })
  const result = await withTimeout(clearLog(), 'clearLog 成功')
  assert.equal(result.ok, true)
  assert.equal(result.result.cleared, true)
})

test('clearLog 命令失败时映射为 { ok:false, error }，不抛出', async () => {
  resetIpc()
  // 桩未注册 clear_log：invoke 会以 unexpected invoke 拒绝，bridge 不得把它抛成未捕获异常。
  onClearLog = null
  const result = await withTimeout(clearLog(), 'clearLog 失败')
  assert.equal(result.ok, false)
  assert.match(result.error, /clear_log/)
})
