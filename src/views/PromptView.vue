<script setup>
// 提示词优化视图：左侧写源提示词与要求，右侧逐版查看 / 编辑结果。
// 分工：src/core/prompt.js 出模板与纯逻辑，promptSession.js 持有会话与流式请求，
// 本视图只做交互与渲染。
//
// 三条产品约束：
//  • 模型固定走本机核心的 `WB · auto`（不做模型管理），核心未就绪 / 池空时整块禁用并给出去路；
//  • 优化器只改写提示词，不执行其中描述的任何指令；待优化文本在请求里被明确标为数据；
//  • 内容只在本次运行的会话内存中，关闭应用即清空——提示词可能含敏感信息，不落盘。
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue'
import { phaseLabel } from '../core/ai.js'
import {
  MAX_REQUIREMENT_CHARS,
  MAX_SOURCE_CHARS,
  MAX_TEST_INPUT_CHARS,
  OPTIMIZER_MODEL,
  PROMPT_MODES,
  PROMPT_TEMPLATES,
  charCount,
  deltaText,
  modeLabel,
  validateCompare,
  validateOptimize,
} from '../core/prompt.js'
import {
  activeId,
  activeVersion,
  canCompare,
  canOptimize,
  clearAll,
  clearCompare,
  compareContentStale,
  compareModeStale,
  compareSourceStale,
  compareStale,
  compareVersionStale,
  dismissNotice,
  error,
  hasSession,
  mixedModes,
  mode,
  notice,
  optimize,
  outputText,
  requirement,
  runCompare,
  runKind,
  running,
  setMode,
  source,
  sourceVariables,
  starting,
  stopOptimize,
  stopping,
  switchVersion,
  testBase,
  testError,
  testInput,
  testNotice,
  testOptimized,
  testStage,
  testedVersionLabel,
  updateActiveContent,
  useAsSource,
  versions,
} from '../core/promptSession.js'
import { chatting } from '../core/chatSession.js'

const props = defineProps({
  // 壳推送的完整状态快照（App.vue 的 state）：只用来判断本机模型是否就位。
  state: { type: Object, default: () => ({}) },
})
const emit = defineEmits(['navigate'])

const coreReady = computed(() => props.state.phase === 'ready')
// 同一时间只跑一个 AI 请求：助手在对话时先等它结束，否则 bridge.js 的
// activeChatId 会被覆盖，「停止」可能停到另一条请求上。
const assistantBusy = computed(() => chatting.value)

// ── 使用门槛：模型就位才可用 ────────────────────────────────────────
// `WB · auto` 要有可转发的落点：核心就绪 + 至少一个探测通过的模型。
const gate = computed(() => {
  if (!coreReady.value) {
    return {
      blocked: true,
      title: `核心${phaseLabel(props.state.phase)}，提示词优化还不能用`,
      reason: '优化要用正在运行的核心作为模型服务。先到「模型与服务」把服务起起来，回来即可开始。',
      fix: 'models',
    }
  }
  const models = Array.isArray(props.state.models) ? props.state.models : []
  const available = Array.isArray(props.state.availableModels) ? props.state.availableModels : []
  if (!models.length) {
    return {
      blocked: true,
      title: '还没读取到免费模型',
      reason: '先到「模型与服务」点「读取免费模型」，名单到位后优化器才有模型可用。',
      fix: 'models',
    }
  }
  if (!available.length) {
    const probe = props.state.probe
    const pending = Array.isArray(probe?.pending) ? probe.pending.length : null
    const progress = probe?.running && Number.isFinite(pending)
      ? `（${Math.max(0, Math.min(models.length, models.length - pending))}/${models.length}）`
      : ''
    return {
      blocked: true,
      title: probe?.running ? '模型正在检测，还没有可用的' : '没有探测通过的模型',
      reason: probe?.running
        ? `已发现 ${models.length} 个模型，正在检测${progress}。检测通过后优化器才用得上本机模型，稍等即可。`
        : `已发现 ${models.length} 个模型，但当前 0 个探测通过。到「模型与服务」点「检测全部」后再试。`,
      fix: 'models',
    }
  }
  return { blocked: false }
})
const blocked = computed(() => gate.value.blocked === true)

// 当前输入是否可发：门槛 + 助手互斥 + 校验（空 / 超长）。
const canStart = computed(
  () => !running.value && !blocked.value && !assistantBusy.value && canOptimize.value,
)
// 迭代要求：有结果，并且写了本轮补充要求（否则「继续优化」与重新优化没有区别）。
const canIterate = computed(
  () => !running.value && !blocked.value && !assistantBusy.value && Boolean(outputText.value) && Boolean(requirement.value.trim()),
)

const validationHint = computed(() => {
  if (blocked.value || assistantBusy.value || running.value) return ''
  // 空输入不提前报错：按钮禁用已经说明还没到发送条件。
  if (!source.value.trim()) {
    return requirementOver.value
      ? `补充要求过长：上限 ${MAX_REQUIREMENT_CHARS} 字符`
      : ''
  }
  return validateOptimize({ source: source.value, requirement: requirement.value }) || ''
})

const startTitle = computed(() => {
  if (blocked.value) return gate.value.title
  if (assistantBusy.value) return 'AI 助手正在对话，等它结束后可用'
  if (stopping.value) return '正在中止这一轮…'
  if (starting.value) return '已经提交，正在等待模型响应…'
  if (running.value) return '正在生成中…'
  return validateOptimize({ source: source.value, requirement: requirement.value }) || '用 WB · auto 开始优化（Ctrl / ⌘ + Enter）'
})

// ── 即时反馈：点击后立刻进入可见的等待态 ────────────────────────────
// 「开始优化」到模型首字之间可能空等数秒。这段时间按钮必须自己说话，
// 并且给出独立的「停止」；只把按钮换成停止会让人看不出请求到底发出去没有。
const optimizing = computed(() => running.value && runKind.value === 'optimize')
const comparing = computed(() => running.value && runKind.value === 'compare')
const optimizePendingLabel = computed(() => (starting.value ? '等待模型响应…' : '正在接收输出…'))
const optimizeProgressText = computed(() => {
  if (stopping.value) return '正在停止，保留已生成的部分…'
  const tag = activeVersion.value ? `（${activeVersion.value.label}）` : ''
  return starting.value ? `已提交${tag}，正在等待模型响应…` : `正在接收输出${tag}…`
})
const compareStageLabel = computed(() => (testStage.value === 'base' ? '源提示词' : '当前版本'))
const comparePendingLabel = computed(() => {
  if (stopping.value) return '正在停止…'
  return starting.value ? `等待${compareStageLabel.value}输出…` : `${compareStageLabel.value}输出中…`
})

