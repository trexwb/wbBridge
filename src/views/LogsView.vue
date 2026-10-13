<script setup>
// 运行日志：只读展示数据目录下核心日志的尾部。
// 面板不读文件也不拼路径：壳侧 read_log 自己定位数据目录下的日志文件，只回尾部内容。
import { computed, onMounted, ref } from 'vue'
import { readLog } from '../core/bridge.js'

const log = ref(null) // { text, truncated, bytes }
const error = ref(null)
const loading = ref(false)
const expanded = ref(false)

// 壳侧 read_log 的尾部上限是 256KB / 1200 行（见 docs/contract.md）。把这整块塞进一个
// `<pre>`（pre-wrap + 逐字符断行）是一次性上千行的重排，切到本视图会明显顿一下。
// 日志的价值在末尾，默认只把最后这些行进 DOM，要看全文再点「显示全部」。
const PREVIEW_LINES = 400

const lines = computed(() => String(log.value?.text ?? '').split('\n'))
// 行数不到上限时不做任何裁剪：既不显示按钮，也不改变呈现。
const hiddenCount = computed(() => (expanded.value ? 0 : Math.max(0, lines.value.length - PREVIEW_LINES)))
const shownText = computed(() =>
  hiddenCount.value ? lines.value.slice(-PREVIEW_LINES).join('\n') : String(log.value?.text ?? ''),
)

// 只做单位换算，不改变原始数值口径（bytes 是日志文件总字节数）。
function formatBytes(bytes) {
  const value = Number(bytes) || 0
  if (value < 1024) return `${value} B`
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${(value / 1024 / 1024).toFixed(2)} MB`
}

async function load() {
  // 同步防抖：双击「刷新」在重渲染前会穿透模板 disabled，叠发两次 read_log 请求。
  if (loading.value) return
  loading.value = true
  error.value = null
  const response = await readLog()
  if (response.ok) log.value = response.result || {}
  else error.value = response.error
  loading.value = false
}

onMounted(load)
</script>

<template>
  <section class="view" aria-label="运行日志">
    <header>
      <div>
        <h2>运行日志</h2>
        <p class="subtitle">核心服务的运行记录，只读展示。</p>
      </div>
      <button id="reload-log" :disabled="loading" @click="load">
        <span v-if="loading" class="spinner" />刷新
      </button>
    </header>

    <div v-if="error" class="notice error" role="alert">
      <svg class="notice-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <span class="notice-text">读取运行日志失败：{{ error }}</span>
      <button type="button" class="ghost" :disabled="loading" @click="load">
        <span v-if="loading" class="spinner" />重试
      </button>
    </div>
    <template v-else>
      <!-- 首次读取：先用骨架占位，形状与真实日志块一致，避免「白屏 → 突然压进一大块文字」 -->
      <div v-if="loading && !log" class="log-skeleton" aria-busy="true">
        <p class="visually-hidden">正在读取运行日志…</p>
        <span v-for="n in 6" :key="n" class="skeleton" />
      </div>
      <template v-else>
        <p v-if="log?.text" class="meta">
          <span>
            日志文件共 {{ formatBytes(log.bytes) }} / {{ lines.length }} 行，{{ log.truncated ? '内容超出上限，仅显示末尾一段' : '已显示全部内容' }}<template v-if="hiddenCount">；为避免一次铺进上千行拖慢界面，此处只渲染末尾 {{ PREVIEW_LINES }} 行</template>。
          </span>
          <button v-if="hiddenCount" type="button" class="ghost" @click="expanded = true">
            显示全部 {{ lines.length }} 行
          </button>
        </p>
        <pre v-if="log?.text" class="log">{{ shownText }}</pre>
        <p v-else-if="log" class="notice">
          日志为空：核心尚未写入任何内容（日志文件由核心启动时创建）。
        </p>
      </template>
    </template>

    <p class="note">日志文件与 status.json 同在应用数据目录；面板不会修改日志，也不跟随任意路径。</p>
  </section>
</template>

<style scoped>
header { display: flex; justify-content: space-between; gap: var(--sp-3); align-items: center; }
.subtitle { margin: 0; color: var(--muted-strong); font-size: var(--fs-sm); }
.meta {
  margin: 0;
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--fs-xs);
  color: var(--muted-strong);
}
.meta > span { min-width: 0; flex: 1; }
.meta .ghost { flex-shrink: 0; font-size: var(--fs-xs); padding: 3px 8px; }
.log {
  margin: 0;
  padding: var(--sp-3);
  background: var(--row);
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
  font-family: ui-monospace, monospace;
  font-size: var(--fs-xs);
  line-height: var(--lh-base);
  color: var(--text);
  white-space: pre-wrap;
  /* `anywhere` 会参与最小内容尺寸（min-content）计算，在可能上千行的文本块上是一条明显更贵的
     路径；`break-word` 只在实际换行时才生效——两者在定宽容器里的观感一致，这里取后者。 */
  overflow-wrap: break-word;
  tab-size: 2;
  user-select: text;
}
/* 首次读取的骨架：行宽错落，形似日志正文，而不是一排整齐色块 */
.log-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: var(--sp-3);
  background: var(--row);
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
}
.log-skeleton .skeleton { height: 11px; }
.log-skeleton .skeleton:nth-of-type(1) { width: 58%; }
.log-skeleton .skeleton:nth-of-type(2) { width: 86%; }
.log-skeleton .skeleton:nth-of-type(3) { width: 72%; }
.log-skeleton .skeleton:nth-of-type(4) { width: 92%; }
.log-skeleton .skeleton:nth-of-type(5) { width: 64%; }
.log-skeleton .skeleton:nth-of-type(6) { width: 78%; }
.notice { margin: 0; font-size: var(--fs-sm); color: var(--muted-strong); }
/* 读取失败：橙色告警条 + 就地重试（读日志是只读动作，重试无副作用） */
.notice.error {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px;
  color: var(--orange);
  background: var(--orange-bg);
  border-radius: var(--radius-m);
  border-left: 3px solid currentColor;
}
.notice-icon { width: 15px; height: 15px; flex-shrink: 0; }
.notice-text { flex: 1; min-width: 0; overflow-wrap: anywhere; }
.notice .ghost { flex-shrink: 0; color: inherit; }
.notice .ghost:hover:not(:disabled) { background: color-mix(in srgb, currentColor 14%, transparent); border-color: transparent; color: inherit; }
.note { margin: 0; font-size: var(--fs-xs); color: var(--muted-strong); line-height: var(--lh-loose); }
</style>
