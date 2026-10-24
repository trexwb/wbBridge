// 提示词优化器的会话状态：跨视图切换保活，关闭应用即清空。
//
// 与 chatSession.js 同一条理由：面板用 v-if 切视图，切走即卸载组件。状态若放在组件里，
// 一轮生成还没回来就被一起销毁，「切走再回来接着看」无从谈起。这里的状态是**模块级单例**：
// 卸载组件不影响在飞的那一轮继续往同一份 ref 里追加增量。
//
// 内容口径：只有用户自己填进来的提示词与模型产出的文本，没有任何凭据。刻意**不落盘**
// （不写 localStorage / IndexedDB）：提示词里可能带着项目信息甚至误粘贴的密钥，
// 会话结束就应当消失。需要留存时用户自己复制。
import { computed, ref } from 'vue'
import { AUTO_MODEL_ID, abortChat, chat } from './ai.js'
import { chatting } from './chatSession.js'
import {
  DEFAULT_MODE,
  MAX_VERSIONS,
  buildOptimizeMessages,
  buildTestMessages,
  charCount,
  cleanOptimizedText,
  compareIsStale,
  compareVersionIsStale,
  extractVariables,
  nextVersionLabel,
  shouldShowRejection,
  submitIsThrottled,
  submitRejection,
  validateCompare,
  validateOptimize,
  variableReport,
  versionIsStale,
} from './prompt.js'

export const mode = ref(DEFAULT_MODE) // 'system' | 'user'
export const source = ref('') // 原始提示词
export const requirement = ref('') // 补充要求（可选）
// 版本链：每轮优化结果一版。只增不改，切换查看，用户可编辑当前版。
export const versions = ref([]) // { id, label, mode, content, requirement, sourceLength, at, stopped, variables }
export const activeId = ref('')
// 在飞标记：同一时间只允许一个 AI 请求（bridge.js 只有一个 activeChatId，
// 并发会互相 abort）。AssistantView 读这个值做互斥。
export const running = ref(false)
// 已提交但还没收到第一个增量。这段空窗（提交 → 模型首字）可能好几秒，必须有独立状态：
// 否则用户看到的是「点了一下按钮，然后什么都没变」。首个字节到达即清除。
export const starting = ref(false)
// 正在跑的是哪一类任务：优化与对比共用 running，界面据此只在对应的区域显示进度。
export const runKind = ref('') // '' | 'optimize' | 'compare'
// 用户已点「停止」，等壳回传中止结果。避免重复点击，也给出即时反馈。
export const stopping = ref(false)
// 失败原因与界面提示都独立于正文：拼进正文会被当成模型说过的话再喂回模型。
export const error = ref('')
export const notice = ref('')

// ── 对比测试 ────────────────────────────────────────────────────────
// 同一份输入分别喂给「源提示词」与「当前版本」。两次请求**顺序**执行：bridge.js 只有一个
// activeChatId，并发会互相覆盖，「停止」也会停错一条；顺序跑还能说清正在跑哪一半。
export const testInput = ref('')
export const testBase = ref('') // 源提示词的输出
export const testOptimized = ref('') // 当前版本的输出
export const testStage = ref('') // '' | 'base' | 'optimized'
// 这一轮对比实际绑定的源文、对象、输入、版本与正文：任何一项变了，旧结果就不再是对照。
export const testedSource = ref('')
export const testedMode = ref('')
export const testedInput = ref('')
export const testedVersionId = ref('')
export const testedContent = ref('')
// 对比测试的反馈独立于优化反馈：错误必须出现在用户正在操作的折叠区里。
export const testError = ref('')
export const testNotice = ref('')

let seq = 0
// 两类提交共用一条前沿节流：刚结束就立刻再点，不重复发请求。
let lastSubmitAt = 0
// 拒绝提示自身的节流按区域分开：优化区的连点不能把对比区的提示一起吞掉。
const lastRejectAt = { optimize: 0, compare: 0 }

function newId() {
  seq += 1
  return `pv${Date.now().toString(36)}${seq}`
}

// 把拒绝原因写进对应的提示槽（优化区用 notice，对比区用 testNotice），并按节流去重。
// 返回值恒等于 reason，方便调用方直接 return。
function rejectInto(channel, reason) {
  const now = Date.now()
  if (!shouldShowRejection(lastRejectAt[channel], now)) return reason
  lastRejectAt[channel] = now
  if (channel === 'compare') testNotice.value = reason
  else notice.value = reason
  return reason
}