const lengthDelta = computed(() => deltaText(activeVersion.value?.baseText || '', outputText.value))
const sourceOver = computed(() => charCount(source.value) > MAX_SOURCE_CHARS)
const requirementOver = computed(() => charCount(requirement.value) > MAX_REQUIREMENT_CHARS)
const testOver = computed(() => charCount(testInput.value) > MAX_TEST_INPUT_CHARS)

// 结果生成后左侧设置又变了：把「这版基于旧设置」说清楚，避免把旧结果当新结果用。
const staleNotes = computed(() => {
  const current = activeVersion.value
  if (!current || running.value) return []
  const notes = []
  if (typeof current.sourceText === 'string' && current.sourceText !== source.value.trim()) notes.push('源提示词已修改')
  if (current.mode !== mode.value) notes.push('优化对象已切换')
  return notes
})
const staleHint = computed(
  () => (staleNotes.value.length ? `${staleNotes.value.join('，')}；${activeVersion.value?.label} 基于旧设置生成` : ''),
)

// ── 对比测试 ────────────────────────────────────────────────────────
// 与「优化」共用同一个在飞互斥（running），所以可发判据里必须带上它。
const compareHint = computed(() => {
  if (blocked.value || assistantBusy.value || running.value) return ''
  if (!testInput.value.trim()) return ''
  return validateCompare({ source: source.value, result: outputText.value, input: testInput.value }) || ''
})
const canStartCompare = computed(
  () => !running.value && !blocked.value && !assistantBusy.value && canCompare.value,
)
const compareTitle = computed(() => {
  if (blocked.value) return gate.value.title
  if (assistantBusy.value) return 'AI 助手正在对话，等它结束后可用'
  if (stopping.value) return '正在中止这一轮…'
  if (starting.value) return '已经提交，正在等待模型响应…'
  if (running.value) return '正在生成中…'
  return compareHint.value || '用同一份输入分别跑源提示词与当前版本'
})
const compareStaleNotes = computed(() => {
  // 没有产出时不存在「旧结果」，不要拿绑定信息吓唬人。
  if (running.value || (!testBase.value && !testOptimized.value)) return []
  const notes = []
  if (compareSourceStale.value) notes.push('源提示词已修改')
  if (compareModeStale.value) notes.push('优化对象已切换')
  if (compareStale.value) notes.push('测试输入已修改')
  if (compareVersionStale.value) notes.push('当前版本已切换')
  else if (compareContentStale.value) notes.push('当前版本内容已修改')
  return notes
})
const compareStaleHint = computed(
  () => (compareStaleNotes.value.length ? `${compareStaleNotes.value.join('，')}；下面的结果对应上一次对比` : ''),
)
function startCompare() {
  // 同上：被挡住时把原因写进折叠区的提示条，而不是静默 return。
  if (blocked.value) {
    testNotice.value = `${gate.value.title}。${gate.value.reason}`
    return
  }
  if (assistantBusy.value) {
    testNotice.value = 'AI 助手正在对话，同一时间只跑一个 AI 请求。'
    return
  }
  void runCompare()
  // 与优化一致：被守卫拒绝时留在开始按钮上，受理后把焦点交给可点的停止按钮。
  nextTick(() => (comparing.value ? compareStopRef.value : compareBtnRef.value)?.focus())
}

function versionTitle(item) {
  const parts = [item.label, modeLabel(item.mode)]
  if (item.requirement) parts.push(`本轮要求：${item.requirement}`)
  if (item.edited) parts.push('已手动编辑')
  if (item.stopped) parts.push('生成已中断')
  return parts.join(' · ')
}

// ── 流式跟随 ────────────────────────────────────────────────────────
// 只在本来就贴着底部时跟随；用户上翻查看前文后，不再抢走滚动位置。
const sourceRef = ref(null)
const outputRef = ref(null)
const baseOutRef = ref(null)
const optimizedOutRef = ref(null)
const optimizeBtnRef = ref(null)
const optimizeStopRef = ref(null)
const compareBtnRef = ref(null)
const compareStopRef = ref(null)
// 用 ref 而不是普通变量：上翻后要立刻出现「回到最新」，不能等下一次重渲染。
const followOutput = ref(true)
const followBase = ref(true)
const followOptimized = ref(true)

function nearBottom(el) {
  return !el || el.scrollHeight - el.scrollTop - el.clientHeight < 40
}
function onOutputScroll() {
  followOutput.value = nearBottom(outputRef.value)
}
function onBaseScroll() {
  followBase.value = nearBottom(baseOutRef.value)
}
function onOptimizedScroll() {
  followOptimized.value = nearBottom(optimizedOutRef.value)
}
function scrollToBottom(elRef) {
  nextTick(() => {
    const el = elRef.value
    if (el) el.scrollTop = el.scrollHeight
  })
}
function jumpToLatest() {
  followOutput.value = true
  scrollToBottom(outputRef)
}

watch(outputText, () => {
  if (running.value && followOutput.value) scrollToBottom(outputRef)
}, { flush: 'post' })
watch(testBase, () => {
  if (testStage.value === 'base' && followBase.value) scrollToBottom(baseOutRef)
}, { flush: 'post' })
watch(testOptimized, () => {
  if (testStage.value === 'optimized' && followOptimized.value) scrollToBottom(optimizedOutRef)
}, { flush: 'post' })
watch(running, active => {
  if (!active) return
  followOutput.value = true
  followBase.value = true
  followOptimized.value = true
})
// 切到另一版时回到开头；生成中已在用「贴底跟随」，两者不会打架。
watch(activeId, () => {
  followOutput.value = true
  nextTick(() => {
    if (outputRef.value) outputRef.value.scrollTop = 0
  })
})
// 停止按钮随本轮结束消失，焦点会掉回 body。只在这种「被动丢焦点」时接回
// 同一个开始按钮；用户自己移到别处时不抢。
watch(runKind, (next, prev) => {
  if (next !== '' || !prev) return
  nextTick(() => {
    const active = document.activeElement
    if (active && active !== document.body) return
    const target = prev === 'compare' ? compareBtnRef.value : optimizeBtnRef.value
    target?.focus()
  })
})

