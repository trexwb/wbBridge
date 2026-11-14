<script setup>
// 单个模型行：状态徽章（推理/图片/状态）与最近耗时。移除后的行只保留「恢复」入口。
import { computed, onBeforeUnmount, ref } from 'vue'

const props = defineProps({
  model: { type: Object, required: true },
  result: { type: Object, default: () => ({}) },
  waiting: { type: Boolean, default: false },
  probing: { type: Boolean, default: false },
  // 这一行的「重新检测」正在进行：核心上报的在飞模型与面板点击后的乐观标记取并集（见 App.vue）。
  reprobing: { type: Boolean, default: false },
  inRequest: { type: Boolean, default: false },
  available: { type: Boolean, default: false },
  selected: { type: Boolean, default: false },
  // 已被用户手动移除：不再检测/发布，只显示状态与「恢复」。
  removed: { type: Boolean, default: false },
  // 这一行的「恢复」请求正在进行：按钮留在原位转 spinner，避免点了没反应。
  restoring: { type: Boolean, default: false },
})
const emit = defineEmits(['toggle', 'reprobe', 'remove', 'restore'])

const LABELS = { timeout: '检测超时', quota: '额度不足', rate_limit: '请求受限', access: '访问受限' }

// 「移除」是破坏性操作（该模型此后不参与检测与发布），沿用运行日志清空的两次点击确认：
// 首次点击只进入待确认态（3 秒后自动撤回），第二次点击才真正提交；
// 400ms 的臂延迟挡住双击穿透，避免一次连击就把按钮点成了确认。
const CONFIRM_ARM_MS = 400
const removeArmed = ref(false)
const removeArmedAt = ref(0)
let removeTimer = 0

function onRemove() {
  if (props.removed) return
  if (!removeArmed.value) {
    removeArmed.value = true
    removeArmedAt.value = Date.now()
    removeTimer = setTimeout(() => { removeArmed.value = false }, 3000)
    return
  }
  if (Date.now() - removeArmedAt.value < CONFIRM_ARM_MS) return
  clearTimeout(removeTimer)
  removeTimer = 0
  removeArmed.value = false
  emit('remove', props.model.id)
}

onBeforeUnmount(() => clearTimeout(removeTimer))

// 行内「还没结束」的统一判据：在批量队列里排队/正在检，或这一行的重新检测在飞。
const busy = computed(() => !props.removed && (props.waiting || props.reprobing))

const statusLabel = computed(() => {
  if (props.removed) return '已移除'
  if (props.reprobing) return '检测中'
  if (props.waiting) return props.probing ? '检测中' : '等待检测'
  if (props.inRequest) return '请求中'
  if (props.available) return props.result?.chatOnly ? '可用 · 仅对话' : '可用'
  return LABELS[props.result?.category] || (props.result?.ok === false ? '不可用' : '待检测')
})
const unavailable = computed(() => !props.removed && !busy.value && !props.inRequest && !props.available && (props.result?.ok === false || !props.available))
// 不可用模型显示「重新检测」入口；点击后按钮留在原位转为「检测中」+ spinner，而不是消失或
// 毫无变化 —— 检测要跑几十秒，入口必须自己交代状态。
// 批量探测进行中**不**禁用单模型重新检测：核心刻意让两条路径互不互斥（见 start_probes），
// 所以这里不看 probe.running。
const canReprobe = computed(() => !props.removed && (unavailable.value || props.reprobing))
// 「移除」只出现在明确不可用的模型上：这是用户裁掉长期失败模型的入口，不是日常清理可用模型的捷径。
const canRemove = computed(() => !props.removed && unavailable.value)
const removeLabel = computed(() => (removeArmed.value ? '确认移除' : '移除'))

const timing = computed(() => {
  const r = props.result
  if (!Number.isFinite(r?.durationMs)) return '响应耗时 —'
  const duration = r.durationMs < 1000 ? `${r.durationMs} ms` : `${(r.durationMs / 1000).toFixed(1)} 秒`
  return `最近${r.source === 'probe' ? '检测' : '调用'} · ${r.ok ? '响应' : '失败'}耗时 ${duration}`
})

// 速度权重来自 modelResults：探测通过时按耗时定权、慢请求扣权。没有数值时不显示，
// 避免把「尚未探测」错说成满分（选择器内部才按 100 兜底）。
const weight = computed(() => {
  const value = props.result?.weight
  return Number.isFinite(value) ? value : null
})
</script>