// 提交类入口（优化 / 迭代 / 对比）的前置守卫：null = 放行，否则已把原因写进对应提示槽。
// 绝不允许静默 return——「点了没反应」是本项目明令禁止的交互。先报在飞原因，
// 再挡前沿节流：生成中说「等待响应」比笼统的「刚提交过」更能解释现状。
function submitGuard(channel, now = Date.now()) {
  const reason = submitRejection({
    running: running.value,
    starting: starting.value,
    assistantBusy: chatting.value,
  })
  if (reason) return rejectInto(channel, reason)
  if (submitIsThrottled(lastSubmitAt, now)) return rejectInto(channel, '刚提交过，请稍候再点。')
  return null
}

// 非提交类动作（切模式 / 切版本 / 设为源 / 清空）在生成中被挡住时同样要说话。
function busyGuard(channel) {
  if (!running.value && !starting.value) return null
  return rejectInto(channel, '正在生成中，请等这一轮结束，或点「停止」中止。')
}

export const activeVersion = computed(() => versions.value.find(item => item.id === activeId.value) || versions.value[versions.value.length - 1] || null)
export const outputText = computed(() => activeVersion.value?.content || '')
export const sourceVariables = computed(() => extractVariables(source.value))
export const canOptimize = computed(() => validateOptimize({ source: source.value, requirement: requirement.value }) === null)
export const hasSession = computed(() => Boolean(source.value.trim() || requirement.value.trim() || versions.value.length))
export const canCompare = computed(
  () => validateCompare({ source: source.value, result: outputText.value, input: testInput.value }) === null,
)
// 当前版本是否基于左侧仍在变的设置生成。界面据此提示「重新优化」。
export const sourceStale = computed(() => versionIsStale(activeVersion.value, source.value, mode.value))
// 对比结果是否还对应当前的源文、对象、输入与版本。
export const compareSourceStale = computed(() => compareIsStale(testedSource.value, source.value))
export const compareModeStale = computed(() => Boolean(testedMode.value) && testedMode.value !== mode.value)
export const compareStale = computed(() => compareIsStale(testedInput.value, testInput.value))
export const compareVersionStale = computed(
  () => compareVersionIsStale(testedVersionId.value, activeVersion.value?.id || ''),
)
// 同一版被手动编辑后，正文变了但 id 没变，同样需要提示。
export const compareContentStale = computed(() => compareIsStale(testedContent.value, outputText.value))
// 版本链里混过系统 / 用户两种对象时，版本按钮需要标出各自的对象。
export const mixedModes = computed(() => new Set(versions.value.map(item => item.mode)).size > 1)
// 对比结果标题应显示实际被测的那一版，而不是永远写「当前版本」。
export const testedVersionLabel = computed(() => {
  if (!testedVersionId.value) return ''
  return versions.value.find(item => item.id === testedVersionId.value)?.label || ''
})

export function setMode(next) {
  if (busyGuard('optimize')) return
  mode.value = next === 'user' ? 'user' : 'system'
}

export function switchVersion(id) {
  // 流式写入绑定的是刚创建的版本，中途切走会让人以为「生成串版」。
  if (busyGuard('optimize')) return
  if (!versions.value.some(item => item.id === id)) return
  activeId.value = id
  error.value = ''
}

export function updateActiveContent(text) {
  if (running.value) return
  const current = activeVersion.value
  if (!current) return
  current.content = String(text ?? '')
  current.edited = true
  current.variables = variableReport(current.baseText, current.content)
  current.sourceLength = charCount(current.baseText)
  error.value = ''
}

export function dismissNotice() {
  notice.value = ''
}

export function stopOptimize() {
  if (!running.value || stopping.value) return
  stopping.value = true
  abortChat()
}

function removeVersion(id) {
  const index = versions.value.findIndex(item => item.id === id)
  if (index < 0) return
  versions.value.splice(index, 1)
  if (activeId.value === id) {
    const last = versions.value[versions.value.length - 1]
    activeId.value = last ? last.id : ''
  }
}