// 版本标签在模板里以 `{{变量}}` 的形态展示；模板本身不能写裸 mustache（会被 Vue 当成插值）。
function varToken(name) {
  return `{{${name}}}`
}

// ── 二次确认：不可撤销 / 会覆盖原文的动作先确认一次 ──────────────────
// 但双击的两次点击间隔通常 < 300ms；没有这条最短间隔，确认按钮会被顺手双击
// 直接「确认」掉，二次确认形同虚设。第一次点击已经改变按钮文案，反馈是即时的。
const CONFIRM_ARM_MS = 400
const clearArmed = ref(false)
let clearArmedAt = 0
let clearTimer = null
function onClear() {
  // 生成中被点（按钮已禁用，属防御分支）时交给 clearAll 说话，不静默 return。
  if (!clearArmed.value) {
    clearArmed.value = true
    clearArmedAt = Date.now()
    clearTimer = setTimeout(() => { clearArmed.value = false }, 3000)
    return
  }
  if (Date.now() - clearArmedAt < CONFIRM_ARM_MS) return
  if (clearTimer) clearTimeout(clearTimer)
  clearArmed.value = false
  if (clearAll()) nextTick(() => sourceRef.value?.focus())
}

// 「设为源提示词」会覆盖左侧原文，而原文不在别处留存；非空且内容不同时先确认一次。
const sourceArmed = ref(false)
let sourceArmedAt = 0
let sourceTimer = null
function onUseAsSource() {
  // 生成中的拒绝由 useAsSource 自己给出提示，这里不静默吞掉点击。
  if (!outputText.value) return
  const current = source.value.trim()
  const wouldOverwrite = Boolean(current) && current !== outputText.value.trim()
  if (wouldOverwrite && !sourceArmed.value) {
    sourceArmed.value = true
    sourceArmedAt = Date.now()
    sourceTimer = setTimeout(() => { sourceArmed.value = false }, 3000)
    return
  }
  if (wouldOverwrite && Date.now() - sourceArmedAt < CONFIRM_ARM_MS) return
  if (sourceTimer) {
    clearTimeout(sourceTimer)
    sourceTimer = null
  }
  sourceArmed.value = false
  if (!useAsSource()) return
  nextTick(() => {
    const el = sourceRef.value
    if (!el) return
    el.focus()
    el.setSelectionRange(el.value.length, el.value.length)
  })
}

// ── 复制：优先 Clipboard API，失败回落到临时 textarea ───────────────
// Tauri WebView 里 Clipboard API 的可用性随平台/权限而变，不能让「复制」点了没反应。
const copyState = ref('') // '' | 'copying' | 'done' | 'fail'
const copiedSide = ref('') // '' | 'base' | 'optimized' | 'fail'
// 剪贴板写入是异步的；在它返回前按钮必须已经在说话，且不被连点重复触发。
const copyBusy = ref(false)
const copyBusySide = ref('') // '' | 'base' | 'optimized'
let copyTimer = null

async function writeClipboard(text) {
  let ok = false
  try {
    if (window.navigator?.clipboard?.writeText) {
      await window.navigator.clipboard.writeText(text)
      ok = true
    }
  } catch {
    ok = false
  }
  if (ok) return true
  const area = document.createElement('textarea')
  area.value = text
  area.setAttribute('readonly', '')
  area.style.position = 'fixed'
  area.style.opacity = '0'
  document.body.appendChild(area)
  area.select()
  try {
    ok = document.execCommand('copy')
  } catch {
    ok = false
  }
  area.remove()
  return ok
}

function flashCopy() {
  if (copyTimer) clearTimeout(copyTimer)
  copyTimer = setTimeout(() => {
    copyState.value = ''
    copiedSide.value = ''
  }, 2400)
}

async function copyResult() {
  const text = outputText.value
  if (!text || running.value || copyBusy.value) return
  copyBusy.value = true
  copyState.value = 'copying'
  try {
    const ok = await writeClipboard(text)
    copyState.value = ok ? 'done' : 'fail'
  } finally {
    copyBusy.value = false
    flashCopy()
  }
}

async function copyCompare(side) {
  const text = side === 'base' ? testBase.value : testOptimized.value
  if (!text || testStage.value || copyBusySide.value) return
  copyBusySide.value = side
  try {
    const ok = await writeClipboard(text)
    copiedSide.value = ok ? side : 'fail'
  } finally {
    copyBusySide.value = ''
    flashCopy()
  }
}

function compareCopyLabel(side) {
  if (copyBusySide.value === side) return '复制中…'
  if (copiedSide.value === side) return '已复制'
  if (copiedSide.value === 'fail') return '复制失败'
  return '复制'
}

// ── 动作入口 ────────────────────────────────────────────────────────
function start(iterate) {
  // 被挡住时必须说话：静默 return 就是「点了没反应」。
  // 在飞 / 已提交未响应 / 刚提交过由会话层的 submitGuard 拦下并给出节流后的文案。
  if (blocked.value) {
    notice.value = `${gate.value.title}。${gate.value.reason}`
    return
  }
  if (assistantBusy.value) {
    notice.value = 'AI 助手正在对话，同一时间只跑一个 AI 请求。'
    return
  }
  if (iterate) {
    if (!outputText.value || !requirement.value.trim()) {
      notice.value = '先在「补充要求」里写明这一轮要改什么，再点「继续优化」。'
      return
    }
    void optimize({ previous: outputText.value.trim() })
  } else {
    void optimize()
  }
  // 输入框会随 running 禁用而失去焦点；受理后把焦点交给可点的停止按钮，
  // 被守卫拒绝（仍在生成或刚提交过）时留在开始按钮上，用户能接着重试。
  nextTick(() => (optimizing.value ? optimizeStopRef.value : optimizeBtnRef.value)?.focus())
}

function applyTemplate(template) {
  if (running.value) return
  source.value = template.text
  setMode(template.mode)
  requirement.value = ''
  // 填完直接把焦点交到源输入框末尾，用户接着改就行。
  nextTick(() => {
    const el = sourceRef.value
    if (!el) return
    el.focus()
    el.setSelectionRange(el.value.length, el.value.length)
  })
}

