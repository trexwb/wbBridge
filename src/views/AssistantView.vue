<script setup>
// AI 助手视图：自然语言管理 WB Bridge。
// 分工：src/core/ai.js 出协议与纯逻辑（prompt / 工具 schema / 校验 / 渲染），本视图只做
// 交互与回合循环。安全叙事与 fastenerTradeWorkbench 的 AI 抽屉一致：
//   • 只读工具立即执行；写入类工具只是**提案**，必须经用户逐条勾选确认后才执行；
//   • API Key 只在会话内存里（设置弹窗填写，不落盘、不回显、不进历史），请求时随命令
//     参数进壳；壳侧另有端点白名单复核；
//   • 对话内容默认记在本机 IndexedDB（src/core/chatSession.js），重开与切换视图都不丢；
//     落盘前丢弃工具入参（可能含平台 Key 明文），可在「设置」整项关掉。
//
// 🔴 会话状态（messages / chatting / confirming / draft）全部取自 chatSession 的模块级单例：
// 面板用 v-if 切视图，切走即卸载本组件。状态若放在组件里，一轮生成还没回来就被一起销毁；
// 放在模块里则可以「生成中切走、回来续上」，包括正等着用户勾选的提案确认框。
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import {
  chatting,
  clearConversation,
  confirming,
  coreSnapshot,
  draft,
  ensureHydrated,
  lastTurn,
  messages,
  persistEnabled,
  schedulePersist,
  setPersist,
} from '../core/chatSession.js'
// 桥接层只有一个在飞 chatId：助手的请求与提示词优化的请求共用它，
// 两边同时发起会互相覆盖、连「停止」都会停错对象，因此在入口处互相排斥。
import { running as promptRunning } from '../core/promptSession.js'
import MarkdownBlock from '../components/MarkdownBlock.vue'
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

// 视图跳转（「去设置 / 去『模型与服务』」这类出路），由 App.vue 换成实际的 view 名。
const emit = defineEmits(['navigate'])

const QUICK_PROMPTS = [
  '现在服务状态怎么样？有哪些模型可用？',
  '哪些模型不可用？帮我看最近日志找原因。',
  '帮我重新读取免费模型，然后检测全部。',
  '智谱平台的 Key 怎么配置？我有 Key，帮我保存。',
  '本机数据目录和插件配置文件都在哪里？',
  '现在有新版本可以更新吗？',
]

// ── 本地界面状态（会话状态已上移到 chatSession） ────────────────────
const config = reactive({ ...currentConfig(), apiKey: '' })
const showSettings = ref(false)
const settingsForm = reactive({ brain: 'self', baseUrl: '', model: '', selfModel: AUTO_MODEL_ID, apiKey: '', tools: true, persist: true })
const settingsError = ref('')
const notice = ref('') // 输入框上方的一次性说明（拦下一次发送时给原因，不进对话历史）
let noticeTimer = null
const chatBox = ref(null) // 对话区容器：滚动到底要用它，而不是 document.querySelector
const composer = ref(null) // 输入框：自动增高 + 发送后重新聚焦
const settingsPanel = ref(null)
const settingsErrorBox = ref(null)
const confirmPanel = ref(null)
// Key 明文的临时显示（与「平台」视图的眼睛按钮同一套交互），每次打开设置都回到隐藏态。
const revealKey = ref(false)
// 弹窗关闭后焦点要回到打开它的控件：否则键盘用户按完 Tab 就从页面开头重新找位置。
// 程序化打开（校验失败自动弹设置）没有触发元素，回落输入框。
let settingsTrigger = null
// 用户往上翻看历史时不再强行拽回底部，但必须给一条明确的路回到底部（见 .assistant-jump）。
const atBottom = ref(true)
const pendingBelow = ref(false)
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

// ── 使用门槛：模型就位才可用 ────────────────────────────────────────
// 「就位」= 这一轮真发得出去。外部端点要求地址 / 模型 / Key 齐备；本机大脑要求核心**就绪**
// 且至少有一个探测通过的模型 —— 池空时连 `WB · auto` 都没有可转发的落点，请求必然失败，
// 让用户对着一个注定失败的输入框打字没有意义。
// 门槛只拦输入（输入框与发送一起禁用），不动「设置」：改大脑来源始终是可走的那条出路。
const gate = computed(() => {
  if (config.brain === 'external') {
    const error = validateConfig({ baseUrl: config.baseUrl, model: config.model })
    if (error) return { blocked: true, title: '外部端点还没配好', reason: error, fix: 'settings' }
    if (needsKey.value && !config.apiKey) {
      return {
        blocked: true,
        title: '还缺这个端点的 API Key',
        reason: '在「设置」填写即可开始（只留在本会话内存，不落盘）；本地 Ollama 一类端点可把地址改成本机后留空。',
        fix: 'settings',
      }
    }
    return { blocked: false }
  }
  if (!coreReady.value) {
    return {
      blocked: true,
      title: `核心${phaseLabel(props.state.phase)}，本机大脑还不能用`,
      reason: '本机大脑要用正在运行的核心作为模型服务。先到「模型与服务」把服务起起来，或在「设置」改用外部端点。',
      fix: 'models',
    }
  }
  const total = modelIds.value.length
  const available = (Array.isArray(props.state.availableModels) ? props.state.availableModels : []).length
  if (!total) {
    return {
      blocked: true,
      title: '还没读取到免费模型',
      reason: '先到「模型与服务」点「读取免费模型」，名单到位后助手才有模型可用。',
      fix: 'models',
    }
  }
  if (!available) {
    const probe = props.state.probe
    const pending = Array.isArray(probe?.pending) ? probe.pending.length : null
    const progress = probe?.running && Number.isFinite(pending)
      ? `（${Math.max(0, Math.min(total, total - pending))}/${total}）`
      : ''
    return {
      blocked: true,
      title: probe?.running ? '模型正在检测，还没有可用的' : '没有探测通过的模型',
      reason: probe?.running
        ? `已发现 ${total} 个模型，正在检测${progress}。检测通过后助手才用得上本机大脑，稍等即可。`
        : `已发现 ${total} 个模型，但当前 0 个探测通过。到「模型与服务」点「检测全部」，或在「设置」改用外部端点。`,
      fix: 'models',
    }
  }
  return { blocked: false }
})

const blocked = computed(() => gate.value.blocked === true)

