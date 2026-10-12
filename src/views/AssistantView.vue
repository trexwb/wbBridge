<script setup>
// AI 助手视图：自然语言管理 WB Bridge。
// 分工：src/core/ai.js 出协议与纯逻辑（prompt / 工具 schema / 校验 / 渲染），本视图只做
// 交互与回合循环。安全叙事与 fastenerTradeWorkbench 的 AI 抽屉一致：
//   • 只读工具立即执行；写入类工具只是**提案**，必须经用户逐条勾选确认后才执行；
//   • API Key 只在会话内存里（设置弹窗填写，不落盘、不回显、不进历史），请求时随命令
//     参数进壳；壳侧另有端点白名单复核；
//   • 对话内容不持久化：刷新或重启即清空，避免把用户输入（可能含敏感信息）写进 WebView 存储。
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import {
  AI_PRESETS,
  AI_TOOLS,
  AUTO_MODEL_ID,
  TOOL_META,
  abortChat,
  buildRequestMessages,
  buildSnapshot,
  chat,
  currentConfig,
  persistConfig,
  phaseLabel,
  renderMarkdownLite,
  runTool,
  summarizeToolCall,
  toolCallPreview,
  toolResultMessage,
  trimRequest,
  validateConfig,
  validateSelfConfig,
  validateToolCall,
} from '../core/ai.js'

const props = defineProps({
  // 壳推送的完整状态快照（core-status + core-activity 合并后的那份）。
  state: { type: Object, default: () => ({}) },
  // 其他视图有动作在飞（App.vue 的 busyAction）：助手此时起草的操作会被核心以「请等待…」拒绝，
  // 提前拦住比让用户看一条失败卡片更符合预期。
  busy: { type: Boolean, default: false },
})

const QUICK_PROMPTS = [
  '现在服务状态怎么样？有哪些模型可用？',
  '哪些模型不可用？帮我看最近日志找原因。',
  '帮我重新读取免费模型，然后检测全部。',
  '智谱平台的 Key 怎么配置？我有 Key，帮我保存。',
  '本机数据目录和插件配置文件都在哪里？',
  '现在有新版本可以更新吗？',
]

// ── 会话状态（全部只在内存） ────────────────────────────────────────
const messages = ref([]) // { id, role, content, pending, failed, trimmed, tools: [] }
const draft = ref('')
const chatting = ref(false)
const config = reactive({ ...currentConfig(), apiKey: '' })
const showSettings = ref(false)
const settingsForm = reactive({ brain: 'self', baseUrl: '', model: '', selfModel: AUTO_MODEL_ID, apiKey: '', tools: true })
const settingsError = ref('')
const confirming = ref(null) // { calls, resolve }
const notice = ref('') // 输入框上方的一次性说明（拦下一次发送时给原因，不进对话历史）
let noticeTimer = null
const chatBox = ref(null) // 对话区容器：滚动到底要用它，而不是 document.querySelector
const composer = ref(null) // 输入框：自动增高 + 发送后重新聚焦
const settingsPanel = ref(null)
const confirmPanel = ref(null)
// 最近一轮的入参，供失败后的「重试」原样重发（不重新读输入框，避免用户已改过文案）。
let lastTurn = null

// 核心就绪才谈得上「本机大脑」与工具：phase 不是 ready 时先给一条明确说明，
// 而不是让用户对着一个必然 401/连接失败的报错猜原因。
const coreReady = computed(() => props.state.phase === 'ready')

// 当前模型 id 名单：本机大脑选模型、probe_models 指定单个模型时都用它校验
//（免费模型名单由上游决定，填不存在的 id 只会在请求发出后才失败）。
const modelIds = computed(() => (Array.isArray(props.state.models) ? props.state.models : []).map(m => String(m?.id ?? '')))

// 「本机大脑」的候选：第一项固定是 WB · auto（默认，按请求内容自动选模型），
// 其余是**探测已通过**的模型（未通过的选了也只会失败）。用下拉而不是手填 id，
// 用户不必去列表里抄一串全限定名。
const selfOptions = computed(() => {
  const models = Array.isArray(props.state.models) ? props.state.models : []
  const results = props.state.modelResults && typeof props.state.modelResults === 'object' ? props.state.modelResults : {}
  const available = new Set((Array.isArray(props.state.availableModels) ? props.state.availableModels : []).map(String))
  const rows = models
    .filter(m => available.has(String(m?.id ?? '')))
    .map(m => {
      const id = String(m?.id ?? '')
      const entry = results[id]
      const tags = []
      if (entry?.chatOnly === true) tags.push('仅对话')
      if (m?.images === true) tags.push('图片')
      if (m?.toolcall === true) tags.push('工具')
      const name = typeof m?.name === 'string' && m.name ? m.name : id
      return { value: id, label: tags.length ? `${name}（${tags.join(' · ')}）` : name }
    })
  // 允许保留当前选中但已不在可用集的 id（模型刚失效时下拉不该把选择吞掉变成 auto）。
  const current = String(settingsForm.selfModel || '')
  if (current && current !== AUTO_MODEL_ID && !rows.some(row => row.value === current)) {
    rows.unshift({ value: current, label: `${current}（当前不在可用集）` })
  }
  return [{ value: AUTO_MODEL_ID, label: 'WB · auto（按请求自动选模型，推荐）' }, ...rows]
})

// 只有「外部端点 + 非本机地址」才需要 Key；本机大脑（自托管）由壳自己持令牌，永远不需要。
const needsKey = computed(
  () => config.brain === 'external' && !/^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:|\/|$)/i.test(config.baseUrl),
)

// 界面当前真正会用到的大脑（供上下文徽标与空状态文案复用，避免两处各写一遍）。
const brainLabel = computed(
  () => (config.brain === 'external' ? `外部端点 · ${config.model}` : `本机免费模型 · ${config.selfModel || '未选择'}`),
)

