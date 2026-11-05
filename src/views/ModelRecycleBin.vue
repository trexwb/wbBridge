<script setup>
// 模型回收站：集中列出被用户手动移除的模型，唯一动作是恢复。
// 移除不等于删除：目录条目还在，恢复后由核心立即发起一次单模型探测，通过后才重新发布。
// 这里是唯一恢复入口，所以列表页只留「移除」，不再把已移除项混在排序列表底部。
import ModelRow from '../components/ModelRow.vue'

const props = defineProps({
  models: { type: Array, default: () => [] },
  results: { type: Object, default: () => ({}) },
  // 正在恢复的模型 id（核心确认前后都由 App.vue 的 busyAction 标记），用于行内 spinner。
  restoring: { type: String, default: '' },
})
defineEmits(['restore'])

// 无结果的行共享同一个空对象：引用稳定，行组件不被无关状态推送触发更新。
const EMPTY_RESULT = {}
const resultOf = (id) => props.results[id] || EMPTY_RESULT
</script>

<template>
  <section class="view recycle-bin" aria-label="模型回收站">
    <!-- 没有单独的「返回」按钮：顶部工具条的切换按钮在打开后会变成「模型列表」，
         Esc 同样可退回，两个入口指向同一个 closeBin。 -->
    <header>
      <div>
        <h2>模型回收站</h2>
        <p class="subtitle">被移除的模型不再检测、不再发布。恢复后立即重新检测，通过后才会重新发布；点顶部「模型列表」或按 Esc 返回。</p>
      </div>
    </header>

    <div v-if="!models.length" class="empty" role="status">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M4 7h16M9 7V4.6h6V7M6.4 7l.9 12.4h9.4L17.6 7" />
        <path d="M10 10.6v5M14 10.6v5" />
      </svg>
      <p><strong>回收站是空的</strong>在模型列表里把经常「不可用 / 访问受限」的模型点「移除」后，会出现在这里。</p>
    </div>

    <div v-else class="bin-list">
      <p class="count" role="status">共 {{ models.length }} 个已移除的模型</p>
      <!-- 逐行错峰入场：与主列表同一套规则，序号封顶 8 级 -->
      <ModelRow
        v-for="(model, i) in models"
        :key="model.id"
        :style="{ '--i': Math.min(i, 8) }"
        :model="model"
        :result="resultOf(model.id)"
        :restoring="restoring === model.id"
        :removed="true"
        @restore="$emit('restore', $event)"
      />
    </div>
  </section>
</template>

<style scoped>
header { display: flex; justify-content: space-between; gap: var(--sp-3); align-items: center; }
.subtitle { margin: 0; color: var(--muted-strong); font-size: var(--fs-sm); }
.bin-list { display: flex; flex-direction: column; gap: 8px; }
.count { margin: 0; font-size: var(--fs-xs); color: var(--muted-strong); }
/* 空态：与主列表同一套虚线框语言，说明「这里将来会有什么」而不是冷冰冰的空白 */
.empty {
  margin: auto;
  display: flex;
  align-items: flex-start;
  gap: 10px;
  max-width: 460px;
  padding: 16px 18px;
  border: 1px dashed var(--line);
  border-radius: var(--radius-m);
  color: var(--muted-strong);
  font-size: var(--fs-sm);
  line-height: 1.7;
}
.empty svg { width: 22px; height: 22px; flex-shrink: 0; color: var(--muted-strong); }
.empty p { margin: 0; }
.empty strong { display: block; color: var(--text); font-weight: 600; margin-bottom: 2px; }
</style>