const composerPlaceholder = computed(() =>
  blocked.value
    ? `${gate.value.title}：模型就位后可在这里提问`
    : '例如：现在服务正常吗？哪些模型不可用？（Enter 发送，Shift + Enter 换行）',
)

// 粘底滚动：只有用户本来就贴在底部时才跟着新内容走。
// 助手在读日志 / 逐模型列表时一次推很长一段，无条件滚到底会把用户往上翻的位置强行拽走，
// 而「悄悄不滚」比「强行滚动」更符合预期——但「悄悄不滚」必须补一个回去的入口，
// 否则用户不知道回复已经结束了（见模板里的 .assistant-jump）。
function onChatScroll() {
  const box = chatBox.value
  if (!box) return
  atBottom.value = box.scrollHeight - box.scrollTop - box.clientHeight < 48
  if (atBottom.value) pendingBelow.value = false
}

function scrollBottom() {
  if (!atBottom.value) {
    pendingBelow.value = true
    return
  }
  nextTick(() => {
    const box = chatBox.value
    if (box) box.scrollTop = box.scrollHeight
  })
}

function jumpToBottom() {
  atBottom.value = true
  pendingBelow.value = false
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
function openSettings(event) {
  // 记住触发元素（程序化调用没有事件：校验失败自动弹设置时回落到输入框）。
  settingsTrigger = event && event.currentTarget ? event.currentTarget : null
  settingsForm.brain = config.brain
  settingsForm.baseUrl = config.baseUrl
  settingsForm.model = config.model
  settingsForm.selfModel = config.selfModel
  settingsForm.apiKey = config.apiKey
  settingsForm.tools = config.tools !== false
  settingsForm.persist = persistEnabled.value
  settingsError.value = ''
  revealKey.value = false
  showSettings.value = true
  // 弹窗是可聚焦容器（tabindex=-1）：打开即取焦点，Tab 才从内容开始走，
  // Esc 也才有明确的收起目标（这里的 Esc 仍由 window 级监听兜住）。
  nextTick(() => settingsPanel.value && settingsPanel.value.focus())
}

// 所有关闭路径（保存 / 取消 / ✕ / 点遮罩 / Esc）都走这一处：收起后把焦点还给打开它的控件，
// 否则键盘用户每关一次弹窗都要从页面开头重新找位置。
function closeSettings() {
  showSettings.value = false
  revealKey.value = false
  const back = settingsTrigger
  settingsTrigger = null
  nextTick(() => {
    const target = back && back.isConnected ? back : composer.value
    if (target && !target.disabled) target.focus()
  })
}

// 表单与当前生效配置的差异：改动未保存时明确标出来，让「点遮罩 / 按 Esc 就丢弃」这件事看得见。
// 刻意不弹二次确认——这是几个端点字段，不是不可逆动作，确认框反而更挡路。
const settingsDirty = computed(() => {
  const same =
    settingsForm.brain === config.brain &&
    settingsForm.baseUrl.trim().replace(/\/+$/, '') === config.baseUrl &&
    settingsForm.model.trim() === config.model &&
    settingsForm.selfModel.trim() === config.selfModel &&
    (settingsForm.tools !== false) === (config.tools !== false) &&
    settingsForm.persist === persistEnabled.value &&
    settingsForm.apiKey === config.apiKey
  return !same
})

function applyPreset(preset) {
  settingsForm.brain = 'external'
  settingsForm.baseUrl = preset.baseUrl
  settingsForm.model = preset.model
}

// 报错落到弹窗正文末尾，而正文是可滚动的：用户在顶部点「保存」时，
// 地址栏那一类的错误会出现在看不见的地方（role="alert" 会念给读屏，但眼睛找不到）。
// 所以每次落错误都把它滚进视野。
function showSettingsError(message) {
  settingsError.value = message
  nextTick(() => settingsErrorBox.value && settingsErrorBox.value.scrollIntoView({ block: 'nearest' }))
}

function saveSettings() {
  if (settingsForm.brain === 'external') {
    const error = validateConfig({ baseUrl: settingsForm.baseUrl, model: settingsForm.model })
    if (error) {
      showSettingsError(error)
      return
    }
  } else {
    const error = validateSelfConfig({ selfModel: settingsForm.selfModel, modelIds: modelIds.value })
    if (error) {
      showSettingsError(error)
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
  // 记忆开关即时生效（关掉会顺带删掉已存的记录），不跟着「保存」之外的路径改。
  setPersist(settingsForm.persist)
  closeSettings()
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

// 「全选」只统计可执行的条目：参数校验没过的那些本来就是禁用项，
// 把它们算进分母会让复选框永远勾不上（看起来像坏了）。
function checkableCalls() {
  return (confirming.value?.calls || []).filter(call => call.validation.ok)
}

const allChecked = computed(() => {
  const list = checkableCalls()
  return list.length > 0 && list.every(call => call.checked)
})

const someChecked = computed(() => checkableCalls().some(call => call.checked))

// 勾选里含高危条目时，主按钮转危险色并换文案：一键下去会停服务 / 装版本重启，
// 按钮不该长得像「确定」。
const runIsDanger = computed(() =>
  checkableCalls().some(call => call.checked && call.danger),
)

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
  if (promptRunning.value) {
    showNotice('提示词优化正在生成：同一时间只能发一个 AI 请求，等它完成或先到「提示词优化」停止后再发送。')
    return
  }
  // 模型还没就位（输入框此时本是禁用态；这里兜住快捷提问等程序化入口，并给出去路）。
  if (blocked.value) {
    showNotice(`${gate.value.title}：${gate.value.reason}`)
    return
  }
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
    showSettingsError(error)
    return
  }
  // 外部端点缺 Key：请求只会被端点判 401，先把用户带到填写处，别发一条注定失败的请求。
  if (needsKey.value && !config.apiKey) {
    openSettings()
    showSettingsError(
      '当前端点需要 API Key：填写后点「保存」（只留在本会话内存，不落盘）。本地 Ollama 一类端点可在「设置」改成本机地址后留空。',
    )
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
  atBottom.value = true
  pendingBelow.value = false
  scrollBottom()
  await runTurn(text, history)
}

// 重发最近一轮：失败与成功一样原地替换成新的尝试（「重试 / 重新生成」），
// 历史沿用当轮开始前的那份快照（不把旧回复教给模型）。工具卡片随旧气泡一起丢弃，
// 已执行的工具副作用不回滚——重新生成是对「这一轮的回答」重跑，不是对世界撤销。
async function retry() {
  if (chatting.value || promptRunning.value || !lastTurn.value) return
  const last = messages.value[messages.value.length - 1]
  if (last && last.role === 'assistant') messages.value.pop()
  await runTurn(lastTurn.value.text, lastTurn.value.history)
}

async function runTurn(userText, history) {
  chatting.value = true
  notice.value = ''
  lastTurn.value = { text: userText, history }
  // error / stopped 独立于 content：失败原因不能拼进气泡正文，否则 apiHistory 会把
  // 「⚠ 连接失败」当成模型自己说过的话回灌给下一轮，模型会照着道歉或复读这句错误。
  messages.value.push({
    id: `m${Date.now()}a`,
    role: 'assistant',
    content: '',
    pending: true,
    tools: [],
    failed: false,
    error: '',
    partial: false,
    stopped: false,
  })
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
    // 快照取 chatSession 里由 App.vue 持续喂新的那份：这一轮可能在「视图已卸载」的时段里
    // 跑工具，此时 props.state 早已停更，读它会把一份过期的状态讲给模型听。
    const request = buildRequestMessages({
      history,
      message: userText,
      snapshot: buildSnapshot(coreSnapshot.value),
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
          live.stopped = true
        } else {
          live.failed = true
          live.error = String(response.error || '请求失败')
          live.partial = response.partial === true
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
          entry.result = await runTool(entry.name, entry.args, { state: coreSnapshot.value })
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
        entry.result = await runTool(entry.name, entry.args, { state: coreSnapshot.value })
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
    live.error = String(error?.message || error)
  } finally {
    if (flushTimer) clearTimeout(flushTimer)
    live.pending = false
    chatting.value = false
    // 一轮结束（正常 / 失败 / 被停止）就落一次盘：流式期间的正文增量不走 watch，
    // 这里补上，保证新内容不会因为一直没触发条数变化而丢。
    schedulePersist()
    scrollBottom()
    // 生成期间 textarea 是禁用态，浏览器会把它从焦点链上摘掉、焦点落到 body：
    // 不主动还回去，连续对话的人每轮都要重新点一次输入框（Enter 发送也就此失效）。
    nextTick(() => composer.value && composer.value.focus())
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

// 弹窗内可聚焦元素（原生 details 的 summary 可聚焦；收起时不在 tab 序里的靠可见性过滤掉）
const FOCUSABLE = 'button, input, select, textarea, summary, [tabindex]:not([tabindex="-1"])'

// 焦点陷阱：弹窗打开时 Tab 不能跑出去。侧栏在本组件之外、不受这里的 aria-hidden 影响，
// 键盘用户按几下 Tab 就「掉进遮罩后面」——看不见焦点在哪，也关不掉弹窗。
function trapTab(event, panel) {
  // 可见性用 getClientRects() 判定：弹窗整块在 position:fixed 的遮罩里，
  // offsetParent 在这条链上的取值各引擎历史上不一致，拿它当「有没有渲染」会误判。
  const nodes = [...panel.querySelectorAll(FOCUSABLE)].filter(el => !el.disabled && el.getClientRects().length > 0)
  if (!nodes.length) return
  const first = nodes[0]
  const last = nodes[nodes.length - 1]
  const active = document.activeElement
  if (!panel.contains(active)) {
    event.preventDefault()
    ;(event.shiftKey ? last : first).focus()
    return
  }
  if (event.shiftKey && active === first) {
    event.preventDefault()
    last.focus()
  } else if (!event.shiftKey && active === last) {
    event.preventDefault()
    first.focus()
  }
}

// Esc 统一收口：先关提案确认（等同全部取消），再关设置。弹窗内任意焦点都能触发。
function onWindowKeydown(event) {
  const panel = confirming.value ? confirmPanel.value : showSettings.value ? settingsPanel.value : null
  if (event.key === 'Tab' && panel) {
    trapTab(event, panel)
    return
  }
  if (event.key !== 'Escape') return
  if (confirming.value) {
    settleConfirm([])
    return
  }
  if (showSettings.value) closeSettings()
}

onMounted(() => {
  window.addEventListener('keydown', onWindowKeydown)
  // 输入框高度由 autoGrow() 写在内联 style 上：切走再回来（或上次留下的草稿）不会有
  // draft 变化事件，高度会退回一行、把多行草稿藏进滚动条里。
  autoGrow()
  // 把上次会话读回来（首次进入才有作用；在飞的那一轮不会被覆盖）。读完后贴到底部，
  // 让「回来续上」落在最新一条上。
  void ensureHydrated().then(() => {
    nextTick(() => {
      const box = chatBox.value
      if (box) box.scrollTop = box.scrollHeight
      // 切走时正等着勾选的提案确认框：回来后要重新拿到焦点，否则 Tab / Esc 都落在页面别处。
      if (confirming.value && confirmPanel.value) confirmPanel.value.focus()
    })
  })
})

function clearHistory() {
  if (chatting.value) return
  // 内存与本机记录一起清：只清内存的话，下次打开又把旧对话读回来。
  clearConversation()
  dismissNotice()
}

function quick(text) {
  if (chatting.value || promptRunning.value || blocked.value) return
  draft.value = text
  send()
}

// 「重试 / 重新生成」只给最新一轮的助手消息：更早的轮次已被后续对话取代，重发会把话题拉回过去。
// 本轮生成中（pending / chatting）不出现；历史快照在重载后不可得（lastTurn 不落盘），故仅限本会话。
function canRetry(message) {
  return (
    !chatting.value &&
    !promptRunning.value &&
    message.role === 'assistant' &&
    !message.pending &&
    lastTurn.value !== null &&
    messages.value[messages.value.length - 1] === message
  )
}

function toolStatusText(entry) {
  if (entry.status === 'running') return '执行中…'
  if (entry.status === 'skipped') return '未执行'
  return entry.result?.ok ? '已完成' : '失败'
}

// 工具结果一旦写入就不再变，但模板里每次重渲染都要它**两次**（`v-else-if` 的判据 +
// `<pre>` 的正文）。流式期间父视图每秒重渲染十几次，等于每秒把最多 8KB 的 JSON 重新
// stringify 二十遍。按 result 的对象身份缓存一次即可——它不会被就地改写，键也随对象一起回收。
const RESULT_TEXT_CACHE = new WeakMap()

// 工具结果折叠区的正文：默认收起（模型已经拿到并转述过，展开是给人核对用的）。
// 上限与 ai.js 里 read_log 的截断同口径，避免把 8KB 日志一次性铺进 DOM。
function resultText(entry) {
  const result = entry.result
  if (!result || typeof result !== 'object') return ''
  const cached = RESULT_TEXT_CACHE.get(result)
  if (cached !== undefined) return cached
  let text
  if (result.ok === false) {
    text = String(result.error ?? '执行失败')
  } else {
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
    if (text.length > 8000) text = `${text.slice(0, 8000)}\n…（其余省略）`
  }
  RESULT_TEXT_CACHE.set(result, text)
  return text
}

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onWindowKeydown)
  if (noticeTimer) clearTimeout(noticeTimer)
  // 🔴 刻意不调 abortChat()：切走视图不该掐断正在生成的那一轮。会话状态在 chatSession
  // 的模块级单例里，回来时按最新内容续上；真要中止由视图里的「停止」按钮决定。
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
        <button type="button" @click="openSettings($event)">设置</button>
        <!-- 没有对话时按钮就是不可用态，而不是点下去毫无反应的假入口（禁用的 opacity 已够说明） -->
        <button
          type="button"
          class="ghost"
          :disabled="chatting || !messages.length"
          title="清空当前对话（同时删除本机保存的记录）"
          @click="clearHistory"
        >清空</button>
      </div>
    </header>

    <!-- 门槛横幅：模型没就位时输入框就是禁用态，这里必须同时说清「为什么不能用」与「往哪走」，
         否则一个点不动的输入框看起来就是坏的。 -->
    <p v-if="blocked" class="assistant-warn" role="status">
      <svg class="assistant-warn-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <span>
        <strong>{{ gate.title }}。</strong>{{ gate.reason }}
      </span>
      <button
        type="button"
        class="assistant-gate-fix"
        @click="gate.fix === 'settings' ? openSettings($event) : emit('navigate', gate.fix)"
      >{{ gate.fix === 'settings' ? '去设置' : '去「模型与服务」' }}</button>
    </p>

    <!-- 外部端点不依赖核心：未就绪只是一条「代管不了本应用」的说明，不拦使用。 -->
    <p v-else-if="!coreReady" class="assistant-warn" role="status">
      <svg class="assistant-warn-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <span>
        核心当前{{ phaseLabel(state.phase) }}{{ state.message ? `（${state.message}）` : '' }}：
        {{ config.brain === 'self'
          ? '本机大脑要用正在运行的核心作为模型服务，未就绪时发不出去。可回「模型与服务」启动服务，或在「设置」改用外部端点。'
          : '外部端点仍可正常问答，但助手代管不了本应用的服务。' }}
      </span>
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
            :disabled="chatting || promptRunning || blocked"
            :title="blocked ? gate.title : promptRunning ? '提示词优化正在生成' : undefined"
            @click="quick(prompt)"
          >{{ prompt }}</button>
        </div>
        <p class="assistant-privacy">
          对话记录保存在本机 IndexedDB：重开应用、切换栏目都不会丢（生成中切走回来也会接着显示），
          点「清空」才会删除；也能在「设置」关掉「记住对话」。落盘前会丢掉工具入参（其中可能有平台 Key 明文），
          API Key 本身始终只在会话内存里、不写磁盘，重启后需在「设置」重新填写。
          本机大脑使用 WB Bridge 自己的免费模型（默认 <code>{{ AUTO_MODEL_ID }}</code>：按每次请求内容自动挑选），
          无需任何 Key，但会消耗少量免费额度。
        </p>
      </article>

      <article v-for="m in messages" :key="m.id" class="assistant-msg" :class="[m.role, { failed: m.failed }]">
        <div class="assistant-msg-meta">
          <span>{{ m.role === 'user' ? '我' : 'AI 助手' }}</span>
          <span v-if="m.role === 'assistant' && m.pending" class="assistant-thinking">正在生成…</span>
          <button v-else-if="canRetry(m)" type="button" class="assistant-retry" @click="retry">{{ m.failed ? '重试' : '重新生成' }}</button>
        </div>
        <div class="assistant-bubble">
          <!-- 受控 Markdown 渲染收进子组件：父视图每 ~80ms 因流式增量重渲染一次，
               v-html 留在父模板会让**每条**历史气泡的 innerHTML 被重新赋值（Vue 对
               innerHTML 不做新旧比对），整段对话反复重解析。收进去后只有正在生成的那条更新。 -->
          <MarkdownBlock class="assistant-text" :content="m.content" />
          <!-- 光标由 CSS 画出：▍ 这类字符的字宽与基线随系统字体变化，也对不齐行尾 -->
          <span v-if="m.pending" class="assistant-cursor" aria-hidden="true" />
        </div>
        <!-- 失败原因独立成行：拼进正文会被 apiHistory 当成模型自己说过的话回灌下一轮 -->
        <p v-if="m.error" class="assistant-msg-error" role="alert">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
            <circle cx="12" cy="12" r="8.4" />
            <path d="M12 7.6v5.6M12 16.4v.4" />
          </svg>
          <span>{{ m.error }}{{ m.partial ? '（以上为已生成部分）' : '' }}</span>
        </p>
        <p v-else-if="m.stopped" class="assistant-msg-stopped" role="status">已停止生成，以上是已经生成的部分。</p>
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

    <!-- 用户往上翻时新内容照样到达，只是不再强行滚动：给一条明确的路回到底部，
         否则「悄悄不滚」会让人误以为助手停住了。
         放在滚动容器**之外**（视图自身的流内）：sticky 浮条要靠滚动容器的约束矩形生效，
         而纵向 flex 滚动容器里的 sticky 在各引擎表现不一致，与其赌一个看不出来的差异，
         不如用一条确定会出现、也确定点得着的横条。 -->
    <button
      v-if="pendingBelow"
      type="button"
      class="assistant-jump"
      aria-label="回到对话底部"
      @click="jumpToBottom"
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M12 5.5v13M6.5 13.2l5.5 5.3 5.5-5.3" />
      </svg>
      回到最新
    </button>

    <p v-if="notice" class="assistant-keyhint assistant-notice" role="status">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <span>{{ notice }}</span>
      <button type="button" class="assistant-close ghost" aria-label="关闭提示" @click="dismissNotice">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" aria-hidden="true">
          <path d="M7 7l10 10M17 7L7 17" />
        </svg>
      </button>
    </p>
    <p v-else-if="promptRunning" class="assistant-keyhint" role="status">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <span>提示词优化正在生成：同一时间只能发一个 AI 请求，等它完成（或先到那边停止）后再继续对话。</span>
      <button type="button" class="assistant-gate-fix" @click="emit('navigate', 'prompt')">查看提示词优化</button>
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
        :disabled="chatting || promptRunning || blocked"
        :placeholder="composerPlaceholder"
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
        <button
          v-else
          type="submit"
          class="primary"
          :disabled="!draft.trim() || props.busy || promptRunning || blocked"
          :title="promptRunning ? '提示词优化正在生成，同一时间只能发一个 AI 请求' : props.busy ? '其他操作正在进行中，等它结束后可用' : blocked ? gate.title : undefined"
        >
          <span v-if="props.busy" class="spinner" />发送
        </button>
      </div>
    </form>
  </section>

  <!-- 设置弹窗 -->
  <div v-if="showSettings" class="assistant-mask" @click.self="closeSettings">
    <div
      ref="settingsPanel"
      class="assistant-dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="aiSettingsTitle"
      tabindex="-1"
    >
      <header class="assistant-dialog-head">
        <h3 id="aiSettingsTitle">AI 设置</h3>
        <button type="button" class="assistant-close ghost" aria-label="关闭设置" @click="closeSettings">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" aria-hidden="true">
            <path d="M7 7l10 10M17 7L7 17" />
          </svg>
        </button>
      </header>
      <div class="assistant-dialog-body">
        <div class="field">
          <span class="f" id="aiBrainLabel">大脑（谁来回答问题）</span>
          <div class="assistant-brains" role="radiogroup" aria-labelledby="aiBrainLabel">
            <label class="assistant-brain" :class="{ active: settingsForm.brain === 'self' }">
              <input v-model="settingsForm.brain" type="radio" value="self" name="aiBrain">
              <span>
                <strong>本机免费模型</strong>
                <em>用 WB Bridge 自己的桥接服务（OpenCode 免费模型）。无需任何 Key，端点与令牌由本应用自己持有；会消耗少量免费额度。</em>
              </span>
            </label>
            <label class="assistant-brain" :class="{ active: settingsForm.brain === 'external' }">
              <input v-model="settingsForm.brain" type="radio" value="external" name="aiBrain">
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
            <span class="f" id="aiPresetLabel">快捷预设</span>
            <div class="assistant-presets" role="group" aria-labelledby="aiPresetLabel">
              <button
                v-for="preset in AI_PRESETS"
                :key="preset.baseUrl"
                type="button"
                class="assistant-chip"
                :aria-pressed="settingsForm.baseUrl === preset.baseUrl && settingsForm.model === preset.model"
                @click="applyPreset(preset)"
              >{{ preset.label }}</button>
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
            <div class="assistant-key-wrap">
              <input
                id="aiApiKey"
                v-model="settingsForm.apiKey"
                :type="revealKey ? 'text' : 'password'"
                autocomplete="off"
                spellcheck="false"
                placeholder="仅本会话内存，不落盘；本地端点可留空"
              >
              <!-- 眼睛按钮与「平台」视图同一套交互：粘贴完能立刻核对有没有多带空格 / 少带字符，
                   比让请求 401 之后再猜原因省一轮。默认隐藏，关弹窗即回到隐藏态。 -->
              <button
                type="button"
                class="reveal-btn"
                :aria-pressed="revealKey"
                :aria-label="revealKey ? '隐藏 API Key 明文' : '显示 API Key 明文'"
                :title="revealKey ? '隐藏明文' : '显示明文'"
                @click="revealKey = !revealKey"
              >
                <svg v-if="!revealKey" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z" />
                  <circle cx="12" cy="12" r="3.2" />
                </svg>
                <svg v-else viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z" />
                  <circle cx="12" cy="12" r="3.2" />
                  <path d="M4 4l16 16" />
                </svg>
              </button>
            </div>
          </div>
        </template>

        <div class="field">
          <label class="assistant-opt">
            <input v-model="settingsForm.tools" type="checkbox">
            <span>允许 AI 调用管理工具（查状态 / 读日志 / 读取与检测模型 / 导入插件 / 保存平台 Key…）</span>
          </label>
          <p class="note">关闭后 AI 只能问答与给建议，不能执行任何操作。写入类工具无论如何都会先弹确认框，勾选后才执行。</p>
        </div>

        <div class="field">
          <label class="assistant-opt">
            <input v-model="settingsForm.persist" type="checkbox">
            <span>记住对话（存到本机 IndexedDB，重开应用与切换栏目都不丢）</span>
          </label>
          <p class="note">只存对话内容：工具入参（其中可能有平台 Key 明文）在落盘前丢弃，API Key 从不落盘。关掉会立刻删除已存的记录；「清空」随时可手动清一遍。</p>
        </div>

        <p v-if="settingsError" ref="settingsErrorBox" class="assistant-dialog-error" role="alert">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
            <circle cx="12" cy="12" r="8.4" />
            <path d="M12 7.6v5.6M12 16.4v.4" />
          </svg>
          <span>{{ settingsError }}</span>
        </p>
        <p class="note">请求由本应用代理发出（壳侧仅允许 https 或本机 http 端点，本机大脑只走回环）；Key 随请求参数进入进程内存，不进日志、不进存储、不出现在任何界面。端点、大脑与模型名会记住，Key 不会。</p>
      </div>
      <footer class="assistant-dialog-foot">
        <!-- 未保存标记：关掉弹窗表单就回到已生效的值，所以这里必须说清「改了但还没生效」，
             靠按钮禁用或二次确认都更挡路 -->
        <p v-if="settingsDirty" class="assistant-unsaved" role="status">有未保存的修改</p>
        <button type="button" class="ghost" @click="closeSettings">取消</button>
        <button type="button" class="primary" @click="saveSettings">保存</button>
      </footer>
    </div>
  </div>

  <!-- 工具提案确认弹窗 -->
  <div v-if="confirming" class="assistant-mask" @click.self="settleConfirm([])">
    <div
      ref="confirmPanel"
      class="assistant-dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="aiConfirmTitle"
      tabindex="-1"
    >
      <header class="assistant-dialog-head">
        <h3 id="aiConfirmTitle">确认 AI 操作提案</h3>
        <button type="button" class="assistant-close ghost" aria-label="取消全部" @click="settleConfirm([])">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" aria-hidden="true">
            <path d="M7 7l10 10M17 7L7 17" />
          </svg>
        </button>
      </header>
      <div class="assistant-dialog-body">
        <p class="note">以下是 AI 起草的操作。勾选后点「执行选中操作」；取消则一条都不执行（AI 会收到「用户未批准」的反馈）。</p>
        <p v-if="confirming.calls.some(c => c.danger)" class="assistant-danger-hint" role="alert">
          标了「高危写入」的条目会停止服务或装新版本并重启应用，执行后本窗口可能随之关闭；不勾选即可安全跳过。
        </p>
        <ul class="assistant-ops">
          <li v-for="call in confirming.calls" :key="call.id" :class="{ invalid: !call.validation.ok }">
            <!-- 整行都是 label：命中区从 16px 的方框变成整条提案，
                 勾选这一列本来就是一整列一列对着点的动作 -->
            <label class="assistant-op">
              <span class="assistant-op-check">
                <input v-model="call.checked" type="checkbox" :disabled="!call.validation.ok">
              </span>
              <span class="assistant-op-main">
                <span class="assistant-op-title">
                  <span class="assistant-tool-tag" :class="call.danger ? 'danger' : 'write'">{{ call.danger ? '高危写入' : '写入' }}</span>
                  {{ call.summary }}
                </span>
                <span v-if="call.validation.ok && Object.keys(call.preview).length" class="assistant-tool-preview">
                  <span v-for="(value, key) in call.preview" :key="key">{{ key }}：{{ value }}</span>
                </span>
                <span v-else-if="!call.validation.ok" class="assistant-tool-error">参数校验未通过：{{ call.validation.error }}</span>
              </span>
            </label>
          </li>
        </ul>
        <label class="assistant-op-all">
          <!-- 半选态（indeterminate）必须显出来：只勾了一部分时复选框看着像「没全选、点一下补满」，
               但点它实际是把全部取消，行为与视觉不符。禁用项不计入分母（见 checkableCalls）。 -->
          <input type="checkbox" :checked="allChecked" :indeterminate="someChecked && !allChecked" @change="toggleAll"> 全选
        </label>
      </div>
      <footer class="assistant-dialog-foot">
        <button type="button" class="ghost" @click="settleConfirm([])">全部取消</button>
        <!-- 勾选里含高危条目时按钮转危险色并改名：这一键下去会停服务 / 装版本重启，
             不能和「执行选中操作」长得一模一样 -->
        <button
          type="button"
          class="primary"
          :class="{ danger: runIsDanger }"
          :disabled="!someChecked"
          @click="runApproved"
        >{{ runIsDanger ? '执行选中的高危操作' : '执行选中操作' }}</button>
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
.assistant-empty code { font-family: ui-monospace, monospace; font-size: var(--fs-xs); background: var(--row); padding: 1px 5px; border-radius: 4px; }
.assistant-quick { display: flex; flex-wrap: wrap; gap: 8px; justify-content: center; }
.assistant-chip {
  font-size: var(--fs-xs);
  /* 8px 纵向内距：6px 时整颗 chip 只有 ~30px 高，指针在密集横排里容易误点到邻颗 */
  padding: 8px 12px;
  border-radius: 999px;
  border: 1px solid var(--line);
  background: var(--row);
  color: var(--muted-strong);
  cursor: pointer;
  white-space: normal;
  text-align: left;
  transition: background var(--dur-1) var(--ease-standard), border-color var(--dur-1) var(--ease-standard), color var(--dur-1) var(--ease-standard);
}
.assistant-chip:hover:not(:disabled) { border-color: color-mix(in srgb, var(--green) 40%, var(--line)); background: var(--green-bg); color: var(--text); }
.assistant-chip:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.assistant-chip:disabled { opacity: .6; cursor: not-allowed; }
/* 预设的「已应用」态：chip 平时是中性底，命中当前端点时要落主色，
   否则用户看不出点了哪一颗生效（aria-pressed 给读屏，这层给眼睛） */
.assistant-presets .assistant-chip[aria-pressed="true"] {
  background: var(--green-bg);
  border-color: color-mix(in srgb, var(--green) 45%, var(--line));
  color: var(--text);
  font-weight: 600;
}
.assistant-privacy { font-size: var(--fs-xs); color: var(--muted-strong); }

.assistant-msg { display: flex; flex-direction: column; gap: 4px; max-width: 86%; }
.assistant-msg.user { align-self: flex-end; align-items: flex-end; }
.assistant-msg.assistant { align-self: flex-start; }
.assistant-msg-meta { font-size: var(--fs-xs); color: var(--muted-strong); display: flex; align-items: center; gap: 8px; }
/* 「正在生成…」与「重试 / 重新生成」是元信息行的两种终态：前者只在 pending 时出现，
   后者只在最新一轮已完成（含失败 / 停止）且不在生成中时出现 */
.assistant-thinking { color: var(--green); }
.assistant-retry {
  font-size: var(--fs-xs);
  /* 负 margin 保住原有视觉位置，只把命中区扩到 26px 高（padding 会撑坏行内对齐，故用这招） */
  padding: 4px 6px;
  margin: -4px -6px;
  border: none;
  background: none;
  color: var(--orange);
  text-decoration: underline;
  text-underline-offset: 2px;
  cursor: pointer;
}
.assistant-retry:hover { color: var(--text); background: var(--row); border-radius: var(--radius-s); }
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
.assistant-msg.failed .assistant-bubble { border-color: color-mix(in srgb, var(--danger) 45%, var(--line)); }
/* 气泡正文内部的 p / code / pre.ai-code 四条规则随 v-html 一起搬进了
   `src/components/MarkdownBlock.vue`（那里的 :deep() 直接命中本组件自己的作用域）。 */
/* 生成光标由 CSS 画出（不用 ▍ 字符：字宽与基线随系统字体变化，也贴不齐行尾）。
   要求减少动态效果时全站循环动画被 base.css 收敛成静态一帧，这里正好留一个常亮光标。 */
.assistant-cursor {
  display: inline-block;
  width: 2px;
  height: 1em;
  margin-left: 2px;
  vertical-align: -0.15em;
  background: var(--green);
  animation: assistant-blink 1s steps(2) infinite;
}
@keyframes assistant-blink { 50% { opacity: 0; } }

/* 失败 / 停止：正文与原因分行，原因带图标（不只靠红色，红色在色弱下与「写入」难分） */
.assistant-msg-error,
.assistant-msg-stopped {
  margin: 0;
  display: flex;
  align-items: flex-start;
  gap: 6px;
  font-size: var(--fs-xs);
  line-height: var(--lh-loose);
}
.assistant-msg-error { color: var(--danger); }
.assistant-msg-stopped { color: var(--muted-strong); }
.assistant-msg-error > svg { width: 14px; height: 14px; flex-shrink: 0; margin-top: 2px; }

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
/* 三级语义（只读 / 写入 / 高危）各占一个色相：写入用橙、高危用红，
   两者此前同系（#a4581a 与 #965e23）扫一眼分不出等级，而这一列正是决定要不要放行 */
.assistant-tool.write { border-left-color: var(--orange); }
.assistant-tool.danger { border-left-color: var(--danger); }
.assistant-tool-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.assistant-tool-tag {
  font-size: var(--fs-xs); font-weight: 600; white-space: nowrap;
  padding: 2px 7px; border-radius: 999px;
  background: var(--row); color: var(--muted-strong);
}
.assistant-tool-tag.write { background: var(--orange-bg); color: var(--orange); }
.assistant-tool-tag.danger { background: var(--danger-bg); color: var(--danger); }
.assistant-tool-label { font-weight: 600; color: var(--text); }
.assistant-tool-status { margin-left: auto; color: var(--muted-strong); }
.assistant-tool-status.done { color: var(--green); }
.assistant-tool-status.skipped { color: var(--muted-strong); }
.assistant-tool-status.running { color: var(--orange); }
.assistant-tool-summary { margin-top: 4px; color: var(--muted-strong); }
.assistant-tool-preview { margin-top: 4px; display: flex; flex-wrap: wrap; gap: 10px; color: var(--muted-strong); font-family: ui-monospace, monospace; }
.assistant-tool-error { margin-top: 4px; color: var(--danger); }
/* 原始返回默认收起：模型已读过并转述过，展开是给人逐条核对用的 */
.assistant-tool-result { margin-top: 4px; }
.assistant-tool-result > summary {
  cursor: pointer;
  color: var(--muted-strong);
  font-size: var(--fs-xs);
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
  font-size: var(--fs-xs);
  line-height: var(--lh-loose);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.assistant-trimmed { margin: 2px 0 0; font-size: var(--fs-xs); color: var(--muted-strong); }

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
.assistant-brain em { font-style: normal; font-size: var(--fs-xs); line-height: var(--lh-loose); color: var(--muted-strong); }
.assistant-opt { display: flex; gap: 8px; align-items: flex-start; font-size: var(--fs-sm); cursor: pointer; }
.assistant-opt input { margin-top: 3px; }

/* 三条说明横幅共用一套视觉：未就绪（.assistant-warn）、需要 Key / 工具状态（.assistant-keyhint）、
   这一轮被拦下（.assistant-notice）。差异只由模板上的类组合决定，不再复制一遍样式。
   统一 flex：图标与正文同基线，读起来比「一行纯橙字」更像一条状态说明而非装饰底色。 */
.assistant-keyhint,
.assistant-warn {
  margin: 0; flex-shrink: 0;
  display: flex; align-items: flex-start; gap: 8px;
  font-size: var(--fs-xs); line-height: var(--lh-loose);
  color: var(--orange);
  background: var(--orange-bg);
  border: 1px solid color-mix(in srgb, var(--orange) 35%, transparent);
  border-radius: var(--radius-s);
  padding: 8px 10px;
}
.assistant-keyhint > span,
.assistant-warn > span { min-width: 0; flex: 1; }
.assistant-warn > span > strong { color: var(--text); }
/* 门槛横幅里的出路按钮（去设置 / 去「模型与服务」）：横幅是 flex 行，
   按钮不能被正文挤扁，也不能在窄窗口里被拉成整行宽 */
.assistant-gate-fix {
  flex-shrink: 0;
  align-self: flex-start;
  font-size: var(--fs-xs);
  padding: 5px 10px;
  background: var(--surface-raised);
  border-color: color-mix(in srgb, var(--orange) 45%, var(--line));
  color: var(--text);
}
.assistant-keyhint > svg,
.assistant-warn > svg { width: 15px; height: 15px; flex-shrink: 0; margin-top: 2px; }
/* 拦下这次发送的一次性提示：带关闭按钮，8s 后自动消隐 */
.assistant-notice > .assistant-close { flex-shrink: 0; }

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
  transition: border-color var(--dur-1) var(--ease-standard);
}
.assistant-composer textarea:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.assistant-composer textarea:disabled { opacity: .6; }
.assistant-composer-actions { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.assistant-context { font-size: var(--fs-xs); color: var(--muted-strong); display: flex; align-items: center; gap: 6px; min-width: 0; overflow-wrap: anywhere; }
.assistant-context em { font-style: normal; color: var(--muted-strong); }
/* 服务就绪与否用颜色点直接标在输入区上方：助手能不能代管操作，取决于这一个状态。
   旁边始终跟着 brainLabel 文字，点只是加速器，不构成「只靠颜色传达信息」。 */
.assistant-dot { flex-shrink: 0; width: 8px; height: 8px; border-radius: 50%; background: var(--muted-strong); }
.assistant-dot.ok { background: var(--green); }
.assistant-dot.warn { background: var(--orange); }

/* 回到最新：贴在对话区下方的横条（见模板注释，不走 sticky），
   只在上翻且下面还有新内容时出现 */
.assistant-jump {
  align-self: center;
  z-index: 2;
  display: flex;
  align-items: center;
  gap: 6px;
  min-height: 30px;
  padding: 6px 14px;
  border-radius: 999px;
  border: 1px solid color-mix(in srgb, var(--green) 38%, var(--line));
  background: var(--surface-raised);
  color: var(--green);
  font-size: var(--fs-xs);
  box-shadow: var(--shadow-s);
}
.assistant-jump > svg { width: 14px; height: 14px; }
.assistant-jump:hover { background: var(--green-bg); }

/* 弹窗（设置 / 提案确认共用） */
.assistant-mask {
  position: fixed; inset: 0; z-index: 20;
  /* 45% 黑：32% 时面板底色与弹窗内容还在抢视线，弹窗里的密集复选框读不清 */
  background: rgb(0 0 0 / 45%);
  display: flex; align-items: center; justify-content: center;
  padding: var(--sp-5);
  animation: assistant-fade var(--dur-2) var(--ease-standard);
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
  /* 入场只动 transform / opacity：弹窗从触发位置上方 8px 落下来，
     说明「这块是刚弹出来的」；0ms 直接出现会让人以为是面板自己变了 */
  animation: assistant-dialog-in var(--dur-3) var(--ease-enter);
}
@keyframes assistant-fade { from { opacity: 0; } to { opacity: 1; } }
@keyframes assistant-dialog-in {
  from { opacity: 0; transform: translateY(-8px) scale(.985); }
  to { opacity: 1; transform: none; }
}
.assistant-dialog-head {
  display: flex; align-items: center; justify-content: space-between;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--line);
}
/* 弹窗标题用 h3 并挂 aria-labelledby：role=dialog 的名字由标题元素提供，
   读屏进弹窗时会念出标题，而不是只念一个 label 字符串 */
.assistant-dialog-head h3 { margin: 0; font-size: var(--fs-base); font-weight: 600; }
.assistant-close {
  width: 30px; height: 30px; flex-shrink: 0;
  display: flex; align-items: center; justify-content: center;
  padding: 0;
}
.assistant-close > svg { width: 15px; height: 15px; }
.assistant-dialog-body { padding: var(--sp-4); overflow-y: auto; display: flex; flex-direction: column; gap: var(--sp-3); }
.assistant-dialog-foot {
  display: flex; justify-content: flex-end; align-items: center; gap: 8px;
  padding: var(--sp-3) var(--sp-4);
  border-top: 1px solid var(--line);
}
/* 未保存标记靠右推到按钮前：关掉弹窗即丢弃这些改动，必须让人看见再决定 */
.assistant-unsaved {
  margin: 0; margin-right: auto;
  font-size: var(--fs-xs); color: var(--orange);
}
.field { display: flex; flex-direction: column; gap: 6px; }
.field .f { font-size: var(--fs-sm); font-weight: 600; }
/* 只给文本类控件描边与内距：checkbox/radio 继承这些会退化成正方形色块 */
.field input:not([type=checkbox]):not([type=radio]),
.field select {
  font: inherit; font-size: var(--fs-sm);
  min-height: 34px;
  padding: 8px 10px;
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--surface-raised); color: var(--text);
  transition: border-color var(--dur-1) var(--ease-standard);
}
.field input:not([type=checkbox]):not([type=radio]):hover,
.field select:hover { border-color: color-mix(in srgb, var(--green) 30%, var(--line)); }
.field input:not([type=checkbox]):not([type=radio]):focus-visible,
.field select:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
/* Key 输入位：右侧留出眼睛按钮（与「平台」视图同构） */
.assistant-key-wrap { position: relative; display: flex; }
.assistant-key-wrap input { flex: 1; min-width: 0; padding-right: 42px; }
.reveal-btn {
  position: absolute; right: 3px; top: 50%; transform: translateY(-50%);
  width: 34px; height: 34px;
  display: flex; align-items: center; justify-content: center;
  border: none; background: transparent; border-radius: var(--radius-s);
  color: var(--muted-strong); cursor: pointer;
  transition: color var(--dur-1) var(--ease-standard), background var(--dur-1) var(--ease-standard);
}
.reveal-btn:hover { color: var(--text); background: var(--nav-hover-bg); }
.reveal-btn:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.reveal-btn[aria-pressed="true"] { color: var(--green); }
.reveal-btn svg { width: 22px; height: 22px; }
/* 原生 checkbox/radio 也必须有可见焦点环（键盘用户在弹窗里靠 Tab 走） */
.assistant-brain input:focus-visible,
.assistant-opt input:focus-visible,
.assistant-op-check input:focus-visible,
.assistant-op-all input:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.assistant-presets { display: flex; flex-wrap: wrap; gap: 8px; }
.note { margin: 0; font-size: var(--fs-xs); color: var(--muted-strong); line-height: var(--lh-loose); }
.assistant-dialog-error {
  margin: 0; display: flex; align-items: flex-start; gap: 6px;
  font-size: var(--fs-xs); line-height: var(--lh-loose); color: var(--danger);
}
.assistant-dialog-error > svg { width: 15px; height: 15px; flex-shrink: 0; margin-top: 2px; }
.assistant-danger-hint {
  margin: 0; font-size: var(--fs-xs); line-height: var(--lh-loose);
  color: var(--danger);
  background: var(--danger-bg);
  border: 1px solid color-mix(in srgb, var(--danger) 35%, transparent);
  border-radius: var(--radius-s);
  padding: 8px 10px;
}

/* 实色危险按钮（勾了高危条目时的「执行」）：文字色沿用 --on-primary，
   暗色下它是深色，正好压得住浅玫红底；hover 方向与 .primary 一致（往 --text 靠） */
.primary.danger { background: var(--danger); border-color: var(--danger); color: var(--on-primary); }
.primary.danger:hover:not(:disabled) {
  background: color-mix(in srgb, var(--danger) 86%, var(--text));
  border-color: color-mix(in srgb, var(--danger) 86%, var(--text));
}

.assistant-ops { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
.assistant-op {
  display: flex; gap: 10px; width: 100%; text-align: left;
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--row); padding: 10px 12px; cursor: pointer;
  transition: border-color var(--dur-1) var(--ease-standard), background var(--dur-1) var(--ease-standard);
}
.assistant-op:hover { border-color: color-mix(in srgb, var(--green) 32%, var(--line)); background: var(--surface-raised); }
/* 参数校验没过的条目整行不可执行：读出来像可点、点下去没反应是最容易被当成 bug 的形态 */
.assistant-ops li.invalid .assistant-op { opacity: .6; cursor: not-allowed; }
/* hover 高亮也要一起收掉：勾不上却还在变绿边，等于用样式承诺了一个点不动的东西 */
.assistant-ops li.invalid .assistant-op:hover { border-color: var(--line); background: var(--row); }
.assistant-op-check { display: flex; align-items: flex-start; padding-top: 2px; }
.assistant-op-main { min-width: 0; display: block; }
.assistant-op-title { font-size: var(--fs-sm); font-weight: 600; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.assistant-op-all { font-size: var(--fs-xs); color: var(--muted-strong); display: flex; align-items: center; gap: 6px; cursor: pointer; }
</style>