// 本机大脑选中「仅对话」模型：核心对 chatOnly 模型带 tools 的请求一律回 400
// tools_not_supported（这是核心的既有红线，不是本视图的策略）。这一轮因此不声明工具，
// 让模型照常问答，并在界面说明为什么现在给不了操作。
// WB · auto 不算：核心的路由器会把带 tools 的请求只发给支持工具的模型（auto.rs 的档位谓词）。
const selfChatOnly = computed(() => {
  if (config.brain !== 'self' || !config.selfModel || config.selfModel === AUTO_MODEL_ID) return false
  const entry = props.state.modelResults?.[config.selfModel]
  return entry?.chatOnly === true
})

// 本轮是否声明工具：开关关闭、或本机大脑选中「仅对话」模型时不声明。
const toolsEnabled = computed(() => config.tools !== false && !selfChatOnly.value)

// 粘底滚动：只有用户本来就贴在底部时才跟着新内容走。
// 助手在读日志 / 逐模型列表时一次推很长一段，无条件滚到底会把用户往上翻的位置强行拽走，
// 而「悄悄不滚」比「强行滚动」更符合预期。
const stickToBottom = ref(true)

function onChatScroll() {
  const box = chatBox.value
  if (!box) return
  stickToBottom.value = box.scrollHeight - box.scrollTop - box.clientHeight < 48
}

function scrollBottom() {
  if (!stickToBottom.value) return
  nextTick(() => {
    const box = chatBox.value
    if (box) box.scrollTop = box.scrollHeight
  })
}

// 输入框随内容增高（上限交给 CSS max-height，超出后自身出滚动条）。
// rows=1 时固定高会把三行以上的提问藏进滚动条里，用户看不见自己写到哪。
function autoGrow() {
  const box = composer.value
  if (!box) return
  box.style.height = 'auto'
  box.style.height = `${box.scrollHeight}px`
}

watch(draft, autoGrow)

// 输入框上方的一次性说明：8s 自动消隐（与面板 FeedbackBar 的口径一致），
// 且**不进对话历史**——那是拦下这次发送的界面提示，不是模型说过的话。
function showNotice(text) {
  notice.value = text
  if (noticeTimer) clearTimeout(noticeTimer)
  noticeTimer = setTimeout(() => {
    notice.value = ''
    noticeTimer = null
  }, 8000)
}

function dismissNotice() {
  if (noticeTimer) clearTimeout(noticeTimer)
  noticeTimer = null
  notice.value = ''
}

// ── 设置弹窗 ────────────────────────────────────────────────────────
function openSettings() {
  settingsForm.brain = config.brain
  settingsForm.baseUrl = config.baseUrl
  settingsForm.model = config.model
  settingsForm.selfModel = config.selfModel
  settingsForm.apiKey = config.apiKey
  settingsForm.tools = config.tools !== false
  settingsError.value = ''
  showSettings.value = true
  // 弹窗是可聚焦容器（tabindex=-1）：打开即取焦点，Tab 才从内容开始走，
  // Esc 也才有明确的收起目标（这里的 Esc 仍由 window 级监听兜住）。
  nextTick(() => settingsPanel.value && settingsPanel.value.focus())
}

function applyPreset(preset) {
  settingsForm.brain = 'external'
  settingsForm.baseUrl = preset.baseUrl
  settingsForm.model = preset.model
}

function saveSettings() {
  if (settingsForm.brain === 'external') {
    const error = validateConfig({ baseUrl: settingsForm.baseUrl, model: settingsForm.model })
    if (error) {
      settingsError.value = error
      return
    }
  } else {
    const error = validateSelfConfig({ selfModel: settingsForm.selfModel, modelIds: modelIds.value })
    if (error) {
      settingsError.value = error
      return
    }
  }
  config.brain = settingsForm.brain === 'external' ? 'external' : 'self'
  config.baseUrl = settingsForm.baseUrl.trim().replace(/\/+$/, '')
  config.model = settingsForm.model.trim()
  config.selfModel = settingsForm.selfModel.trim()
  config.tools = settingsForm.tools !== false
  // Key 只进会话内存：即使保存设置也绝不写进 prefs / 任何存储。
  config.apiKey = settingsForm.apiKey.trim()
  persistConfig({
    brain: config.brain,
    baseUrl: config.baseUrl,
    model: config.model,
    selfModel: config.selfModel,
    tools: config.tools,
  })
  showSettings.value = false
}

// ── 工具确认弹窗 ────────────────────────────────────────────────────
// 提案 → 勾选 → 执行。全不选或取消 = 一条都不执行（对应 tool 结果回传「用户未批准」）。
function confirmToolCalls(calls) {
  return new Promise(resolve => {
    confirming.value = {
      calls: calls.map(call => ({ ...call, checked: call.validation.ok === true })),
      resolve,
    }
    // 确认框是这一轮唯一的阻塞点：焦点直接落在它上面，Tab 与 Esc 都从「要不要执行」开始。
    nextTick(() => confirmPanel.value && confirmPanel.value.focus())
  })
}

function allChecked() {
  const list = confirming.value?.calls || []
  return list.length > 0 && list.every(call => call.checked)
}

function toggleAll(event) {
  const checked = event.target.checked
  for (const call of confirming.value?.calls || []) {
    if (call.validation.ok) call.checked = checked
  }
}

function settleConfirm(approved) {
  const current = confirming.value
  if (!current) return
  confirming.value = null
  current.resolve(approved)
}

function runApproved() {
  const approved = (confirming.value?.calls || []).filter(call => call.checked && call.validation.ok)
  settleConfirm(approved)
}

// ── 回合循环 ────────────────────────────────────────────────────────
// 历史只取已成文的 user/assistant 消息（工具调换过程不跨回合保留，最终 assistant
// 文本已把结论说清）；本轮的工具调用与结果随 request 增量追加。
// history 由 send() 在**推送用户消息之前**取好，避免本轮提问在历史里出现两次。
function apiHistory() {
  return messages.value
    .filter(m => (m.role === 'user' || m.role === 'assistant') && !m.pending && String(m.content || '').trim())
    .slice(-20)
    .map(m => ({ role: m.role, content: String(m.content || '') }))
}

