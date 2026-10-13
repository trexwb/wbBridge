<script setup>
// 受控 Markdown 渲染块（AI 助手的气泡正文）。
//
// 为什么单独成组件，而不是在父模板里写 `v-html`：
// 一轮流式生成每 ~80ms 追加一次增量，父视图整体重渲染一次；`v-html` 写在父模板里时，
// **每一条**历史气泡都会被重新赋值 `innerHTML` —— Vue 对 `innerHTML` 不做新旧比对
// （runtime-dom 的 patchDOMProp 直接赋值），于是整段对话每 80ms 被浏览器重新解析一遍：
// 布局重算、文本选中被冲掉、滚动位置抖动。
// 收进子组件后，content 没变的消息在 props 判等处就被跳过，只有**正在生成的那一条**
// 重新解析；其余气泡连 DOM 都不碰。
import { computed } from 'vue'
import { renderMarkdownLite } from '../core/ai.js'

const props = defineProps({
  // 原始文本（先整段转义再解析，原始 HTML 不可能成为标签）。
  content: { type: String, default: '' },
})

const html = computed(() => renderMarkdownLite(props.content))
</script>

<template>
  <div class="markdown-block" v-html="html" />
</template>

<style scoped>
/* 这四条原本挂在父组件的 `.assistant-text :deep(...)` 上：v-html 生成的内容拿不到
   父级的 scope id，靠 `:deep()` 把作用域挪到外层容器上才命中。挪进本组件后
   仍是同样的写法，但不必再依赖「子组件根节点继承父级 scope id」这一条隐式约定。 */
.markdown-block :deep(p) { margin: 0 0 6px; }
.markdown-block :deep(p:last-child) { margin-bottom: 0; }
.markdown-block :deep(code) {
  font-family: ui-monospace, monospace;
  font-size: var(--fs-xs);
  background: var(--panel);
  padding: 1px 5px;
  border-radius: 4px;
}
.markdown-block :deep(pre.ai-code) {
  margin: 6px 0;
  padding: 8px 10px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius-s);
  overflow-x: auto;
}
.markdown-block :deep(pre.ai-code code) { background: none; padding: 0; }
</style>