<template>
  <div
    class="model"
    :class="{ selected, removed }"
    :role="removed ? null : 'option'"
    :aria-selected="removed ? null : String(selected)"
    :tabindex="removed ? -1 : 0"
    @click="!removed && $emit('toggle')"
    @keydown.enter="!removed && $emit('toggle')"
    @keydown.space.prevent="!removed && $emit('toggle')"
  >
    <span class="model-icon" aria-hidden="true">
      <span v-if="busy" class="spinner" />
      <svg
        v-else
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linecap="round"
        stroke-linejoin="round"
      >
        <path d="M12 3.4l7.4 4.3v8.6L12 20.6 4.6 16.3V7.7Z" />
        <path d="M12 12l7.4-4.3M12 12v8.6M12 12L4.6 7.7" />
      </svg>
    </span>
    <span class="model-info">
      <span class="model-name">{{ model.id }}</span>
      <span class="duration">{{ timing }}</span>
    </span>
    <span class="badges">
      <span v-if="model.reasoning" class="badge reasoning">推理</span>
      <span v-if="model.images" class="badge images">图片</span>
      <span
        v-if="model.paid"
        class="badge paid"
        title="付费白名单放行：探测与对话按平台标价真实扣费，不进入 WB · auto"
      >付费</span>
      <span
        v-if="weight !== null"
        class="badge weight"
        :title="`速度权重 ${weight}：WB · auto 优先选择权重最高的可用模型`"
      >权重 {{ weight }}</span>
      <span class="badge" :class="{ unavailable: unavailable && !available, waiting: busy || inRequest, removed }">{{ statusLabel }}</span>
      <button
        v-if="canReprobe"
        class="reprobe-btn"
        :class="{ 'is-busy': reprobing }"
        :disabled="busy"
        :aria-busy="String(reprobing)"
        :title="reprobing ? '此模型正在重新检测' : '重新检测此模型'"
        @click.stop="$emit('reprobe', model.id)"
      ><span v-if="reprobing" class="spinner sm" />{{ reprobing ? '检测中' : '重新检测' }}</button>
      <button
        v-if="canRemove"
        class="remove-btn"
        :class="{ 'is-armed': removeArmed }"
        :title="removeArmed ? '再次点击确认移除：此后不再检测、不再发布' : '移除这个模型：不再检测、不再发布，可随时恢复'"
        @click.stop="onRemove"
      >{{ removeLabel }}</button>
      <button
        v-if="removed"
        class="restore-btn"
        :class="{ 'is-busy': restoring }"
        :disabled="restoring"
        :aria-busy="String(restoring)"
        :title="restoring ? '正在恢复并重新检测' : '恢复这个模型：立即重新参与检测'"
        @click.stop="$emit('restore', model.id)"
      ><span v-if="restoring" class="spinner sm" />{{ restoring ? '恢复中' : '恢复' }}</button>
    </span>
  </div>
</template>

<style scoped>
.model {
  position: relative;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 13px 14px;
  border: 1px solid transparent;
  background: var(--row);
  border-radius: var(--radius-m);
  text-align: left;
  min-height: 69px;
  width: 100%;
  white-space: normal;
  cursor: pointer;
  /* 入场错峰由列表侧给 --i（见 ModelList.vue）；fill-mode 用 backwards，
     动画结束后不保留 transform，避免压掉 :active 与选中态。 */
  animation: enter-rise var(--dur-3) var(--ease-enter) backwards;
  animation-delay: calc(var(--i, 0) * var(--stagger-step));
  transition: background var(--dur-1) var(--ease-standard),
              border-color var(--dur-1) var(--ease-standard),
              box-shadow var(--dur-2) var(--ease-standard);
}
/* 选中指示轨：左缘主色竖条，与侧栏当前项共用同一套视觉语言 */
.model::before {
  content: "";
  position: absolute;
  left: -1px;
  top: 50%;
  width: 3px;
  height: 0;
  border-radius: 0 3px 3px 0;
  background: var(--green);
  opacity: 0;
  transform: translateY(-50%);
  transition: height var(--dur-2) var(--ease-emphasis), opacity var(--dur-2) var(--ease-standard);
}
.model:hover:not(.selected):not(.removed) {
  background: color-mix(in srgb, var(--row) 84%, var(--green) 16%);
  border-color: color-mix(in srgb, var(--green) 22%, transparent);
}
/* 已移除的行不可选中：弱化底色并去掉手型，但仍保持正文可读（它仍是可恢复的对象） */
.model.removed {
  cursor: default;
  background: var(--panel);
  border-color: var(--line);
  box-shadow: none;
}
.model.selected {
  background: var(--green-bg);
  border-color: color-mix(in srgb, var(--green) 55%, transparent);
  box-shadow: var(--shadow-s);
}
.model.selected::before { height: 26px; opacity: 1; }
.model:focus-visible { outline: 2px solid var(--green); outline-offset: -2px; }
.model-icon { display: flex; align-items: center; justify-content: center; width: 26px; flex-shrink: 0; color: var(--green); }
.model-icon svg { width: 22px; height: 22px; }
.model-icon .spinner { margin-right: 0; }
.model-info { min-width: 0; flex: 1; display: block; }
.model-name { display: block; font-size: 14px; font-weight: 550; line-height: 1.4; overflow-wrap: anywhere; }
.duration { display: block; font-size: 11px; color: var(--muted-strong); margin-top: 6px; font-variant-numeric: tabular-nums; }
.badges { display: flex; align-items: center; gap: 5px; flex-shrink: 0; flex-wrap: wrap; justify-content: flex-end; }
/* 徽章统一 pill：状态语义只靠文字与配色区分，不改形状，扫读更快。
   底色与字色也进过渡：状态推进（等待检测 → 检测中 → 可用）若硬切，整行会闪一下。 */