function parseArgs(raw) {
  try {
    const parsed = JSON.parse(String(raw || '{}'))
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : {}
  } catch {
    return null // 参数不是合法 JSON：校验阶段报错，不猜
  }
}

async function send() {
  const text = draft.value.trim()
  if (!text || chatting.value) return
  // 其他视图的动作在飞时不发起请求：核心的 /admin/* 会拒绝并发的读取与探测，
  // 助手这时起草的工具只会得到一条失败卡片，拦在前面比事后解释更符合预期。
  if (props.busy) {
    showNotice('其他操作正在进行中，请等它结束后再让助手执行动作（也可以先让它解释状态、读日志）。')
    return
  }
  const error =
    config.brain === 'external'
      ? validateConfig({ baseUrl: config.baseUrl, model: config.model })
      : validateSelfConfig({ selfModel: config.selfModel, modelIds: modelIds.value })
  if (error) {
    openSettings()
    settingsError.value = error
    return
  }
  // 外部端点缺 Key：请求只会被端点判 401，先把用户带到填写处，别发一条注定失败的请求。
  if (needsKey.value && !config.apiKey) {
    openSettings()
    settingsError.value = '当前端点需要 API Key：填写后点「保存」（只留在本会话内存，不落盘）。本地 Ollama 一类端点可在「设置」改成本机地址后留空。'
    return
  }
  // 本机大脑依赖正在运行的核心（端点与令牌由壳解析），未就绪时请求只会变成「连不上回环地址」。
  if (config.brain === 'self' && !coreReady.value) {
    showNotice(
      `本机大脑需要核心处于「就绪」，当前阶段是「${phaseLabel(props.state.phase)}」。可在「设置」改用外部端点，或回「模型与服务」把服务起起来。`,
    )
    return
  }
  const history = apiHistory()
  messages.value.push({ id: `m${Date.now()}`, role: 'user', content: text })
  draft.value = ''
  stickToBottom.value = true
  scrollBottom()
  await runTurn(text, history)
}

// 重发最近一轮：失败的那条气泡原地替换成新的尝试，历史沿用出错前的那份（不把失败回复教给模型）。
async function retry() {
  if (chatting.value || !lastTurn) return
  const last = messages.value[messages.value.length - 1]
  if (last && last.role === 'assistant' && last.failed) messages.value.pop()
  await runTurn(lastTurn.text, lastTurn.history)
}

async function runTurn(userText, history) {
  chatting.value = true
  notice.value = ''
  lastTurn = { text: userText, history }
  messages.value.push({ id: `m${Date.now()}a`, role: 'assistant', content: '', pending: true, tools: [] })
  // 取回数组里的响应式代理再持有：直接改原始对象不会触发重渲染，
  // 流式增量会看起来「冻住」，直到别的状态变化才一次性显出来。
  const live = messages.value[messages.value.length - 1]
  scrollBottom()

  // 流式增量节流：80ms 合一批再渲染，避免每个 chunk 都触发整树更新。
  let buffer = ''
  let flushTimer = null
  const flush = () => {
    flushTimer = null
    if (buffer) {
      live.content += buffer
      buffer = ''
      scrollBottom()
    }
  }
  const onDelta = text => {
    buffer += text
    if (!flushTimer) flushTimer = setTimeout(flush, 80)
  }

  try {
    const request = buildRequestMessages({
      history,
      message: userText,
      snapshot: buildSnapshot(props.state),
    })
    for (let round = 0; round < 6; round++) {
      // 工具开关关闭、或本机大脑选中「仅对话」模型时不声明 tools（壳侧同样只在非空时才带
      // tool_choice）：模型无从发起调用，界面也明确告知「只能问答」。
      const tools = toolsEnabled.value ? AI_TOOLS : []
      const response = await chat(
        config.brain === 'external'
          ? { baseUrl: config.baseUrl, apiKey: config.apiKey, model: config.model, messages: request, tools }
          : { selfHosted: true, model: config.selfModel, messages: request, tools },
        onDelta,
      )
      flush()
      if (!response.ok) {
        if (response.aborted) {
          live.content = (live.content ? live.content + '\n\n' : '') + '（已停止生成，以上为已生成部分）'
        } else {
          live.failed = true
          live.content = (live.content ? live.content + '\n\n' : '') + `⚠ ${response.error || '请求失败'}`
          if (response.partial) live.content += '\n（以上为已生成部分）'
        }
        break
      }
      const calls = response.toolCalls || []
      if (!calls.length) {
        // 纯文本回合：以模型最终返回为准（流式中断时保底已累积内容）。
        if (response.content && response.content.length >= live.content.length) live.content = response.content
        break
      }
      // 工具回合：先落 assistant 消息（OpenAI 协议要求 tool 结果挂在 tool_calls 之后）。
      live.content = response.content || live.content
      request.push({
        role: 'assistant',
        content: live.content || '',
        tool_calls: calls.map(call => ({
          id: call.id,
          type: 'function',
          function: { name: call.name, arguments: call.arguments },
        })),
      })
      // 逐条建卡片：只读工具立即执行，写工具先进提案确认。
      // 卡片状态（running / done / result）要能在执行过程中上屏，因此同样取回代理对象。
      const entries = calls.map(call => {
        const args = parseArgs(call.arguments)
        const validation = validateToolCall(call.name, args || {}, { modelIds: modelIds.value })
        live.tools.push({
          id: call.id,
          name: call.name,
          args: args || {},
          label: TOOL_META[call.name]?.label || call.name,
          write: TOOL_META[call.name]?.write === true,
          danger: TOOL_META[call.name]?.danger === true,
          summary: summarizeToolCall(call.name, args || {}),
          preview: toolCallPreview(call.name, args || {}),
          validation,
          status: 'pending', // pending → running → done / skipped
          result: null,
        })
        return live.tools[live.tools.length - 1]
      })
      scrollBottom()

      const writes = entries.filter(entry => entry.write)
      // 确认框拿到的是卡片快照的副本（勾选状态要能独立改动），回传的自然不是同一批对象，
      // 因此按 id 认领批准项——用 includes(entry) 比对象恒为 false，勾了也不会执行。
      const approvedIds = new Set(
        (writes.length ? await confirmToolCalls(writes) : []).map(call => call.id),
      )
      for (const entry of entries) {
        if (!entry.validation.ok) {
          // 参数都不合法（模型编造了平台 / 模型 id）：不执行，把原因回给模型。
          entry.status = 'done'
          entry.result = { ok: false, error: entry.validation.error }
          continue
        }
        if (!entry.write) {
          entry.status = 'running'
          entry.result = await runTool(entry.name, entry.args, { state: props.state })
          entry.status = 'done'
          scrollBottom()
          continue
        }
        if (!approvedIds.has(entry.id)) {
          entry.status = 'skipped'
          entry.result = { ok: false, error: '用户未批准该操作' }
          continue
        }
        entry.status = 'running'
        entry.result = await runTool(entry.name, entry.args, { state: props.state })
        entry.status = 'done'
        scrollBottom()
      }
      for (const entry of entries) {
        request.push(toolResultMessage(entry.id, entry.result))
      }
      // 壳侧有 40 条消息的上限（AI_MAX_MESSAGES）：多轮工具循环每轮都追加 assistant +
      // N 条 tool 结果，长对话必然撞上。按「整块」裁掉最旧的几轮，并在气泡里如实说明。
      const dropped = trimRequest(request)
      if (dropped > 0) live.trimmed = (live.trimmed || 0) + dropped
    }
  } catch (error) {
    live.failed = true
    live.content = (live.content ? live.content + '\n\n' : '') + `⚠ ${error?.message || error}`
  } finally {
    if (flushTimer) clearTimeout(flushTimer)
    live.pending = false
    chatting.value = false
    scrollBottom()
  }
}