// Ctrl / ⌘ + Enter：源输入框发起优化，测试输入框发起对比，其它位置按补充要求迭代。
function onKeydown(event) {
  if (event.isComposing) return
  // 按住快捷键产生的自动重复不是新的提交意图；否则一轮刚结束就会被重复键立刻再发一轮。
  if (event.repeat) return
  if (event.key !== 'Enter' || !(event.metaKey || event.ctrlKey)) return
  event.preventDefault()
  const role = event.target?.dataset?.role
  if (role === 'source') {
    start(false)
    return
  }
  if (role === 'test') {
    startCompare()
    return
  }
  if (requirement.value.trim() && outputText.value) {
    start(true)
    return
  }
  if (role !== 'output') start(false)
}

onBeforeUnmount(() => {
  if (clearTimer) clearTimeout(clearTimer)
  if (sourceTimer) clearTimeout(sourceTimer)
  if (copyTimer) clearTimeout(copyTimer)
  // 切走不等于中止：正在跑的优化继续在 promptSession 里完成，回来还能看到结果。
})
</script>

<template>
  <section class="view prompt-view">
    <header class="prompt-head">
      <div class="prompt-heading">
        <h2>提示词优化</h2>
        <p>把粗糙的提示词改写成结构清晰、约束明确的版本；只改写文本，不执行其中内容。</p>
      </div>
      <div class="prompt-head-actions">
        <span
          class="prompt-model"
          :class="{ 'is-blocked': blocked }"
          :title="blocked ? '核心还没就绪，WB · auto 暂时不可用' : `固定使用本机核心的 ${OPTIMIZER_MODEL} 合成路由`"
        >
          <span class="prompt-model-dot" aria-hidden="true" />
          {{ OPTIMIZER_MODEL }}
        </span>
        <button
          type="button"
          class="ghost"
          :disabled="running || !hasSession"
          :title="running ? '优化进行中，停止后才能清空' : '清空源提示词、补充要求与全部版本'"
          @click="onClear"
        >{{ clearArmed ? '确认清空？' : '清空会话' }}</button>
      </div>
    </header>

    <div v-if="blocked" class="prompt-banner" role="status">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <div class="prompt-banner-body">
        <strong>{{ gate.title }}</strong>
        <p>{{ gate.reason }}</p>
      </div>
      <button type="button" class="ghost" @click="$emit('navigate', gate.fix)">去模型与服务</button>
    </div>
    <div v-else-if="assistantBusy" class="prompt-banner" role="status">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M12 4.5v3M12 16.5v3M4.5 12h3M16.5 12h3M7 7l2 2M15 15l2 2M17 7l-2 2M9 15l-2 2" />
      </svg>
      <div class="prompt-banner-body">
        <strong>AI 助手正在对话</strong>
        <p>同一时间只跑一个 AI 请求，避免两边互相中断。等助手这一轮结束，或先到「AI 助手」停止它。</p>
      </div>
      <button type="button" class="ghost" @click="$emit('navigate', 'assistant')">去 AI 助手</button>
    </div>

    <div class="prompt-workspace">
      <!-- 左：源提示词与要求 -->
      <section class="prompt-pane">
        <header class="prompt-pane-head">
          <div>
            <h3 id="promptSourceHeading">源提示词</h3>
            <p id="promptSourceDesc" class="prompt-pane-sub">要优化的原文；结构不用讲究，模型会重整</p>
          </div>
          <span id="promptSourceCount" class="prompt-count" :class="{ over: sourceOver }">
            {{ charCount(source) }} / {{ MAX_SOURCE_CHARS }}
          </span>
        </header>

        <div class="prompt-modes" role="group" aria-label="优化对象">
          <button
            v-for="item in PROMPT_MODES"
            :key="item.id"
            type="button"
            class="prompt-mode"
            :class="{ active: mode === item.id }"
            :aria-pressed="mode === item.id"
            :disabled="running"
            @click="setMode(item.id)"
          >
            <strong>{{ item.label }}</strong>
            <span>{{ item.hint }}</span>
          </button>
        </div>

        <textarea
          ref="sourceRef"
          v-model="source"
          class="prompt-source"
          data-role="source"
          :disabled="running"
          aria-labelledby="promptSourceHeading"
          :aria-describedby="validationHint ? 'promptSourceDesc promptSourceCount promptValidationHint' : 'promptSourceDesc promptSourceCount'"
          :aria-invalid="sourceOver"
          spellcheck="false"
          placeholder="例如：帮我把会议纪要整理成三个要点，语气正式一点，保留所有数字和人名。"
          @keydown="onKeydown"
        />

        <div v-if="!source.trim() && !running" class="prompt-templates">
          <p class="prompt-templates-title">没有思路？从模板开始</p>
          <div class="prompt-template-list">
            <button
              v-for="item in PROMPT_TEMPLATES"
              :key="item.id"
              type="button"
              class="prompt-template"
              :title="item.mode === 'user' ? '用户提示词模板' : '系统提示词模板'"
              @click="applyTemplate(item)"
            >{{ item.label }}</button>
          </div>
        </div>

        <div v-if="sourceVariables.length" class="prompt-vars">
          <span class="prompt-vars-label">检测到占位符</span>
          <code v-for="name in sourceVariables" :key="name" v-text="varToken(name)" />
          <em>结果会尽量原样保留</em>
        </div>

        <div class="prompt-label-row">
          <label id="promptRequirementLabel" class="prompt-label" for="promptRequirement">
            补充要求 <em>可选</em>
          </label>
          <span
            id="promptRequirementCount"
            class="prompt-count"
            :class="{ over: requirementOver }"
          >
            {{ charCount(requirement) }} / {{ MAX_REQUIREMENT_CHARS }}
          </span>
        </div>
        <textarea
          id="promptRequirement"
          v-model="requirement"
          class="prompt-requirement"
          data-role="requirement"
          :disabled="running"
          aria-labelledby="promptRequirementLabel"
          :aria-describedby="validationHint ? 'promptRequirementCount promptValidationHint' : 'promptRequirementCount'"
          :aria-invalid="requirementOver"
          spellcheck="false"
          :placeholder="`例如：再精简一点，固定输出 JSON，保留原有占位符（上限 ${MAX_REQUIREMENT_CHARS} 字符）`"
          @keydown="onKeydown"
        />

        <div class="prompt-actions" :aria-busy="optimizing ? 'true' : 'false'">
          <!-- 点击后立刻换成「待响应 + 停止」：等待模型首字的数秒里按钮必须自己说话 -->
          <template v-if="optimizing">
            <button type="button" class="primary is-pending" disabled :title="optimizeProgressText">
              <span class="spinner" aria-hidden="true" />{{ optimizePendingLabel }}
            </button>
            <button
              ref="optimizeStopRef"
              type="button"
              class="ghost"
              :disabled="stopping"
              @click="stopOptimize"
            >{{ stopping ? '正在停止…' : '停止' }}</button>
          </template>
          <button
            v-else
            ref="optimizeBtnRef"
            type="button"
            class="primary"
            :disabled="!canStart"
            :title="startTitle"
            @click="start(false)"
          >开始优化</button>
          <span class="prompt-kbd">Ctrl / ⌘ + Enter</span>
        </div>
        <p v-if="validationHint" id="promptValidationHint" class="prompt-inline-hint">{{ validationHint }}</p>
      </section>

      <!-- 右：结果与版本 -->
      <section class="prompt-pane" :aria-busy="optimizing ? 'true' : 'false'">
        <header class="prompt-pane-head">
          <div>
            <h3 id="promptOutputHeading">优化结果</h3>
            <p class="prompt-pane-sub">
              {{ activeVersion ? `${activeVersion.label} · ${modeLabel(activeVersion.mode)}` : '模型只返回提示词全文' }}
            </p>
          </div>
          <span v-if="versions.length" class="prompt-count">{{ versions.length }} 个版本</span>
        </header>

        <div v-if="staleHint" class="prompt-stale" role="status">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M20 12a8 8 0 1 1-2.34-5.66M20 4v5h-5" />
          </svg>
          <span>{{ staleHint }}</span>
          <button type="button" class="ghost" :disabled="!canStart" :title="startTitle" @click="start(false)">重新优化</button>
        </div>

        <div v-if="versions.length" class="prompt-versions" role="group" aria-label="优化版本">
          <button
            v-for="item in versions"
            :key="item.id"
            type="button"
            class="prompt-version"
            :class="{ active: item.id === activeVersion?.id }"
            :aria-pressed="item.id === activeVersion?.id"
            :disabled="running"
            :title="versionTitle(item)"
            @click="switchVersion(item.id)"
          >
            {{ item.label }}
            <span v-if="mixedModes" class="prompt-version-mode">{{ item.mode === 'user' ? '用户' : '系统' }}</span>
            <span v-if="item.edited" class="prompt-version-tag">已编辑</span>
            <span v-if="item.stopped" class="prompt-version-tag">已停止</span>
          </button>
        </div>

        <!-- 只有优化轮次在这里显示进度：对比轮次的进度在折叠区里，两者共用 running -->
        <div v-if="optimizing" class="prompt-progress" role="status">
          <div class="busy-bar" aria-hidden="true"></div>
          <p><span class="spinner" aria-hidden="true" />{{ optimizeProgressText }}</p>
          <p v-if="starting" class="prompt-progress-hint">模型首个字到达后开始逐字显示；等待期间可随时停止。</p>
        </div>

        <p v-if="error" class="prompt-error" role="alert">{{ error }}</p>
        <p v-if="notice" class="prompt-notice" role="status">
          <span>{{ notice }}</span>
          <button type="button" class="ghost prompt-notice-close" aria-label="关闭提示" @click="dismissNotice">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" aria-hidden="true">
              <path d="M7 7l10 10M17 7L7 17" />
            </svg>
          </button>
        </p>

        <div v-if="!versions.length" class="prompt-empty">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M5 19l7.5-7.5M14.5 9.5l-2-2M12 4.2l.9 2.1 2.1.9-2.1.9-.9 2.1-.9-2.1L9 7.2l2.1-.9ZM18 12l.6 1.4 1.4.6-1.4.6-.6 1.4-.6-1.4-1.4-.6 1.4-.6ZM6 5.2l.5 1.3 1.3.5-1.3.5L6 8.8l-.5-1.3L4.2 7l1.3-.5Z" />
          </svg>
          <strong>结果会出现在这里</strong>
          <p>左侧填写提示词后点「开始优化」。<code>{{ OPTIMIZER_MODEL }}</code> 会按当次请求自动选一个可用模型，逐字流式返回。出结果后可展开「对比测试」，用同一份输入比较新旧效果。</p>
        </div>

        <template v-else>
          <textarea
            ref="outputRef"
            class="prompt-output"
            data-role="output"
            :value="outputText"
            :disabled="running"
            aria-labelledby="promptOutputHeading"
            aria-describedby="promptOutputDesc"
            spellcheck="false"
            @input="updateActiveContent($event.target.value)"
            @scroll="onOutputScroll"
            @keydown="onKeydown"
          />

          <div
            v-if="activeVersion?.variables?.missing?.length || activeVersion?.variables?.added?.length"
            class="prompt-var-report"
          >
            <p v-if="activeVersion.variables.missing.length" class="prompt-var-warn" role="status">
              缺少原占位符：
              <code v-for="name in activeVersion.variables.missing" :key="name" v-text="varToken(name)" />
            </p>
            <p v-if="activeVersion.variables.added.length" class="prompt-var-warn" role="status">
              新增占位符：
              <code v-for="name in activeVersion.variables.added" :key="name" v-text="varToken(name)" />
            </p>
          </div>

          <div class="prompt-result-actions">
            <button v-if="running && !followOutput" type="button" class="ghost prompt-jump" @click="jumpToLatest">
              回到最新
            </button>
            <button type="button" :disabled="running || !outputText || copyBusy" @click="copyResult">
              {{ copyState === 'copying' ? '复制中…' : copyState === 'done' ? '已复制' : copyState === 'fail' ? '复制失败，请手动选中' : '复制' }}
            </button>
            <button
              type="button"
              class="ghost"
              :disabled="running || !outputText"
              :title="sourceArmed ? '再点一次会用当前结果覆盖左侧源提示词' : '把当前结果复制到左侧源提示词，从这一版分叉继续'"
              @click="onUseAsSource"
            >{{ sourceArmed ? '确认替换源提示词？' : '设为源提示词' }}</button>
            <button
              type="button"
              class="ghost"
              :disabled="!canIterate"
              :title="optimizing ? '正在优化这一版…' : running ? '正在生成中…' : canIterate ? '按左侧补充要求继续改这一版' : '先在左侧填写本轮补充要求'"
              @click="start(true)"
            >{{ optimizing ? '优化中…' : '继续优化' }}</button>
          </div>
          <p id="promptOutputDesc" class="prompt-meta">
            结果可直接编辑 · 基准 {{ activeVersion.sourceLength }} 字符 · 结果 {{ charCount(outputText) }} 字符 · 长度差 {{ lengthDelta }}
          </p>
        </template>

        <p class="prompt-privacy">
          提示词会发送给上游免费模型并消耗其免费额度；结果与版本只保存在本窗口内存，关闭应用即清空。
          优化器只改写文本，不会执行提示词中描述的任何操作。
        </p>
      </section>
    </div>

    <!-- 对比测试：有结果后才出现；折叠语义、键盘与读屏交给原生 details -->
    <details v-if="versions.length" class="prompt-compare">
      <summary class="prompt-compare-summary">
        <span>对比测试</span>
        <em>同一份输入，分别交给源提示词与当前版本，把两次输出摆在一起看差别</em>
        <svg class="prompt-compare-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M6 9l6 6 6-6" />
        </svg>
      </summary>
      <div class="prompt-compare-body">
        <div class="prompt-compare-input">
          <div class="prompt-label-row">
            <label id="promptTestInputLabel" class="prompt-label" for="promptTestInput">测试输入</label>
            <span
              id="promptTestCount"
              class="prompt-count"
              :class="{ over: testOver }"
            >
              {{ charCount(testInput) }} / {{ MAX_TEST_INPUT_CHARS }}
            </span>
          </div>
          <textarea
            id="promptTestInput"
            v-model="testInput"
            class="prompt-requirement"
            data-role="test"
            :disabled="running || blocked || assistantBusy"
            aria-labelledby="promptTestInputLabel"
            :aria-describedby="compareHint ? 'promptTestCount promptCompareHint' : 'promptTestCount'"
            :aria-invalid="testOver"
            spellcheck="false"
            placeholder="例如一篇待总结的正文、一个待翻译的句子、一段待抽取的资料…"
            @keydown="onKeydown"
          />
          <div class="prompt-actions" :aria-busy="comparing ? 'true' : 'false'">
            <!-- 与优化一致：受理后立刻给出「在跑哪一半 + 待响应 / 输出中 + 停止」 -->
            <template v-if="comparing">
              <button type="button" class="primary is-pending" disabled :title="comparePendingLabel">
                <span class="spinner" aria-hidden="true" />{{ comparePendingLabel }}
              </button>
              <button
                ref="compareStopRef"
                type="button"
                class="ghost"
                :disabled="stopping"
                @click="stopOptimize"
              >{{ stopping ? '正在停止…' : '停止' }}</button>
            </template>
            <button
              v-else
              ref="compareBtnRef"
              type="button"
              class="primary"
              :disabled="!canStartCompare"
              :title="compareTitle"
              @click="startCompare"
            >开始对比</button>
            <button
              v-if="!comparing && (testBase || testOptimized)"
              type="button"
              class="ghost"
              :disabled="running"
              :title="running ? '正在生成中，等这一轮结束后再清空' : '清空对比测试的输入与结果'"
              @click="clearCompare"
            >清空测试</button>
            <span v-if="!comparing" class="prompt-kbd">Ctrl / ⌘ + Enter</span>
          </div>
          <p v-if="comparing" class="visually-hidden" role="status">{{ comparePendingLabel }}</p>
          <div v-if="compareStaleHint" class="prompt-stale" role="status">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M20 12a8 8 0 1 1-2.34-5.66M20 4v5h-5" />
            </svg>
            <span>{{ compareStaleHint }}</span>
            <button type="button" class="ghost" :disabled="!canStartCompare" :title="compareTitle" @click="startCompare">重新对比</button>
          </div>
          <p v-if="compareHint" id="promptCompareHint" class="prompt-inline-hint">{{ compareHint }}</p>
          <p v-if="testError" class="prompt-error" role="alert">{{ testError }}</p>
          <p v-if="testNotice" class="prompt-notice" role="status"><span>{{ testNotice }}</span></p>
        </div>
        <div v-if="!testStage && !testBase && !testOptimized" class="prompt-compare-empty">
          <p>填好测试输入后点「开始对比」，源提示词与当前版本的输出会分别出现在这里。</p>
        </div>
        <div v-else class="prompt-compare-out">
          <div class="prompt-compare-col">
            <div class="prompt-compare-col-head">
              <h4>源提示词的输出</h4>
              <button
                type="button"
                class="ghost prompt-copy"
                :disabled="!testBase || testStage !== '' || copyBusySide !== ''"
                @click="copyCompare('base')"
              >{{ compareCopyLabel('base') }}</button>
            </div>
            <pre
              ref="baseOutRef"
              class="prompt-compare-text"
              tabindex="0"
              aria-label="源提示词的输出"
              @scroll="onBaseScroll"
            >{{ testBase || '—' }}</pre>
          </div>
          <div class="prompt-compare-col">
            <div class="prompt-compare-col-head">
              <h4>{{ testedVersionLabel ? `${testedVersionLabel} 的输出` : '当前版本的输出' }}</h4>
              <button
                type="button"
                class="ghost prompt-copy"
                :disabled="!testOptimized || testStage !== '' || copyBusySide !== ''"
                @click="copyCompare('optimized')"
              >{{ compareCopyLabel('optimized') }}</button>
            </div>
            <pre
              ref="optimizedOutRef"
              class="prompt-compare-text"
              tabindex="0"
              :aria-label="testedVersionLabel ? `${testedVersionLabel} 的输出` : '当前版本的输出'"
              @scroll="onOptimizedScroll"
            >{{ testOptimized || '—' }}</pre>
          </div>
        </div>
      </div>
    </details>
  </section>
