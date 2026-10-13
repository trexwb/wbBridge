// AI 助手会话：跨视图切换 / 重启的会话状态，以及本机持久化（IndexedDB）。
//
// 为什么状态放在模块里而不是组件里：
// 面板用 v-if 切换视图，切走即卸载组件。会话状态要是组件的 ref，一轮生成还没回来就被一起
// 销毁，「切走再回来接着看」无从谈起。这里的状态是**模块级单例**：卸载组件不影响在飞的那一轮
// 继续往同一份 messages 里追加增量，回来时按最新内容重新渲染即可续上（生成中切走也不会被
// 中止 —— onBeforeUnmount 不再调 abortChat）。
//
// 持久化口径（只写本机 IndexedDB；不进 localStorage、不进 prefs、不进任何接口）：
//  • 只存对话内容。外部端点的 API Key 始终只在会话内存里（ai.js 的既定纪律，本模块不碰它）；
//    set_provider_key 的工具入参带着用户 Key 明文，落盘前把整条 args 丢弃 —— 界面只用到
//    已经算好的 summary / preview（那里是掩码），args 执行完就再没人读。
//  • 写盘失败（隐私模式 / 配额 / 存储被禁 / WebView 没给 IndexedDB）一律**静默降级**成
//    「这次不记住」：不弹错、不影响对话本身。
//  • 「清空」同时清内存与库；「设置」里可关掉记忆，关掉后立刻删除已存的记录。
import { ref, watch } from 'vue'

const DB_NAME = 'wbbridge-panel'
const DB_VERSION = 1
const STORE = 'kv'
const CHAT_KEY = 'chat'
const META_KEY = 'meta'

// ── 落盘上限 ────────────────────────────────────────────────────────
// 对话可以很长、工具返回可以很大（read_log 一次 8KB），不设上限会把整机存储慢慢吃掉。
// 取值只求「够回看一段对话」：条数按最近算，正文与字段各自截断。
export const MAX_STORED_MESSAGES = 100
export const MAX_STORED_CONTENT = 16000
export const MAX_STORED_FIELD = 2000
const MAX_STORED_TOOLS = 20
const MAX_ARRAY_ITEMS = 50
const MAX_DEPTH = 4
const MAX_PREVIEW_ITEMS = 12

const SAVE_DEBOUNCE_MS = 600
// 流式期间内容每 80ms 变一次，纯防抖会被一直推后、永远写不下去；
// 加一个最长等待，保证生成中途关掉应用也只丢最后几秒。
const SAVE_MAX_WAIT_MS = 2500

// ── 会话状态（模块级单例） ──────────────────────────────────────────
export const messages = ref([]) // { id, role, content, pending, failed, error, partial, stopped, trimmed, tools: [] }
export const chatting = ref(false)
export const draft = ref('')
export const confirming = ref(null) // { calls, resolve } —— 提案确认也要活过卸载，否则切走再回来这一轮永远卡住
export const lastTurn = ref(null) // { text, history } —— 「重试」用；不落盘
export const persistEnabled = ref(true)
// 核心快照的副本：由 App.vue 持续喂进来。工具执行要读它，而在飞的那一轮可能正处在
// 「视图已卸载」的时段，此时组件的 props 早已停更 —— 取这里的副本才拿得到当下的状态。
export const coreSnapshot = ref({})

export function setCoreSnapshot(next) {
  coreSnapshot.value = next && typeof next === 'object' ? next : {}
}

// ── 纯函数：内存态 ↔ 存储态 ─────────────────────────────────────────
function clipText(value, max) {
  const text = typeof value === 'string' ? value : String(value ?? '')
  return text.length > max ? `${text.slice(0, max)}…（已截断）` : text
}

// 递归裁剪：字符串按上限截断、数组只留前 N 项（工具返回可能是几百行列表）。
// 深度到顶直接压成空串，避免嵌套结构把库撑爆。
export function clipDeep(value, max = MAX_STORED_FIELD, depth = 0) {
  if (value === null || value === undefined) return null
  if (typeof value === 'string') return clipText(value, max)
  if (typeof value === 'number' || typeof value === 'boolean') return value
  if (depth >= MAX_DEPTH) return ''
  if (Array.isArray(value)) {
    return value.slice(0, MAX_ARRAY_ITEMS).map(item => clipDeep(item, max, depth + 1))
  }
  if (typeof value === 'object') {
    const out = {}
    for (const [key, item] of Object.entries(value)) out[key] = clipDeep(item, max, depth + 1)
    return out
  }
  return ''
}

const TOOL_STATUSES = ['pending', 'running', 'done', 'skipped']