function stop() {
  abortChat()
}

// Enter 发送（Shift / Cmd / Ctrl + Enter 换行）；输入法组词中不触发，避免拼音上屏即误发。
function onComposerKeydown(event) {
  if (event.isComposing) return
  if (event.key === 'Enter' && !event.shiftKey && !event.metaKey && !event.ctrlKey) {
    event.preventDefault()
    send()
  }
}

// Esc 统一收口：先关提案确认（等同全部取消），再关设置。弹窗内任意焦点都能触发。
function onWindowKeydown(event) {
  if (event.key !== 'Escape') return
  if (confirming.value) {
    settleConfirm([])
    return
  }
  if (showSettings.value) showSettings.value = false
}

onMounted(() => window.addEventListener('keydown', onWindowKeydown))

function clearHistory() {
  if (chatting.value) return
  messages.value = []
  lastTurn = null
  dismissNotice()
}

function quick(text) {
  if (chatting.value) return
  draft.value = text
  send()
}

function render(content) {
  return renderMarkdownLite(content)
}

// 失败气泡上的「重试」只给最新一轮：更早的失败已被后续对话取代，重发会把话题拉回过去。
function canRetry(message) {
  return !chatting.value && message.failed && lastTurn !== null && messages.value[messages.value.length - 1] === message
}

function toolStatusText(entry) {
  if (entry.status === 'running') return '执行中…'
  if (entry.status === 'skipped') return '未执行'
  return entry.result?.ok ? '已完成' : '失败'
}

// 工具结果折叠区的正文：默认收起（模型已经拿到并转述过，展开是给人核对用的）。
// 上限与 ai.js 里 read_log 的截断同口径，避免把 8KB 日志一次性铺进 DOM。
function resultText(entry) {
  const result = entry.result
  if (!result) return ''
  if (result.ok === false) return String(result.error ?? '执行失败')
  let text
  try {
    text = JSON.stringify(result.result ?? result, null, 1)
  } catch {
    text = String(result.result ?? '')
  }
  if (entry.name === 'read_log') {
    // 日志正文单独成形：整段 JSON 化会把换行变成 \n，读起来比原始日志更难。
    const log = result.result || {}
    text = `${log.truncated ? '（已截断，原始 ' + log.bytes + ' 字节）\n' : ''}${String(log.text ?? '')}`
  }
  return text.length > 8000 ? `${text.slice(0, 8000)}\n…（其余省略）` : text
}

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onWindowKeydown)
  if (noticeTimer) clearTimeout(noticeTimer)
  if (chatting.value) abortChat()
})
</script>