</template>

<style scoped>
.prompt-view { min-height: 0; }
.prompt-head { display: flex; align-items: flex-start; justify-content: space-between; gap: var(--sp-4); flex-wrap: wrap; }
.prompt-heading h2 { margin: 0; font-size: var(--fs-xl); }
.prompt-heading p { margin: var(--sp-1) 0 0; color: var(--muted-strong); font-size: var(--fs-sm); line-height: var(--lh-loose); max-width: 720px; }
.prompt-head-actions { display: flex; align-items: center; gap: var(--sp-2); }
.prompt-model {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 6px 11px;
  border-radius: 999px;
  border: 1px solid color-mix(in srgb, var(--green) 28%, var(--line));
  background: var(--green-bg);
  color: var(--text);
  font-size: var(--fs-xs);
  font-weight: 600;
}
.prompt-model-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--green); flex-shrink: 0; }
/* 核心未就绪时，绿色状态点不能继续冒充「可用」 */
.prompt-model.is-blocked { background: var(--row); border-color: var(--line); color: var(--muted-strong); }
.prompt-model.is-blocked .prompt-model-dot { background: var(--muted); }

/* 门槛 / 互斥横幅：橙色是说明性阻断，不是高危动作，不用 --danger */
.prompt-banner {
  display: flex;
  align-items: flex-start;
  gap: var(--sp-3);
  padding: var(--sp-3);
  border: 1px solid color-mix(in srgb, var(--orange) 32%, var(--line));
  border-radius: var(--radius-m);
  background: var(--orange-bg);
}
.prompt-banner > svg { width: 16px; height: 16px; flex-shrink: 0; margin-top: 2px; color: var(--orange); }
.prompt-banner-body { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.prompt-banner-body strong { font-size: var(--fs-sm); color: var(--text); }
.prompt-banner-body p { margin: 0; font-size: var(--fs-xs); line-height: var(--lh-loose); color: var(--muted-strong); }

.prompt-workspace {
  flex: 1;
  min-height: 460px;
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: var(--sp-4);
}
.prompt-pane {
  min-width: 0;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-4);
  border: 1px solid var(--line);
  border-radius: var(--radius-l);
  background: var(--surface-raised);
  box-shadow: var(--shadow-s);
}
.prompt-pane-head { display: flex; align-items: flex-start; justify-content: space-between; gap: var(--sp-2); }
.prompt-pane-head h3 { margin: 0; font-size: var(--fs-base); }
.prompt-pane-sub { margin: 2px 0 0; font-size: var(--fs-xs); color: var(--muted-strong); }
.prompt-count { font-size: var(--fs-xs); color: var(--muted-strong); white-space: nowrap; font-variant-numeric: tabular-nums; }
.prompt-count.over { color: var(--orange); font-weight: 600; }