// 发起一轮优化。iterate 时把当前结果作为「上一版」，只按补充要求继续改；
// 否则从左侧源提示词重新开始一版。返回值仅供调用方记录，界面读 refs。
export async function optimize({ previous = '' } = {}) {
  const now = Date.now()
  // 提交守卫：在飞 / 已受理未响应 / 助手占用 / 刚提交过，一律拦下并给出文案。
  const rejected = submitGuard('optimize', now)
  if (rejected) return { ok: false, error: rejected }
  const base = String(previous ?? '').trim()
  const input = base || String(source.value ?? '').trim()
  const check = validateOptimize({ source: input, requirement: requirement.value })
  if (check) {
    error.value = check
    return { ok: false, error: check }
  }
  // 只有真正要发的请求才占用节流窗口：校验失败后改完再点，不该被「刚提交过」挡住。
  lastSubmitAt = now

  const messages = buildOptimizeMessages({
    mode: mode.value,
    source: input,
    requirement: requirement.value,
    previous: base,
  })

  error.value = ''
  notice.value = ''
  running.value = true
  // 提交已受理：从这一刻起界面就必须是「正在启动」态，直到首个字节到达。
  starting.value = true
  runKind.value = 'optimize'
  stopping.value = false
  const id = newId()
  versions.value.push({
    id,
    label: nextVersionLabel(versions.value),
    mode: mode.value,
    content: '',
    baseText: input,
    // 记下这一版对应的左侧源文：之后源文一改，旧结果就要标明「基于旧设置」。
    sourceText: String(source.value ?? '').trim(),
    requirement: String(requirement.value ?? '').trim(),
    sourceLength: charCount(input),
    at: Date.now(),
    stopped: false,
    edited: false,
    variables: null,
  })
  // 超出上限从最旧的一版丢；刚 push 的这版永远保留。
  if (versions.value.length > MAX_VERSIONS) versions.value.splice(0, versions.value.length - MAX_VERSIONS)
  const live = versions.value[versions.value.length - 1]
  activeId.value = id

  // 流式增量节流：60ms 合一批再渲染，避免每个 chunk 都触发整树更新。
  let buffer = ''
  let timer = null
  const flush = () => {
    timer = null
    if (buffer) {
      live.content += buffer
      buffer = ''
    }
  }
  const onDelta = text => {
    // 首个非空增量到达：启动态结束，界面转成「正在接收输出」。
    if (text) starting.value = false
    buffer += text
    if (!timer) timer = setTimeout(flush, 60)
  }

  try {
    // 本机大脑的唯一入口：壳侧解析回环端点与 Bearer，面板不碰任何 Key。
    const response = await chat({ selfHosted: true, model: AUTO_MODEL_ID, messages }, onDelta)
    flush()
    if (!response.ok && response.aborted) {
      live.stopped = true
      live.content = cleanOptimizedText(live.content)
      if (!live.content) removeVersion(id)
      notice.value = live.content ? '已停止，保留已生成的部分内容。' : '已停止。'
      return { ok: false, aborted: true }
    }
    if (!response.ok) {
      const reason = String(response.error || '请求失败')
      live.content = cleanOptimizedText(live.content)
      error.value = reason
      if (!live.content) removeVersion(id)
      return { ok: false, error: reason, partial: response.partial === true }
    }
    const cleaned = cleanOptimizedText(response.content || live.content)
    if (!cleaned) {
      removeVersion(id)
      error.value = '模型返回了空内容，请重试或补充更明确的修改要求。'
      return { ok: false, error: error.value }
    }
    live.content = cleaned
    live.variables = variableReport(input, cleaned)
    // 提示记在界面上，但不阻断结果：用户可能就是要删掉某个变量。
    if (live.variables.missing.length) {
      notice.value = `输出缺少原变量：${live.variables.missing.join('、')}。结果已保留，请确认是否需要手动补回。`
    } else if (live.variables.added.length) {
      notice.value = `输出新增了变量：${live.variables.added.join('、')}。请确认它确实是你需要的。`
    }
    // 本轮要求已经生效，清掉避免下一次迭代被重复携带。
    requirement.value = ''
    return { ok: true, content: cleaned }
  } catch (err) {
    const reason = String(err?.message || err)
    error.value = reason
    live.content = cleanOptimizedText(live.content)
    if (!live.content) removeVersion(id)
    return { ok: false, error: reason }
  } finally {
    if (timer) clearTimeout(timer)
    starting.value = false
    runKind.value = ''
    stopping.value = false
    running.value = false
  }
}