<template>
  <section class="view assistant" aria-label="AI 助手" :aria-hidden="showSettings || confirming ? 'true' : null">
    <header>
      <div>
        <h2>AI 助手</h2>
        <p class="subtitle">
          用自然语言管理 WB Bridge：读状态与日志、读取与检测模型、导入插件、配置平台 Key、管环境。
          只读操作立即执行，写入操作必须先经你逐条确认。
        </p>
      </div>
      <div class="actions">
        <button type="button" @click="openSettings">设置</button>
        <button type="button" class="ghost" :disabled="chatting" title="清空当前对话（内容本来也不落盘）" @click="clearHistory">清空</button>
      </div>
    </header>

    <p v-if="!coreReady" class="assistant-warn" role="status">
      核心当前{{ phaseLabel(state.phase) }}{{ state.message ? `（${state.message}）` : '' }}：
      {{ config.brain === 'self'
        ? '本机大脑要用正在运行的核心作为模型服务，未就绪时发不出去。可回「模型与服务」启动服务，或在「设置」改用外部端点。'
        : '外部端点仍可正常问答，但助手代管不了本应用的服务。' }}
    </p>

    <div ref="chatBox" class="assistant-chat" role="log" aria-label="对话记录" @scroll.passive="onChatScroll">
      <article v-if="!messages.length" class="assistant-empty">
        <strong>开始一段新对话</strong>
        <p>
          我可以读懂服务状态、定位模型不可用原因、执行读取与检测、把可用模型导入 WorkBuddy / CodeBuddy，
          也能查环境、打开平台申请页、检查并安装更新。当前大脑：<code>{{ brainLabel }}</code>。
        </p>
        <div class="assistant-quick" role="group" aria-label="常用提问">
          <button
            v-for="prompt in QUICK_PROMPTS"
            :key="prompt"
            type="button"
            class="assistant-chip"
            :disabled="chatting"
            @click="quick(prompt)"
          >{{ prompt }}</button>
        </div>
        <p class="assistant-privacy">
          对话内容只保存在本窗口内存中，关闭或重启即清空；API Key 不写入磁盘，重启后需在「设置」重新填写。
          本机大脑使用 WB Bridge 自己的免费模型（默认 <code>{{ AUTO_MODEL_ID }}</code>：按每次请求内容自动挑选），
          无需任何 Key，但会消耗少量免费额度。
        </p>
      </article>

      <article v-for="m in messages" :key="m.id" class="assistant-msg" :class="[m.role, { failed: m.failed }]">
        <div class="assistant-msg-meta">
          <span>{{ m.role === 'user' ? '我' : 'AI 助手' }}</span>
          <span v-if="m.role === 'assistant' && m.pending" class="assistant-thinking">正在生成…</span>
          <button v-else-if="canRetry(m)" type="button" class="assistant-retry" @click="retry">重试</button>
        </div>
        <div class="assistant-bubble">
          <!-- renderMarkdownLite 先整段转义再解析，原始 HTML 不会成为标签 -->
          <div class="assistant-text" v-html="render(m.content)" />
          <span v-if="m.pending" class="assistant-cursor">▍</span>
        </div>
        <p v-if="m.trimmed" class="assistant-trimmed">（为控制上下文长度，已丢弃最早的 {{ m.trimmed }} 条消息）</p>
        <ul v-if="m.tools && m.tools.length" class="assistant-tools">
          <li v-for="entry in m.tools" :key="entry.id" class="assistant-tool" :class="[entry.write ? 'write' : 'read', entry.danger ? 'danger' : '', entry.status]">
            <div class="assistant-tool-head">
              <span class="assistant-tool-tag" :class="entry.write ? (entry.danger ? 'danger' : 'write') : 'read'">{{ entry.write ? (entry.danger ? '高危写入' : '写入') : '只读' }}</span>
              <span class="assistant-tool-label">{{ entry.label }}</span>
              <span class="assistant-tool-status" :class="entry.status">{{ toolStatusText(entry) }}</span>
            </div>
            <div class="assistant-tool-summary">{{ entry.summary }}</div>
            <div v-if="entry.validation && !entry.validation.ok" class="assistant-tool-error">参数校验未通过：{{ entry.validation.error }}</div>
            <div v-else-if="entry.preview && Object.keys(entry.preview).length" class="assistant-tool-preview">
              <span v-for="(value, key) in entry.preview" :key="key">{{ key }}：{{ value }}</span>
            </div>
            <div v-if="entry.result && entry.result.ok === false && entry.status !== 'skipped'" class="assistant-tool-error">执行失败：{{ entry.result.error }}</div>
            <!-- 结果默认收起：模型已经拿到并转述过，展开是给人逐条核对用的。 -->
            <details v-else-if="entry.result && resultText(entry)" class="assistant-tool-result">
              <summary>查看返回结果</summary>
              <pre>{{ resultText(entry) }}</pre>
            </details>
          </li>
        </ul>
      </article>
    </div>

    <p v-if="notice" class="assistant-keyhint assistant-notice" role="status">
      <span>{{ notice }}</span>
      <button type="button" class="ghost" aria-label="关闭提示" @click="dismissNotice">✕</button>
    </p>
    <p v-else-if="needsKey && !config.apiKey" class="assistant-keyhint" role="status">
      当前端点需要 API Key：点右上角「设置」填写（仅本会话内存，不落盘；本地 Ollama 一类端点可留空）。
    </p>
    <p v-else-if="selfChatOnly" class="assistant-keyhint" role="status">
      本机模型 <code>{{ config.selfModel }}</code> 是「仅对话」模型：本轮不声明工具，助手只能回答问题、不会执行操作。
      要让它代管服务，请在「设置」改选支持工具的模型，或直接选 <code>{{ AUTO_MODEL_ID }}</code>。
    </p>
    <p v-else-if="config.tools === false" class="assistant-keyhint" role="status">
      已在「设置」关闭「允许 AI 调用管理工具」：助手只问答、不执行任何操作。
    </p>

    <form class="assistant-composer" @submit.prevent="send">
      <textarea
        ref="composer"
        v-model="draft"
        rows="1"
        :disabled="chatting"
        placeholder="例如：现在服务正常吗？哪些模型不可用？（Enter 发送，Shift + Enter 换行）"
        aria-label="向 AI 助手提问"
        @keydown="onComposerKeydown"
      />
      <div class="assistant-composer-actions">
        <!-- 上下文徽标说清「这一轮实际会带上什么」：大脑、工具开关、快照新鲜度都是排查时的第一问。 -->
        <span class="assistant-context" :title="`大脑：${brainLabel}`">
          <span class="assistant-dot" :class="coreReady ? 'ok' : 'warn'" />
          {{ brainLabel }}
          <em>· 工具{{ toolsEnabled ? '可用' : '关闭' }}</em>
          <em v-if="!toolsEnabled && config.tools !== false">（模型为仅对话）</em>
        </span>
        <button v-if="chatting" type="button" class="ghost" @click="stop">停止</button>
        <button v-else type="submit" class="primary" :disabled="!draft.trim() || props.busy">
          <span v-if="props.busy" class="spinner" />发送
        </button>
      </div>
    </form>
  </section>

  <!-- 设置弹窗 -->
  <div v-if="showSettings" class="assistant-mask" @click.self="showSettings = false">
    <div ref="settingsPanel" class="assistant-dialog" role="dialog" aria-modal="true" aria-label="AI 设置" tabindex="-1">
      <header class="assistant-dialog-head">
        <strong>AI 设置</strong>
        <button type="button" class="ghost" aria-label="关闭设置" @click="showSettings = false">✕</button>
      </header>
      <div class="assistant-dialog-body">
        <div class="field">
          <label class="f">大脑（谁来回答问题）</label>
          <div class="assistant-brains">
            <label class="assistant-brain" :class="{ active: settingsForm.brain === 'self' }">
              <input v-model="settingsForm.brain" type="radio" value="self">
              <span>
                <strong>本机免费模型</strong>
                <em>用 WB Bridge 自己的桥接服务（OpenCode 免费模型）。无需任何 Key，端点与令牌由本应用自己持有；会消耗少量免费额度。</em>
              </span>
            </label>
            <label class="assistant-brain" :class="{ active: settingsForm.brain === 'external' }">
              <input v-model="settingsForm.brain" type="radio" value="external">
              <span>
                <strong>外部端点</strong>
                <em>任意 OpenAI 兼容服务（DeepSeek / 智谱 / 通义千问 / 本地 Ollama…）。需要该服务的 API Key（仅本会话内存）。</em>
              </span>
            </label>
          </div>
        </div>

        <template v-if="settingsForm.brain === 'self'">
          <div class="field">
            <label class="f" for="aiSelfModel">本机模型</label>
            <select id="aiSelfModel" v-model="settingsForm.selfModel">
              <option v-for="opt in selfOptions" :key="opt.value" :value="opt.value">{{ opt.label }}</option>
            </select>
            <p class="note">默认「WB · auto」：由核心按每次请求的内容在可用模型里自动选一个，工具类请求只会发给支持工具的模型，不必手动挑 id。名单与可用性由上游决定；指定模型不可用时换一个再试。</p>
          </div>
        </template>

        <template v-else>
          <div class="field">
            <label class="f">快捷预设</label>
            <div class="assistant-presets">
              <button v-for="preset in AI_PRESETS" :key="preset.baseUrl" type="button" class="assistant-chip" @click="applyPreset(preset)">{{ preset.label }}</button>
            </div>
            <p class="note">点击自动填入地址与模型名，也可手填任意 OpenAI 兼容端点。</p>
          </div>
          <div class="field">
            <label class="f" for="aiBaseUrl">服务地址 Base URL</label>
            <input id="aiBaseUrl" v-model="settingsForm.baseUrl" type="text" autocomplete="off" spellcheck="false" placeholder="https://api.deepseek.com/v1">
          </div>
          <div class="field">
            <label class="f" for="aiModel">模型名称</label>
            <input id="aiModel" v-model="settingsForm.model" type="text" autocomplete="off" spellcheck="false" placeholder="deepseek-chat">
          </div>
          <div class="field">
            <label class="f" for="aiApiKey">API Key</label>
            <input id="aiApiKey" v-model="settingsForm.apiKey" type="password" autocomplete="off" spellcheck="false" placeholder="仅本会话内存，不落盘；本地端点可留空">
          </div>
        </template>

        <div class="field">
          <label class="assistant-opt">
            <input v-model="settingsForm.tools" type="checkbox">
            <span>允许 AI 调用管理工具（查状态 / 读日志 / 读取与检测模型 / 导入插件 / 保存平台 Key…）</span>
          </label>
          <p class="note">关闭后 AI 只能问答与给建议，不能执行任何操作。写入类工具无论如何都会先弹确认框，勾选后才执行。</p>
        </div>

        <p v-if="settingsError" class="assistant-dialog-error" role="alert">{{ settingsError }}</p>
        <p class="note">请求由本应用代理发出（壳侧仅允许 https 或本机 http 端点，本机大脑只走回环）；Key 随请求参数进入进程内存，不进日志、不进存储、不出现在任何界面。端点、大脑与模型名会记住，Key 不会。</p>
      </div>
      <footer class="assistant-dialog-foot">
        <button type="button" class="ghost" @click="showSettings = false">取消</button>
        <button type="button" class="primary" @click="saveSettings">保存</button>
      </footer>
    </div>
  </div>

  <!-- 工具提案确认弹窗 -->
  <div v-if="confirming" class="assistant-mask" @click.self="settleConfirm([])">
    <div ref="confirmPanel" class="assistant-dialog" role="dialog" aria-modal="true" aria-label="确认 AI 操作提案" tabindex="-1">
      <header class="assistant-dialog-head">
        <strong>确认 AI 操作提案</strong>
        <button type="button" class="ghost" aria-label="取消全部" @click="settleConfirm([])">✕</button>
      </header>
      <div class="assistant-dialog-body">
        <p class="note">以下是 AI 起草的操作。勾选后点「执行选中操作」；取消则一条都不执行（AI 会收到「用户未批准」的反馈）。</p>
        <p v-if="confirming.calls.some(c => c.danger)" class="assistant-danger-hint" role="alert">
          标了「高危写入」的条目会停止服务或装新版本并重启应用，执行后本窗口可能随之关闭；不勾选即可安全跳过。
        </p>
        <ul class="assistant-ops">
          <li v-for="call in confirming.calls" :key="call.id" class="assistant-op" :class="{ invalid: !call.validation.ok }">
            <label class="assistant-op-check">
              <input v-model="call.checked" type="checkbox" :disabled="!call.validation.ok">
            </label>
            <div class="assistant-op-main">
              <div class="assistant-op-title">
                <span class="assistant-tool-tag" :class="call.danger ? 'danger' : 'write'">{{ call.danger ? '高危写入' : '写入' }}</span>
                {{ call.summary }}
              </div>
              <div v-if="call.validation.ok && Object.keys(call.preview).length" class="assistant-tool-preview">
                <span v-for="(value, key) in call.preview" :key="key">{{ key }}：{{ value }}</span>
              </div>
              <div v-else-if="!call.validation.ok" class="assistant-tool-error">参数校验未通过：{{ call.validation.error }}</div>
            </div>
          </li>
        </ul>
        <label class="assistant-op-all">
          <input type="checkbox" :checked="allChecked()" @change="toggleAll"> 全选
        </label>
      </div>
      <footer class="assistant-dialog-foot">
        <button type="button" class="ghost" @click="settleConfirm([])">全部取消</button>
        <button type="button" class="primary" :disabled="!confirming.calls.some(c => c.checked && c.validation.ok)" @click="runApproved">执行选中操作</button>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.assistant { gap: var(--sp-3); }