.badge {
  font-size: 11px;
  font-weight: 500;
  white-space: nowrap;
  padding: 4px 9px;
  border-radius: 999px;
  background: var(--green-bg);
  color: var(--green);
  transition: background-color var(--dur-2) var(--ease-standard),
              color var(--dur-2) var(--ease-standard);
}
.reasoning { color: var(--badge-reason-fg); background: var(--badge-reason-bg); }
.images { color: var(--badge-image-fg); background: var(--badge-image-bg); }
/* 付费放行 = 真实账单：与「写入」的橙棕、可用的绿色都拉开色相，扫读时一眼能挑出来 */
.paid { color: var(--danger); background: var(--danger-bg); }
/* 权重是排序依据而非能力，用中性底；tabular-nums 让 100 / 80 这种位数变化时徽章不抖动 */
.weight {
  color: var(--muted-strong);
  background: var(--panel);
  font-variant-numeric: tabular-nums;
}
.unavailable { color: var(--badge-unavailable-fg); background: var(--orange-bg); }
/* 进行中的徽章轻微呼吸，与行内 spinner 一起表达「尚未结束」；减弱动效时由全局规则压成静态 */
.waiting { color: var(--muted-strong); background: var(--panel); animation: badge-pulse 1.6s ease-in-out infinite; }
@keyframes badge-pulse { 0%, 100% { opacity: 1; } 50% { opacity: .62; } }
/* 重新检测按钮：仅在不可用模型上出现，pill 形态与徽章一致但可点击 */
.reprobe-btn {
  display: inline-flex;
  align-items: center;
  font-size: 11px;
  font-weight: 500;
  white-space: nowrap;
  padding: 4px 9px;
  border-radius: 999px;
  border: none;
  background: var(--orange-bg);
  color: var(--badge-unavailable-fg);
  cursor: pointer;
  transition: opacity var(--dur-1) var(--ease-standard);
}
.reprobe-btn:hover:not(:disabled) { opacity: 0.75; }
.reprobe-btn:disabled { cursor: not-allowed; opacity: 0.4; }
.reprobe-btn:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
/* 检测中的按钮保持全不透明：disabled 的 .4 透明度说的是「点不动」，
   用在这里会把「正在进行」读成「坏了」。写在 :disabled 之后，同特异度按顺序覆盖。 */
.reprobe-btn.is-busy { opacity: 1; cursor: progress; }
.reprobe-btn .spinner.sm { color: var(--badge-unavailable-fg); }
/* 已移除：中性徽章，与「不可用」的橙色区分开——它不是当前失败，而是被用户主动停用 */
.badge.removed { color: var(--muted-strong); background: var(--panel); }
/* 移除按钮：只在明确不可用的行上出现；第一次点击进入「确认移除」，用 danger 色明确后果 */
.remove-btn {
  display: inline-flex;
  align-items: center;
  font-size: 11px;
  font-weight: 500;
  white-space: nowrap;
  padding: 4px 9px;
  border-radius: 999px;
  border: none;
  background: var(--panel);
  color: var(--muted-strong);
  cursor: pointer;
  transition: color var(--dur-1) var(--ease-standard), background var(--dur-1) var(--ease-standard);
}
.remove-btn:hover { color: var(--danger); }
.remove-btn.is-armed { color: var(--danger); background: var(--danger-bg); }
.remove-btn:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
/* 恢复按钮：安全的逆向操作，用主色说清「可以回来」 */
.restore-btn {
  display: inline-flex;
  align-items: center;
  font-size: 11px;
  font-weight: 500;
  white-space: nowrap;
  padding: 4px 9px;
  border-radius: 999px;
  border: none;
  background: var(--green-bg);
  color: var(--green);
  cursor: pointer;
  transition: opacity var(--dur-1) var(--ease-standard);
}
.restore-btn:hover:not(:disabled) { opacity: 0.75; }
.restore-btn:disabled { cursor: not-allowed; opacity: 0.4; }
/* 恢复中的按钮保持全不透明：.4 说的是「点不动」，不是「正在进行」 */
.restore-btn.is-busy { opacity: 1; cursor: progress; }
.restore-btn .spinner.sm { color: var(--green); }
.restore-btn:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
</style>