// 发起一次对比测试。两次请求顺序跑（见 testStage 处的说明），界面读那三个 ref。
export async function runCompare() {
  const now = Date.now()
  // 与优化同一条守卫：在飞 / 已受理未响应 / 助手占用 / 刚提交过，拦下并给出文案。
  const rejected = submitGuard('compare', now)
  if (rejected) return { ok: false, error: rejected }
  const check = validateCompare({ source: source.value, result: outputText.value, input: testInput.value })
  if (check) {
    testError.value = check
    return { ok: false, error: check }
  }
  lastSubmitAt = now
  testError.value = ''
  testNotice.value = ''
  testBase.value = ''
  testOptimized.value = ''
  // 先记下这一轮绑定的源文、对象、输入与版本，之后任何一项变化都能被界面识别出来。
  testedSource.value = source.value.trim()
  testedMode.value = mode.value
  testedInput.value = testInput.value.trim()
  testedVersionId.value = activeVersion.value?.id || ''
  testedContent.value = outputText.value.trim()
  running.value = true
  starting.value = true
  runKind.value = 'compare'
  stopping.value = false
  const common = { mode: mode.value, input: testInput.value }

  // 流式增量节流（60ms 合一批）：两段各写自己的 sink，不复用对方的缓冲区。
  const runOne = async (prompt, sink) => {
    let buffer = ''
    let timer = null
    const flush = () => {
      timer = null
      if (buffer) {
        sink.value += buffer
        buffer = ''
      }
    }
    const onDelta = text => {
      if (text) starting.value = false
      buffer += text
      if (!timer) timer = setTimeout(flush, 60)
    }
    try {
      return await chat(
        { selfHosted: true, model: AUTO_MODEL_ID, messages: buildTestMessages({ ...common, prompt }) },
        onDelta,
      )
    } finally {
      if (timer) clearTimeout(timer)
      flush()
    }
  }

  try {
    testStage.value = 'base'
    const first = await runOne(source.value, testBase)
    if (!first.ok) {
      if (first.aborted) testNotice.value = '已停止测试，以上是已经跑完的部分。'
      else testError.value = String(first.error || '源提示词的测试请求失败')
      return first
    }
    // 两段之间存在没有在飞请求的空档：此时点「停止」不会中止任何东西，
    // 必须在起第二段之前认账，否则停不下来。
    if (stopping.value) {
      testNotice.value = '已停止测试，以上是已经跑完的部分。'
      return { ok: false, aborted: true }
    }
    // 第二段是新的一次提交：重新启动态，界面才说得出「正在跑优化结果」。
    testStage.value = 'optimized'
    starting.value = true
    const second = await runOne(outputText.value, testOptimized)
    if (!second.ok && !second.aborted) {
      testError.value = String(second.error || '优化结果的测试请求失败')
    } else if (second.aborted) {
      testNotice.value = '已停止测试，以上是已经跑完的部分。'
    }
    return second
  } catch (err) {
    const reason = String(err?.message || err)
    testError.value = reason
    return { ok: false, error: reason }
  } finally {
    testStage.value = ''
    starting.value = false
    runKind.value = ''
    stopping.value = false
    running.value = false
  }
}

// 只清对比测试那一块（源提示词与版本链保留）。
export function clearCompare() {
  if (busyGuard('compare')) return false
  testInput.value = ''
  testBase.value = ''
  testOptimized.value = ''
  testedSource.value = ''
  testedMode.value = ''
  testedInput.value = ''
  testedVersionId.value = ''
  testedContent.value = ''
  testError.value = ''
  testNotice.value = ''
  return true
}

// 把当前版设为新源提示词：用户想从这一版分叉继续改时用。
export function useAsSource() {
  if (busyGuard('optimize')) return false
  const current = activeVersion.value
  if (!current?.content) return false
  source.value = current.content
  mode.value = current.mode === 'user' ? 'user' : 'system'
  requirement.value = ''
  error.value = ''
  notice.value = '已把当前结果设为新的源提示词，可以继续优化。'
  return true
}

// 「清空」清掉整个工作台（源提示词、要求、版本链与提示），不可撤销，界面走二次确认。
export function clearAll() {
  if (busyGuard('optimize')) return false
  mode.value = DEFAULT_MODE
  source.value = ''
  requirement.value = ''
  versions.value = []
  activeId.value = ''
  testInput.value = ''
  testBase.value = ''
  testOptimized.value = ''
  testedSource.value = ''
  testedMode.value = ''
  testedInput.value = ''
  testedVersionId.value = ''
  testedContent.value = ''
  testError.value = ''
  testNotice.value = ''
  error.value = ''
  notice.value = ''
  return true
}