header { display: flex; justify-content: space-between; gap: var(--sp-3); align-items: center; }
.subtitle { margin: 0; color: var(--muted-strong); font-size: var(--fs-sm); max-width: 720px; }
.actions { display: flex; gap: 8px; }

/* 对话区：填满主区并自行滚动 */
.assistant-chat {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  padding: var(--sp-2);
}
.assistant-empty {
  margin: auto;
  max-width: 560px;
  text-align: center;
  color: var(--muted-strong);
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.assistant-empty strong { font-size: var(--fs-lg); color: var(--text); }
.assistant-empty p { margin: 0; font-size: var(--fs-sm); line-height: var(--lh-loose); }
.assistant-empty code { font-family: ui-monospace, monospace; font-size: 11px; background: var(--row); padding: 1px 5px; border-radius: 4px; }
.assistant-quick { display: flex; flex-wrap: wrap; gap: 8px; justify-content: center; }
.assistant-chip {
  font-size: var(--fs-xs);
  padding: 6px 12px;
  border-radius: 999px;
  border: 1px solid var(--line);
  background: var(--row);
  color: var(--muted-strong);
  cursor: pointer;
  white-space: normal;
  text-align: left;
}
.assistant-chip:hover:not(:disabled) { border-color: color-mix(in srgb, var(--green) 40%, var(--line)); background: var(--green-bg); color: var(--text); }
.assistant-chip:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.assistant-chip:disabled { opacity: .6; cursor: not-allowed; }
.assistant-privacy { font-size: 11px; color: var(--muted-strong); }

.assistant-msg { display: flex; flex-direction: column; gap: 4px; max-width: 86%; }
.assistant-msg.user { align-self: flex-end; align-items: flex-end; }
.assistant-msg.assistant { align-self: flex-start; }
.assistant-msg-meta { font-size: 11px; color: var(--muted-strong); display: flex; align-items: center; gap: 8px; }
/* 「正在生成…」与「重试」是元信息行的两种终态：前者只在 pending 时出现，后者只在失败且可重发时出现 */
.assistant-thinking { color: var(--green); }
.assistant-retry {
  font-size: 11px;
  padding: 0;
  border: none;
  background: none;
  color: var(--orange);
  text-decoration: underline;
  text-underline-offset: 2px;
  cursor: pointer;
}
.assistant-retry:hover { color: var(--text); }
.assistant-retry:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); border-radius: var(--radius-s); }
.assistant-bubble {
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
  padding: 10px 12px;
  background: var(--row);
  font-size: var(--fs-sm);
  line-height: var(--lh-loose);
  overflow-wrap: anywhere;
}
.assistant-msg.user .assistant-bubble { background: var(--green-bg); border-color: color-mix(in srgb, var(--green) 30%, transparent); }
.assistant-msg.assistant .assistant-bubble { background: var(--surface-raised); }
.assistant-msg.failed .assistant-bubble { border-color: color-mix(in srgb, var(--badge-unavailable-fg) 45%, var(--line)); }
.assistant-text :deep(p) { margin: 0 0 6px; }
.assistant-text :deep(p:last-child) { margin-bottom: 0; }
.assistant-text :deep(code) { font-family: ui-monospace, monospace; font-size: 12px; background: var(--panel); padding: 1px 5px; border-radius: 4px; }
.assistant-text :deep(pre.ai-code) {
  margin: 6px 0;
  padding: 8px 10px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius-s);
  overflow-x: auto;
}
.assistant-text :deep(pre.ai-code code) { background: none; padding: 0; }
.assistant-cursor { color: var(--green); animation: assistant-blink 1s steps(2) infinite; }
@keyframes assistant-blink { 50% { opacity: 0; } }