.prompt-modes { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: var(--sp-2); }
.prompt-mode {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
  padding: var(--sp-2) var(--sp-3);
  text-align: left;
  background: var(--row);
  white-space: normal;
}
.prompt-mode strong { font-size: var(--fs-sm); }
.prompt-mode span { font-size: var(--fs-xs); color: var(--muted-strong); line-height: var(--lh-base); }
.prompt-mode.active {
  background: var(--green-bg);
  border-color: color-mix(in srgb, var(--green) 45%, var(--line));
  box-shadow: inset 0 0 0 1px var(--nav-active-ring);
}

.prompt-source,
.prompt-requirement,
.prompt-output {
  font: inherit;
  font-size: var(--fs-sm);
  line-height: var(--lh-loose);
  color: var(--text);
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
  padding: 10px 12px;
  resize: none;
  transition: border-color var(--dur-1) var(--ease-standard);
}
.prompt-source { flex: 1 1 220px; min-height: 180px; }
.prompt-requirement { min-height: 66px; max-height: 110px; }
.prompt-output { flex: 1 1 260px; min-height: 220px; font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace; }
.prompt-source:hover:not(:disabled),
.prompt-requirement:hover:not(:disabled),
.prompt-output:hover:not(:disabled) { border-color: color-mix(in srgb, var(--green) 30%, var(--line)); }
.prompt-source:focus-visible,
.prompt-requirement:focus-visible,
.prompt-output:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.prompt-source:disabled,
.prompt-requirement:disabled,
.prompt-output:disabled { opacity: .62; }