function toStorageTool(entry) {
  const source = entry && typeof entry === 'object' ? entry : {}
  const validation = source.validation && typeof source.validation === 'object' ? source.validation : { ok: true }
  return {
    id: String(source.id ?? ''),
    name: String(source.name ?? ''),
    label: String(source.label ?? ''),
    write: source.write === true,
    danger: source.danger === true,
    summary: clipText(source.summary ?? '', 300),
    // preview 由 ai.js 生成（Key 已是掩码），可以直接存。
    preview: clipPreview(source.preview),
    validation: {
      ok: validation.ok === true,
      error: clipText(validation.error ?? '', 300),
    },
    // 待确认 / 执行中的卡片落盘后不可能再继续，统一记成已跳过，回来时不留一个永远转的状态。
    status: TOOL_STATUSES.includes(source.status) && source.status !== 'running' && source.status !== 'pending'
      ? source.status
      : source.result
        ? 'done'
        : 'skipped',
    // 🔴 不存 args：set_provider_key 的入参里有用户 Key 明文。
    //    执行早已完成、界面也只显示 summary / preview，落盘后没有任何地方再读它。
    result: source.result ? clipDeep(source.result) : null,
  }
}

function clipPreview(value) {
  const source = value && typeof value === 'object' && !Array.isArray(value) ? value : {}
  const out = {}
  let count = 0
  for (const [key, item] of Object.entries(source)) {
    if (count >= MAX_PREVIEW_ITEMS) break
    if (typeof item === 'string' || typeof item === 'number' || typeof item === 'boolean') {
      out[String(key)] = clipText(item, 200)
      count += 1
    }
  }
  return out
}

export function toStorageMessages(list) {
  const source = Array.isArray(list) ? list : []
  const rows = []
  for (const row of source.slice(-MAX_STORED_MESSAGES)) {
    const item = row && typeof row === 'object' ? row : null
    if (!item) continue
    // 只有成文的 user / assistant 进库：工具消息是协议层内容，界面不单独渲染。
    if (item.role !== 'user' && item.role !== 'assistant') continue
    const content = clipText(item.content ?? '', MAX_STORED_CONTENT)
    const wasPending = item.pending === true
    rows.push({
      id: String(item.id ?? ''),
      role: item.role,
      content,
      // 未完成的回复落盘后不可能再接着生成：存成终态，回来时不会留下一个一直闪烁的光标。
      pending: false,
      stopped: wasPending ? content.length > 0 : item.stopped === true,
      // 一个字都还没生成就被打断：给一条明确原因，而不是空气泡。
      failed: wasPending ? content.length === 0 : item.failed === true,
      error: wasPending && content.length === 0
        ? '这轮回复还没生成完，应用已退出或重载'
        : clipText(item.error ?? '', 300),
      partial: wasPending ? content.length > 0 : item.partial === true,
      trimmed: Number.isFinite(item.trimmed) ? item.trimmed : 0,
      tools: (Array.isArray(item.tools) ? item.tools.slice(0, MAX_STORED_TOOLS) : []).map(toStorageTool),
    })
  }
  return rows
}

export function fromStorageMessages(rows) {
  const source = Array.isArray(rows) ? rows : []
  const out = []
  for (const row of source) {
    const item = row && typeof row === 'object' ? row : null
    if (!item) continue
    if (item.role !== 'user' && item.role !== 'assistant') continue
    out.push({
      id: String(item.id ?? ''),
      role: item.role,
      content: typeof item.content === 'string' ? item.content : '',
      pending: false,
      failed: item.failed === true,
      error: typeof item.error === 'string' ? item.error : '',
      partial: item.partial === true,
      stopped: item.stopped === true,
      trimmed: Number.isFinite(item.trimmed) ? item.trimmed : 0,
      tools: (Array.isArray(item.tools) ? item.tools : []).map(entry => {
        const source = entry && typeof entry === 'object' ? entry : {}
        return {
          id: String(source.id ?? ''),
          name: String(source.name ?? ''),
          args: {},
          label: String(source.label ?? ''),
          write: source.write === true,
          danger: source.danger === true,
          summary: typeof source.summary === 'string' ? source.summary : '',
          preview: source.preview && typeof source.preview === 'object' ? source.preview : {},
          validation: source.validation && typeof source.validation === 'object'
            ? { ok: source.validation.ok === true, error: String(source.validation.error ?? '') }
            : { ok: true },
          status: TOOL_STATUSES.includes(source.status) ? source.status : 'done',
          result: source.result && typeof source.result === 'object' ? source.result : null,
        }
      }),
    })
  }
  return out
}

// ── IndexedDB（全部静默降级） ───────────────────────────────────────
let dbPromise = null