/* 工具执行卡片 */
.assistant-tools { list-style: none; margin: 4px 0 0; padding: 0; display: flex; flex-direction: column; gap: 6px; width: 100%; }
.assistant-tool {
  border: 1px solid var(--line);
  border-left: 3px solid var(--muted);
  border-radius: var(--radius-s);
  background: var(--panel);
  padding: 8px 10px;
  font-size: var(--fs-xs);
}
.assistant-tool.write { border-left-color: var(--orange); }
.assistant-tool.danger { border-left-color: var(--badge-unavailable-fg); }
.assistant-tool-head { display: flex; align-items: center; gap: 8px; }
.assistant-tool-tag {
  font-size: 10px; font-weight: 600; white-space: nowrap;
  padding: 2px 7px; border-radius: 999px;
  background: var(--row); color: var(--muted-strong);
}
.assistant-tool-tag.write { background: var(--orange-bg); color: var(--orange); }
.assistant-tool-tag.danger { background: color-mix(in srgb, var(--badge-unavailable-fg) 16%, transparent); color: var(--badge-unavailable-fg); }
.assistant-tool-label { font-weight: 600; color: var(--text); }
.assistant-tool-status { margin-left: auto; color: var(--muted); }
.assistant-tool-status.done { color: var(--green); }
.assistant-tool-status.skipped { color: var(--muted); }
.assistant-tool-status.running { color: var(--orange); }
.assistant-tool-summary { margin-top: 4px; color: var(--muted-strong); }
.assistant-tool-preview { margin-top: 4px; display: flex; flex-wrap: wrap; gap: 10px; color: var(--muted-strong); font-family: ui-monospace, monospace; }
.assistant-tool-error { margin-top: 4px; color: var(--badge-unavailable-fg); }
/* 原始返回默认收起：模型已读过并转述过，展开是给人逐条核对用的 */
.assistant-tool-result { margin-top: 4px; }
.assistant-tool-result > summary {
  cursor: pointer;
  color: var(--muted);
  font-size: 11px;
}
.assistant-tool-result > summary:hover { color: var(--text); }
.assistant-tool-result > summary:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); border-radius: var(--radius-s); }
.assistant-tool-result > pre {
  margin: 6px 0 0;
  padding: 8px 10px;
  max-height: 260px;
  overflow: auto;
  background: var(--row);
  border: 1px solid var(--line);
  border-radius: var(--radius-s);
  font-family: ui-monospace, monospace;
  font-size: 11px;
  line-height: var(--lh-loose);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.assistant-trimmed { margin: 2px 0 0; font-size: 11px; color: var(--muted-strong); }

/* 设置弹窗：大脑二选一 + 工具开关 */
.assistant-brains { display: flex; flex-direction: column; gap: 8px; }
.assistant-brain {
  display: flex; gap: 10px; align-items: flex-start;
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--row); padding: 10px 12px; cursor: pointer;
}
.assistant-brain.active { border-color: color-mix(in srgb, var(--green) 45%, var(--line)); background: var(--green-bg); }
.assistant-brain input { margin-top: 3px; }
.assistant-brain span { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.assistant-brain strong { font-size: var(--fs-sm); }
.assistant-brain em { font-style: normal; font-size: 11px; line-height: var(--lh-loose); color: var(--muted-strong); }
.assistant-opt { display: flex; gap: 8px; align-items: flex-start; font-size: var(--fs-sm); cursor: pointer; }
.assistant-opt input { margin-top: 3px; }

/* 三条说明横幅共用一套视觉：未就绪（.assistant-warn）、需要 Key / 工具状态（.assistant-keyhint）、
   这一轮被拦下（.assistant-notice）。差异只由模板上的类组合决定，不再复制一遍样式。 */
.assistant-keyhint,
.assistant-warn {
  margin: 0; flex-shrink: 0;
  font-size: var(--fs-xs); line-height: var(--lh-loose);
  color: var(--orange);
  background: var(--orange-bg);
  border: 1px solid color-mix(in srgb, var(--orange) 35%, transparent);
  border-radius: var(--radius-s);
  padding: 8px 10px;
}
/* 拦下这次发送的一次性提示：带关闭按钮，8s 后自动消隐 */
.assistant-notice { display: flex; align-items: flex-start; gap: 8px; }
.assistant-notice > button { flex-shrink: 0; padding: 0 4px; line-height: 1.4; }

/* 输入区 */
.assistant-composer { flex-shrink: 0; display: flex; flex-direction: column; gap: 8px; }
.assistant-composer textarea {
  font: inherit;
  resize: none;
  padding: 10px 12px;
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
  background: var(--surface-raised);
  color: var(--text);
  /* 高度由 autoGrow() 按内容写内联值：下限一行的呼吸空间，上限到顶后自身出滚动条，
     否则多行提问会被藏进看不见的位置。 */
  min-height: 42px;
  max-height: 180px;
  overflow-y: auto;
}
.assistant-composer textarea:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.assistant-composer textarea:disabled { opacity: .6; }
.assistant-composer-actions { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.assistant-context { font-size: 11px; color: var(--muted-strong); display: flex; align-items: center; gap: 6px; min-width: 0; overflow-wrap: anywhere; }
.assistant-context em { font-style: normal; color: var(--muted); }
/* 服务就绪与否用颜色点直接标在输入区上方：助手能不能代管操作，取决于这一个状态 */
.assistant-dot { flex-shrink: 0; width: 8px; height: 8px; border-radius: 50%; background: var(--muted); }
.assistant-dot.ok { background: var(--green); }
.assistant-dot.warn { background: var(--orange); }

/* 弹窗（设置 / 提案确认共用） */
.assistant-mask {
  position: fixed; inset: 0; z-index: 20;
  background: rgb(0 0 0 / 32%);
  display: flex; align-items: center; justify-content: center;
  padding: var(--sp-5);
}
.assistant-dialog {
  width: min(560px, 100%);
  max-height: 100%;
  display: flex; flex-direction: column;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius-l);
  box-shadow: var(--shadow-m);
  overflow: hidden;
}
.assistant-dialog-head {
  display: flex; align-items: center; justify-content: space-between;
  padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--line);
}
.assistant-dialog-body { padding: var(--sp-4); overflow-y: auto; display: flex; flex-direction: column; gap: var(--sp-3); }
.assistant-dialog-foot {
  display: flex; justify-content: flex-end; gap: 8px;
  padding: var(--sp-3) var(--sp-4);
  border-top: 1px solid var(--line);
}
.field { display: flex; flex-direction: column; gap: 6px; }
.field .f { font-size: var(--fs-sm); font-weight: 600; }
/* 只给文本类控件描边与内距：checkbox/radio 继承这些会退化成正方形色块 */
.field input:not([type=checkbox]):not([type=radio]),
.field select {
  font: inherit; font-size: var(--fs-sm);
  padding: 8px 10px;
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--surface-raised); color: var(--text);
}
.field input:not([type=checkbox]):not([type=radio]):focus-visible,
.field select:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
/* 原生 checkbox/radio 也必须有可见焦点环（键盘用户在弹窗里靠 Tab 走） */
.assistant-brain input:focus-visible,
.assistant-opt input:focus-visible,
.assistant-op-check input:focus-visible,
.assistant-op-all input:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.assistant-presets { display: flex; flex-wrap: wrap; gap: 8px; }
.note { margin: 0; font-size: 11px; color: var(--muted-strong); line-height: var(--lh-loose); }
.assistant-dialog-error { margin: 0; font-size: var(--fs-xs); color: var(--badge-unavailable-fg); }
.assistant-danger-hint {
  margin: 0; font-size: var(--fs-xs); line-height: var(--lh-loose);
  color: var(--badge-unavailable-fg);
  background: color-mix(in srgb, var(--badge-unavailable-fg) 12%, transparent);
  border: 1px solid color-mix(in srgb, var(--badge-unavailable-fg) 35%, transparent);
  border-radius: var(--radius-s);
  padding: 8px 10px;
}

.assistant-ops { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
.assistant-op {
  display: flex; gap: 10px;
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--row); padding: 10px 12px;
}
.assistant-op.invalid { opacity: .6; }
.assistant-op-check { display: flex; align-items: flex-start; padding-top: 2px; }
.assistant-op-main { min-width: 0; }
.assistant-op-title { font-size: var(--fs-sm); font-weight: 600; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.assistant-op-all { font-size: var(--fs-xs); color: var(--muted-strong); display: flex; align-items: center; gap: 6px; }
</style>