.prompt-templates { display: flex; flex-direction: column; gap: var(--sp-2); padding: var(--sp-2) 0; }
.prompt-templates-title { margin: 0; font-size: var(--fs-xs); color: var(--muted-strong); }
.prompt-template-list { display: flex; flex-wrap: wrap; gap: 6px; }
.prompt-template {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-height: 30px;
  font-size: var(--fs-xs);
  padding: 5px 10px;
  border-radius: 999px;
  background: var(--row);
  color: var(--muted-strong);
}
.prompt-template:hover:not(:disabled) { color: var(--text); }

.prompt-vars { display: flex; align-items: center; flex-wrap: wrap; gap: 6px; font-size: var(--fs-xs); }
.prompt-vars-label { color: var(--muted-strong); }
.prompt-vars code,
.prompt-var-warn code {
  font-family: ui-monospace, monospace;
  font-size: var(--fs-xs);
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--row);
  border: 1px solid var(--line);
}
.prompt-vars em { font-style: normal; color: var(--muted-strong); }
.prompt-vars code { max-width: 100%; overflow-wrap: anywhere; }

.prompt-label-row { display: flex; align-items: center; justify-content: space-between; gap: var(--sp-2); }
.prompt-label { display: flex; align-items: center; gap: 6px; font-size: var(--fs-sm); font-weight: 600; }
.prompt-label em { font-style: normal; font-weight: 400; font-size: var(--fs-xs); color: var(--muted-strong); }
.prompt-actions { display: flex; align-items: center; flex-wrap: wrap; gap: var(--sp-3); }
/* 提交后按钮换成独立的最小宽度，切换时不把旁边的快捷键提示挤走 */
.prompt-actions > button { min-width: 80px; }
/* 待响应态虽然 disabled（防重复提交），但不能看起来像坏掉的按钮 */
.prompt-actions .is-pending:disabled {
  opacity: 1;
  cursor: progress;
  background: var(--green-bg);
  color: var(--green);
  border-color: color-mix(in srgb, var(--green) 40%, var(--line));
}
.prompt-kbd { font-size: var(--fs-xs); color: var(--muted-strong); }
.prompt-inline-hint { margin: 0; font-size: var(--fs-xs); color: var(--orange); }
.prompt-progress-hint { margin: 0; font-size: var(--fs-xs); color: var(--muted-strong); }