function openDb() {
  if (dbPromise) return dbPromise
  dbPromise = new Promise(resolve => {
    const factory = globalThis.indexedDB
    if (!factory || typeof factory.open !== 'function') {
      resolve(null)
      return
    }
    let request
    try {
      request = factory.open(DB_NAME, DB_VERSION)
    } catch {
      resolve(null)
      return
    }
    request.onupgradeneeded = () => {
      const db = request.result
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE, { keyPath: 'id' })
    }
    request.onsuccess = () => resolve(request.result)
    // 打开失败（隐私模式 / 被禁 / 别的标签页占着旧版本）不当成错误：只是这次不记住。
    request.onerror = () => resolve(null)
    request.onblocked = () => resolve(null)
  })
  return dbPromise
}

function readRecord(db, key) {
  return new Promise(resolve => {
    try {
      const request = db.transaction(STORE, 'readonly').objectStore(STORE).get(key)
      request.onsuccess = () => resolve(request.result ?? null)
      request.onerror = () => resolve(null)
    } catch {
      resolve(null)
    }
  })
}

function writeRecords(db, records) {
  return new Promise(resolve => {
    try {
      const tx = db.transaction(STORE, 'readwrite')
      const store = tx.objectStore(STORE)
      for (const record of records) store.put(record)
      tx.oncomplete = () => resolve(true)
      tx.onerror = () => resolve(false)
      tx.onabort = () => resolve(false)
    } catch {
      resolve(false)
    }
  })
}

function deleteRecord(db, key) {
  return new Promise(resolve => {
    try {
      const tx = db.transaction(STORE, 'readwrite')
      tx.objectStore(STORE).delete(key)
      tx.oncomplete = () => resolve(true)
      tx.onerror = () => resolve(false)
      tx.onabort = () => resolve(false)
    } catch {
      resolve(false)
    }
  })
}

// ── 节流写盘 ────────────────────────────────────────────────────────
let saveTimer = null
let waitingSince = 0

function cancelPendingSave() {
  if (saveTimer) clearTimeout(saveTimer)
  saveTimer = null
  waitingSince = 0
}

async function writeNow() {
  if (!persistEnabled.value) return
  const db = await openDb()
  if (!db) return
  await writeRecords(db, [{ id: CHAT_KEY, updatedAt: Date.now(), messages: toStorageMessages(messages.value) }])
}

// 防抖 + 最长等待：连续变化合并成一次写，但不会无限期推后。
export function schedulePersist() {
  if (!persistEnabled.value || !hydrated) return
  const now = Date.now()
  if (!waitingSince) waitingSince = now
  if (saveTimer) clearTimeout(saveTimer)
  const delay = Math.max(0, Math.min(SAVE_DEBOUNCE_MS, waitingSince + SAVE_MAX_WAIT_MS - now))
  saveTimer = setTimeout(() => {
    saveTimer = null
    waitingSince = 0
    void writeNow()
  }, delay)
}

// 条数变化（提问 / 落一条回复 / 清空）自动触发一次写盘。
// 正文的流式增量不走 watch（深监听一整棵树代价太大），由回合结束处显式 schedulePersist()。
watch(() => messages.value.length, () => schedulePersist())

// ── 读取与生命周期 ──────────────────────────────────────────────────
let hydrated = false

export function isHydrated() {
  return hydrated
}

// 首次进入助手视图时把上次会话读回来。重复调用安全；在飞的那一轮绝不被覆盖。
export async function ensureHydrated() {
  if (hydrated || chatting.value) return
  hydrated = true
  const db = await openDb()
  if (!db) {
    // 没有可用的存储：记忆功能当场失效，界面上的开关也随之关掉（不假装还在记）。
    persistEnabled.value = false
    return
  }
  const [chat, meta] = await Promise.all([readRecord(db, CHAT_KEY), readRecord(db, META_KEY)])
  if (meta && typeof meta.persist === 'boolean') persistEnabled.value = meta.persist
  // 读库是异步的：这期间用户可能已经发了新提问。绝不能用旧记录覆盖当下的会话。
  if (chatting.value || messages.value.length) return
  const rows = Array.isArray(chat?.messages) ? chat.messages : []
  const restored = fromStorageMessages(rows)
  if (restored.length) messages.value = restored
}

export function setPersist(enabled) {
  const next = enabled === true
  persistEnabled.value = next
  if (!next) cancelPendingSave()
  void (async () => {
    const db = await openDb()
    if (!db) return
    await writeRecords(db, [{ id: META_KEY, persist: next }])
    // 关掉记忆就把已经存下的记录删掉：留着等于「说不记了但还在记」。
    if (!next) await deleteRecord(db, CHAT_KEY)
  })()
}

// 「清空」：内存与库一起清，不留半份。
export function clearConversation() {
  messages.value = []
  lastTurn.value = null
  draft.value = ''
  cancelPendingSave()
  void (async () => {
    const db = await openDb()
    if (db) await deleteRecord(db, CHAT_KEY)
  })()
}