.prompt-versions { display: flex; flex-wrap: wrap; gap: 6px; }
.prompt-version {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  min-height: 30px;
  font-size: var(--fs-xs);
  padding: 5px 10px;
  border-radius: 999px;
  background: var(--row);
  color: var(--muted-strong);
}
.prompt-version.active {
  background: var(--green-bg);
  border-color: color-mix(in srgb, var(--green) 45%, var(--line));
  color: var(--text);
  font-weight: 600;
}
.prompt-version-mode { font-size: var(--fs-xs); font-weight: 400; color: var(--muted-strong); }
.prompt-version-tag { font-size: var(--fs-xs); color: var(--orange); }

/* 结果与当前设置脱节：说明性提示，不是错误，不打断编辑 */
.prompt-stale {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: 6px var(--sp-3);
  border: 1px solid color-mix(in srgb, var(--orange) 24%, var(--line));
  border-radius: var(--radius-s);
  background: var(--row);
  font-size: var(--fs-xs);
  line-height: var(--lh-base);
  color: var(--muted-strong);
}
.prompt-stale > svg { width: 13px; height: 13px; flex-shrink: 0; color: var(--orange); }
.prompt-stale > span { flex: 1; min-width: 0; }
.prompt-stale > button { flex-shrink: 0; padding: 3px 8px; font-size: var(--fs-xs); }

.prompt-progress { display: flex; flex-direction: column; gap: 6px; }
.prompt-progress p { margin: 0; display: flex; align-items: center; font-size: var(--fs-xs); color: var(--muted-strong); }
.prompt-error,
.prompt-notice,
.prompt-var-warn {
  margin: 0;
  font-size: var(--fs-sm);
  line-height: var(--lh-base);
}
.prompt-error { padding: var(--sp-2) var(--sp-3); border-radius: var(--radius-s); background: var(--orange-bg); color: var(--orange); }
.prompt-notice {
  display: flex;
  align-items: flex-start;
  gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3);
  border-radius: var(--radius-s);
  background: var(--row);
  color: var(--muted-strong);
}
.prompt-notice > span { flex: 1; min-width: 0; }
.prompt-notice-close { padding: 4px; flex-shrink: 0; }
.prompt-notice-close > svg { width: 12px; height: 12px; display: block; }
.prompt-var-report { display: flex; flex-direction: column; gap: 4px; padding: var(--sp-2) var(--sp-3); border-radius: var(--radius-s); background: var(--orange-bg); }
.prompt-var-warn { color: var(--orange); font-size: var(--fs-xs); }

.prompt-empty {
  flex: 1;
  min-height: 220px;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--sp-2);
  text-align: center;
  color: var(--muted-strong);
  padding: var(--sp-4);
}
.prompt-empty > svg { width: 30px; height: 30px; color: var(--green); opacity: .8; }
.prompt-empty strong { font-size: var(--fs-base); color: var(--text); }
.prompt-empty p { margin: 0; max-width: 360px; font-size: var(--fs-xs); line-height: var(--lh-loose); }
.prompt-empty code { font-family: ui-monospace, monospace; font-size: var(--fs-xs); background: var(--row); padding: 1px 5px; border-radius: 4px; }

.prompt-result-actions { display: flex; flex-wrap: wrap; align-items: center; gap: var(--sp-2); }
/* 流式过程中用户上翻查看前文时，给一条回到生成末尾的去路 */
.prompt-jump { border-color: color-mix(in srgb, var(--green) 35%, var(--line)); color: var(--green); }
.prompt-jump:hover:not(:disabled) { color: var(--green-hover); }
.prompt-meta { margin: 0; font-size: var(--fs-xs); color: var(--muted-strong); font-variant-numeric: tabular-nums; }
.prompt-privacy { margin: auto 0 0; padding-top: var(--sp-2); border-top: 1px solid var(--line); font-size: var(--fs-xs); line-height: var(--lh-loose); color: var(--muted-strong); }

/* ── 对比测试（折叠区） ───────────────────────────────────────────
   用原生 details：折叠语义、键盘与读屏行为都由浏览器负责，不用自造开关状态。 */
.prompt-compare {
  flex-shrink: 0;
  border: 1px solid var(--line);
  border-radius: var(--radius-l);
  background: var(--surface-raised);
  box-shadow: var(--shadow-s);
}
.prompt-compare-summary {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-3) var(--sp-4);
  cursor: pointer;
  font-size: var(--fs-sm);
  font-weight: 600;
  border-radius: var(--radius-l);
}
.prompt-compare-summary > em {
  flex: 1;
  min-width: 0;
  font-style: normal;
  font-weight: 400;
  font-size: var(--fs-xs);
  color: var(--muted-strong);
  line-height: var(--lh-loose);
}
.prompt-compare-summary:hover { color: var(--green); }
.prompt-compare-summary:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.prompt-compare-chevron {
  width: 14px;
  height: 14px;
  flex-shrink: 0;
  color: var(--muted-strong);
  transition: transform var(--dur-2) var(--ease-standard);
}
.prompt-compare[open] .prompt-compare-chevron { transform: rotate(180deg); }
/* 上下排：窄窗口下并排会把两份输出各挤到 ~150px，读不出差别 */
.prompt-compare-body {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
  padding: 0 var(--sp-4) var(--sp-4);
}
.prompt-compare-input { display: flex; flex-direction: column; gap: var(--sp-2); min-width: 0; }
.prompt-compare-input .prompt-requirement { min-height: 96px; max-height: 170px; }
.prompt-compare-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 120px;
  padding: var(--sp-4);
  border: 1px dashed var(--line);
  border-radius: var(--radius-m);
  color: var(--muted-strong);
  text-align: center;
}
.prompt-compare-empty p { margin: 0; max-width: 420px; font-size: var(--fs-xs); line-height: var(--lh-loose); }
.prompt-compare-out { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: var(--sp-3); min-width: 0; }
.prompt-compare-col { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
.prompt-compare-col-head { display: flex; align-items: center; justify-content: space-between; gap: var(--sp-2); }
.prompt-compare-col h4 { margin: 0; font-size: var(--fs-sm); font-weight: 600; }
.prompt-copy { padding: 2px 8px; font-size: var(--fs-xs); }
/* 模型输出按原文呈现：不渲染 Markdown，两边才真的是「同一份东西看差别」 */
.prompt-compare-text {
  margin: 0;
  flex: 1;
  min-height: 160px;
  max-height: 260px;
  overflow-y: auto;
  padding: 10px 12px;
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
  background: var(--bg);
  font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
  font-size: var(--fs-xs);
  line-height: var(--lh-loose);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
</style>
