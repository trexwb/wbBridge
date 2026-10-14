// src/core/ai.js 的单测：node --test，无 DOM、无 Tauri、不联网。
// 覆盖收口门槛要求的三类行为：Markdown 渲染的 XSS 防护、工具调用校验与掩码、
// 快照/提示词/请求组装对真实状态的如实转述。
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

import {
  AI_PRESETS,
  AI_TOOLS,
  AUTO_MODEL_ID,
  DEFAULT_BASE_URL,
  DEFAULT_MODEL,
  DEFAULT_SELF_MODEL,
  PROVIDER_IDS,
  PROVIDER_PAGES,
  REQUEST_MESSAGE_LIMIT,
  TOOL_META,
  buildRequestMessages,
  buildSnapshot,
  buildSystemPrompt,
  currentConfig,
  maskKey,
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
} from './ai.js'

// ── 受控 Markdown 渲染 ──────────────────────────────────────────────

test('renderMarkdownLite：原始 HTML 一律转义，不会成为标签', () => {
  const html = renderMarkdownLite('<script>alert(1)</script><img src=x onerror=alert(1)>')
  assert.equal(html.includes('<script'), false)
  assert.equal(html.includes('<img'), false)
  assert.ok(html.includes('&lt;script&gt;'))
  assert.ok(html.includes('&lt;img'))
})

test('renderMarkdownLite：围栏代码块整体转义，行内代码与加粗生效', () => {
  const html = renderMarkdownLite('看这段：\n```js\nconst a = 1 < 2\n```\n还有 `x < y` 与 **重点**')
  assert.ok(html.includes('<pre class="ai-code"><code>const a = 1 &lt; 2</code></pre>'))
  assert.ok(html.includes('<code>x &lt; y</code>'))
  assert.ok(html.includes('<strong>重点</strong>'))
  // 代码块里的 ** 不当成加粗（代码段不走行内替换）。
  const code = renderMarkdownLite('```\n**not bold**\n```')
  assert.equal(code.includes('<strong>'), false)
  assert.ok(code.includes('**not bold**'))
})

test('renderMarkdownLite：列表/标题行降级为文本行，空输入返回空串', () => {
  assert.ok(renderMarkdownLite('- 第一项\n- 第二项').includes('• 第一项'))
  assert.ok(renderMarkdownLite('### 小标题').includes('小标题'))
  assert.equal(renderMarkdownLite(''), '')
  assert.equal(renderMarkdownLite(null), '')
  assert.equal(renderMarkdownLite(undefined), '')
})

test('renderMarkdownLite：未闭合的围栏（流式中）也按代码块渲染，不吐原始反引号', () => {
  const html = renderMarkdownLite('前半段\n```py\nprint(1')
  assert.ok(html.includes('<pre class="ai-code">'))
  assert.equal(html.includes('```'), false)
})

// ── Key 掩码 ────────────────────────────────────────────────────────

test('maskKey：只露前后各 4 位，短 Key 全掩码', () => {
  assert.equal(maskKey('sk-abcdefghijklmnop'), 'sk-a…mnop（19 字符）')
  assert.equal(maskKey('short'), '*****')
  assert.equal(maskKey(''), '(空)')
  assert.equal(maskKey(null), '(空)')
})

// ── 工具调用校验 ─────────────────────────────────────────────────────

test('validateToolCall：未知工具、缺参、非法平台/模型一一拦截', () => {
  assert.equal(validateToolCall('nope', {}).ok, false)

  assert.equal(validateToolCall('set_provider_key', { provider: 'zhipuai' }).ok, false)
  assert.equal(validateToolCall('set_provider_key', { provider: 'zhipuai' }).error.includes('apiKey'), true)
  assert.equal(validateToolCall('set_provider_key', { provider: 'unknown', apiKey: 'k' }).ok, false)
  assert.equal(validateToolCall('set_provider_key', { provider: 'zhipuai', apiKey: 'k' }).ok, true)
  assert.equal(validateToolCall('set_provider_key', { provider: 'zhipuai', apiKey: 'a\nb' }).ok, false)
  assert.equal(validateToolCall('set_provider_key', { provider: 'zhipuai', apiKey: 'x'.repeat(1025) }).ok, false)

  assert.equal(validateToolCall('clear_provider_key', { provider: 'modelscope' }).ok, true)
  assert.equal(validateToolCall('clear_provider_key', {}).ok, false)

  assert.equal(validateToolCall('set_system_proxy', { enabled: 'yes' }).ok, false)
  assert.equal(validateToolCall('set_system_proxy', { enabled: false }).ok, true)

  assert.equal(validateToolCall('probe_models', {}).ok, true)
  assert.equal(validateToolCall('probe_models', { model: 'opencode/big-pickle' }).ok, true)
  assert.equal(validateToolCall('probe_models', { model: 'ghost/model' }, { modelIds: ['opencode/big-pickle'] }).ok, false)

  assert.equal(validateToolCall('list_models', { filter: 'nope' }).ok, false)
  assert.equal(validateToolCall('list_models', { filter: 'available' }).ok, true)
  assert.equal(validateToolCall('restart_core', {}).ok, true)

  // open_provider_page 只认注册表里的四家：填别的域名不该被带到壳的 open_external。
  assert.equal(validateToolCall('open_provider_page', {}).ok, false)
  assert.equal(validateToolCall('open_provider_page', { provider: 'zhipuai' }).ok, true)
  assert.equal(validateToolCall('open_provider_page', { provider: 'evil.example.com' }).ok, false)

  // 新增的三条写入提案无参数，缺参校验用不上，但仍必须存在且被登记为写入。
  for (const name of ['shutdown_core', 'download_update', 'apply_update']) {
    assert.equal(validateToolCall(name, {}).ok, true)
  }
})

test('工具表：写工具全部登记在 TOOL_META，schema 名称唯一且与元数据一致', () => {
  const names = AI_TOOLS.map(t => t.function.name)
  assert.equal(new Set(names).size, names.length)
  for (const name of names) assert.ok(TOOL_META[name], `TOOL_META 缺少 ${name}`)
  assert.equal(names.length, 18)
  const writeNames = names.filter(n => TOOL_META[n].write)
  assert.deepEqual(writeNames.sort(), [
    'apply_update',
    'clear_provider_key',
    'download_update',
    'import_models',
    'open_provider_page',
    'probe_models',
    'refresh_models',
    'restart_core',
    'set_provider_key',
    'set_system_proxy',
    'shutdown_core',
  ].sort())
  // danger 是「会停服务 / 会退出重启」那一档，界面用红标 + 确认框里的额外提醒兜住。
  for (const name of ['restart_core', 'shutdown_core', 'download_update', 'apply_update']) {
    assert.equal(TOOL_META[name].danger, true, `${name} 应标为高危`)
  }
  assert.equal(TOOL_META.get_status.write, false)
  assert.equal(TOOL_META.check_update.write, false)
  assert.equal(TOOL_META.get_environment.write, false)
  assert.equal(TOOL_META.open_provider_page.danger, false)
  // set_provider_key 的 schema 必须把 apiKey 列为必填（模型才知道非给不可）。
  const def = AI_TOOLS.find(t => t.function.name === 'set_provider_key')
  assert.deepEqual(def.function.parameters.required, ['provider', 'apiKey'])
  assert.deepEqual(def.function.parameters.properties.provider.enum, PROVIDER_IDS)
})

test('PROVIDER_PAGES：主机名必须逐条落在壳的 open_external 白名单里', () => {
  // 双保险对账：ai.js 里的申请页地址与壳侧 EXTERNAL_HOSTS 分属两种语言，
  // 任何一边改了都必须被抓到——否则工具会「成功执行」后被壳拒绝，或直接开出名单外域名。
  assert.deepEqual(Object.keys(PROVIDER_PAGES).sort(), [...PROVIDER_IDS].sort())
  const rust = readFileSync(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8')
  const match = rust.match(/const EXTERNAL_HOSTS[^\n]*\n([\s\S]*?)\n\];/)
  assert.ok(match, '未能从壳源码读到 EXTERNAL_HOSTS 名单（写法变了要同步改这条对账）')
  const hosts = (match[1].match(/"([^"]+)"/g) ?? []).map(text => text.slice(1, -1))
  assert.equal(hosts.length, PROVIDER_IDS.length, 'EXTERNAL_HOSTS 条数与平台注册表不一致')
  assert.equal(hosts.length, PROVIDER_IDS.length)
  for (const [id, url] of Object.entries(PROVIDER_PAGES)) {
    assert.ok(url.startsWith('https://'), `${id} 必须是 https`)
    assert.ok(hosts.includes(new URL(url).hostname), `${id} 的主机不在 EXTERNAL_HOSTS：${new URL(url).hostname}`)
  }
})

// ── 摘要与预览 ──────────────────────────────────────────────────────

test('summarizeToolCall / toolCallPreview：Key 只以掩码形态出现', () => {
  const summary = summarizeToolCall('set_provider_key', { provider: 'zhipuai', apiKey: 'glm-1234567890abcd' })
  assert.ok(summary.includes('智谱'))
  assert.equal(summary.includes('1234567890abcd'), false)
  const preview = toolCallPreview('set_provider_key', { provider: 'zhipuai', apiKey: 'glm-1234567890abcd' })
  assert.equal(preview.Key, maskKey('glm-1234567890abcd'))
  assert.equal(JSON.stringify(preview).includes('1234567890abcd'), false)

  assert.equal(summarizeToolCall('probe_models', { model: 'opencode/big-pickle' }), '检测模型 opencode/big-pickle')
  assert.equal(summarizeToolCall('probe_models', {}), '检测全部模型（消耗少量免费额度）')
  assert.equal(summarizeToolCall('restart_core'), '重启核心服务')
  assert.equal(summarizeToolCall('shutdown_core'), '停止核心服务（模型立即不可用）')
  assert.equal(summarizeToolCall('check_update', {}), '检查是否有新版本（不下载、不安装）')
  assert.equal(summarizeToolCall('download_update', {}), '下载并安装新版本')
  assert.equal(summarizeToolCall('apply_update', {}), '重启应用以启用新版本')

  // 申请页：确认框要能把地址摊开给用户看（点了会离开本应用去浏览器）。
  assert.equal(summarizeToolCall('open_provider_page', { provider: 'modelscope' }).includes('ModelScope'), true)
  const page = toolCallPreview('open_provider_page', { provider: 'zhipuai' })
  assert.equal(page.地址, PROVIDER_PAGES.zhipuai)
  assert.equal(toolCallPreview('open_provider_page', { provider: 'evil' }).地址, '不在名单内')
  // 名单外的平台 id 在摘要里原样显示：校验环节已经拦下它，确认框只是让人看清模型编了什么。
  assert.equal(summarizeToolCall('open_provider_page', { provider: 'unknown' }).includes('unknown'), true)
})

// ── 快照与提示词 ─────────────────────────────────────────────────────

test('buildSnapshot：如实转述壳推送快照，缺字段不编造', () => {
  const snapshot = buildSnapshot({
    phase: 'ready',
    message: 'WB Bridge ready at 127.0.0.1:41980; 12 free models',
    models: [
      { id: 'a', toolcall: true, images: true },
      { id: 'b', toolcall: true },
      { id: 'c', chatOnly: true },
    ],
    availableModels: ['a', 'b'],
    probe: { running: true, pending: ['c'] },
    sync: { time: '2026-10-12T00:00:00Z', count: 2 },
    usage: { total: { requests: 9, ok: 8, failed: 1 } },
    activity: { message: '正在探测 opencode/big-pickle' },
    useSystemProxy: true,
  })
  assert.ok(snapshot.includes('阶段：就绪'))
  assert.ok(snapshot.includes('共 3 个'))
  assert.ok(snapshot.includes('探测可用 2 个'))
  assert.ok(snapshot.includes('进行中（2/3）'))
  assert.ok(snapshot.includes('已导入 2 个模型'))
  assert.ok(snapshot.includes('真实请求 9 次'))
  assert.ok(snapshot.includes('已开启'))
  // 能力档位：模型能不能带工具，是「带 tools 的请求为什么 400」的唯一解释来源。
  assert.ok(snapshot.includes('支持工具调用 2 个'))
  assert.ok(snapshot.includes('支持图片输入 1 个'))
  assert.ok(snapshot.includes('仅对话（不能带工具）1 个'))
  assert.ok(snapshot.includes(`合成路由 ${AUTO_MODEL_ID}`))

  // 同步失败时错误原文优先于时间。
  assert.ok(buildSnapshot({ sync: { error: '写入失败' } }).includes('失败：写入失败'))
  // 空状态：不抛，也给得出「未知阶段」而不是 undefined。
  const empty = buildSnapshot({})
  assert.ok(empty.includes('阶段：未知'))
  assert.ok(empty.includes('尚未导入'))
  assert.ok(empty.includes('支持工具调用 0 个'))
})

test('phaseLabel：已知阶段给中文，未知阶段原样回显不编造', () => {
  assert.equal(phaseLabel('ready'), '就绪')
  assert.equal(phaseLabel('starting'), '启动中')
  assert.equal(phaseLabel('error'), '故障')
  assert.equal(phaseLabel('stopping'), '停止中')
  assert.equal(phaseLabel('draining'), 'draining')
  assert.equal(phaseLabel(undefined), '未知')
})

test('buildSystemPrompt / buildRequestMessages：system 打头、工具纪律入提示、历史限量', () => {
  const prompt = buildSystemPrompt('【服务状态】阶段：就绪')
  assert.ok(prompt.includes('【当前状态快照】'))
  assert.ok(prompt.includes('【服务状态】阶段：就绪'))
  for (const name of Object.keys(TOOL_META)) assert.ok(prompt.includes(name), `提示词未提及 ${name}`)
  assert.ok(prompt.includes('提案'))
  // 更新链路只有一条固定顺序，提示词必须把它讲死（否则模型会跳过 check_update 直接装）。
  assert.ok(prompt.includes('先 check_update'))
  assert.ok(prompt.includes('apply_update 会立刻退出并重启本应用'))
  // 做不到的事要明说，防止模型假装切换视图。
  assert.ok(prompt.includes('切换面板视图'))

  const history = Array.from({ length: 30 }, (_, i) => ({ role: i % 2 ? 'assistant' : 'user', content: `m${i}` }))
  const messages = buildRequestMessages({ history, message: '现在什么状态', snapshot: 'S' })
  assert.equal(messages[0].role, 'system')
  assert.equal(messages[messages.length - 1].role, 'user')
  assert.equal(messages[messages.length - 1].content, '现在什么状态')
  // 最小上下文：默认只带最近 6 条历史，外加本轮提问；窗口截断按完整历史显式标注省略条数。
  const body = messages.slice(1)
  assert.equal(body.length, 8)
  assert.ok(body[0].content.includes('已省略'))
  assert.ok(body[0].content.includes('24'))
  // 超字符预算时才从最早开始丢，并显式标注省略条数（不让模型看到无声断裂的对话）。
  const squeezed = buildRequestMessages({ history, message: '问', maxChars: 5 })
  assert.ok(squeezed[1].content.includes('已省略'))
  assert.equal(squeezed[squeezed.length - 1].content, '问')

  // 单条超长历史被丢弃时不抛，且至少保留最近一条。
  const huge = [{ role: 'user', content: 'x'.repeat(30000) }, { role: 'assistant', content: '短' }]
  const trimmed = buildRequestMessages({ history: huge, message: '问', maxChars: 100 })
  assert.equal(trimmed[trimmed.length - 1].content, '问')
  assert.ok(trimmed.some(m => m.content === '短'))

  // 单条历史超过 messageChars 就地截断并标注，不再撑爆请求。
  const longMsg = 'y'.repeat(6000)
  const capped = buildRequestMessages({ history: [{ role: 'user', content: longMsg }], message: '问' })
  const cappedBody = capped.find(m => m.content.startsWith('yyy'))
  assert.ok(cappedBody, '超长历史条目必须保留（截断而非丢弃）')
  assert.ok(cappedBody.content.endsWith('…（已截断）'))
  assert.ok(cappedBody.content.length < longMsg.length)
})

// ── 端点配置 ────────────────────────────────────────────────────────

test('validateConfig：https 放行、http 仅本机、缺模型名拦截', () => {
  assert.equal(validateConfig({ baseUrl: 'https://api.deepseek.com/v1', model: 'deepseek-chat' }), null)
  assert.equal(validateConfig({ baseUrl: 'http://127.0.0.1:11434/v1', model: 'qwen2.5' }), null)
  assert.equal(validateConfig({ baseUrl: 'http://localhost:8000/v1', model: 'qwen' }), null)
  assert.ok(validateConfig({ baseUrl: 'http://api.deepseek.com/v1', model: 'm' }).includes('本机'))
  assert.ok(validateConfig({ baseUrl: 'api.deepseek.com', model: 'm' }).includes('http'))
  assert.ok(validateConfig({ baseUrl: 'https://api.deepseek.com/v1', model: '  ' }).includes('模型'))
  assert.ok(validateConfig({ baseUrl: '', model: 'm' }).includes('服务地址'))
})

test('currentConfig：未保存时回落默认端点，本机大脑默认用 WB · auto', () => {
  // node 环境没有 localStorage：prefs 静默降级为默认值。
  assert.deepEqual(currentConfig(), {
    brain: 'self',
    baseUrl: DEFAULT_BASE_URL,
    model: DEFAULT_MODEL,
    selfModel: AUTO_MODEL_ID,
    tools: true,
  })
  assert.equal(AUTO_MODEL_ID, 'WB · auto')
  assert.equal(DEFAULT_SELF_MODEL, AUTO_MODEL_ID)
  assert.equal(persistConfig({ brain: 'external', baseUrl: 'https://api.deepseek.com/v1', model: 'deepseek-chat' }), false)
  assert.equal(AI_PRESETS[0].baseUrl, DEFAULT_BASE_URL)
  assert.equal(AI_PRESETS[0].model, DEFAULT_MODEL)
})

test('validateSelfConfig：WB · auto 免校验，指定模型时须在名单内', () => {
  // auto 是核心的合成路由，刻意不进 status.json 的模型名单，因此也不可能在 modelIds 里；
  // 把它当普通 id 校验会必然报「不在名单」，助手就没默认大脑可用了。
  assert.equal(validateSelfConfig({ selfModel: AUTO_MODEL_ID, modelIds: ['opencode/big-pickle'] }), null)
  assert.equal(validateSelfConfig({ selfModel: AUTO_MODEL_ID, modelIds: [] }), null)
  assert.equal(validateSelfConfig({ selfModel: 'opencode/big-pickle' }), null)
  assert.equal(validateSelfConfig({ selfModel: 'opencode/big-pickle', modelIds: ['opencode/big-pickle'] }), null)
  assert.ok(validateSelfConfig({ selfModel: '' }).includes('本机模型'))
  assert.ok(validateSelfConfig({ selfModel: '' }).includes(AUTO_MODEL_ID))
  assert.ok(validateSelfConfig({ selfModel: 'ghost/model', modelIds: ['opencode/big-pickle'] }).includes('不在当前名单'))
  // 名单为空（核心未就绪）时不拦：让壳去报「核心服务未运行」。
  assert.equal(validateSelfConfig({ selfModel: 'ghost/model', modelIds: [] }), null)
})

// ── 工具执行与回传 ──────────────────────────────────────────────────

test('runTool 只读工具：直接取面板快照，不发请求', async () => {
  const state = {
    phase: 'ready',
    models: [{ id: 'a' }, { id: 'b' }],
    availableModels: ['a'],
    modelResults: { b: { ok: false, message: '额度不足' } },
    usage: { total: { requests: 3, ok: 3, failed: 0 } },
  }
  const status = await runTool('get_status', {}, { state })
  assert.equal(status.ok, true)
  assert.equal(status.result.phase, 'ready')
  assert.equal(status.result.models, 2)
  assert.equal(status.result.availableModels, 1)

  const list = await runTool('list_models', { filter: 'unavailable' }, { state })
  assert.equal(list.result.count, 1)
  assert.equal(list.result.models[0].id, 'b')
  assert.equal(list.result.models[0].note, '额度不足')

  // 能力位随模型行回传：模型据此能回答「这个模型能不能带工具」，而不是等请求 400。
  const capable = await runTool(
    'list_models',
    { filter: 'all' },
    { state: { models: [{ id: 'a', toolcall: true, images: true }, { id: 'b' }], availableModels: ['a'], modelResults: { b: { chatOnly: true } } } },
  )
  assert.deepEqual(capable.result.models[0], {
    id: 'a',
    name: undefined,
    available: true,
    toolcall: true,
    images: true,
    chatOnly: false,
    lastOk: null,
    note: undefined,
  })
  assert.equal(capable.result.models[1].chatOnly, true)

  const usage = await runTool('get_usage', {}, { state })
  assert.deepEqual(usage.result, { total: { requests: 3, ok: 3, failed: 0 } })

  // 未知工具不执行，明确报错。
  const unknown = await runTool('drop_database', {}, { state })
  assert.equal(unknown.ok, false)
  assert.ok(unknown.error.includes('未知工具'))
})

test('toolResultMessage：tool 消息带 call id，结果是 JSON 文本', () => {
  const message = toolResultMessage('call_1', { ok: true, result: { phase: 'ready' } })
  assert.equal(message.role, 'tool')
  assert.equal(message.tool_call_id, 'call_1')
  assert.deepEqual(JSON.parse(message.content), { ok: true, result: { phase: 'ready' } })
  assert.deepEqual(JSON.parse(toolResultMessage('call_1', { ok: false, error: 'x' }).content), { ok: false, error: 'x' })
})

// ── 请求裁剪（壳侧 AI_MAX_MESSAGES 闸门的前端配张） ──────────────────

test('trimRequest：按整块裁剪，assistant 与它的 tool 结果不分离', () => {
  // 一块 = 一条带 tool_calls 的 assistant + 紧随的全部 tool 结果。
  const request = [
    { role: 'system', content: 'S' },
    { role: 'user', content: 'u1' },
    { role: 'assistant', content: '', tool_calls: [{ id: 'a' }, { id: 'b' }] },
    { role: 'tool', tool_call_id: 'a', content: '{}' },
    { role: 'tool', tool_call_id: 'b', content: '{}' },
    { role: 'user', content: 'u2' },
    { role: 'assistant', content: 'ok' },
  ]
  assert.equal(trimRequest(request), 0, '未超限时不动')
  const grown = [
    { role: 'system', content: 'S' },
    ...Array.from({ length: REQUEST_MESSAGE_LIMIT }, (_, i) => ({ role: 'user', content: `u${i}` })),
    { role: 'assistant', content: 'tail' },
  ]
  assert.ok(grown.length > REQUEST_MESSAGE_LIMIT)
  const dropped = trimRequest(grown)
  assert.ok(dropped > 0)
  assert.ok(grown.length <= REQUEST_MESSAGE_LIMIT)
  assert.equal(grown[0].role, 'system', 'system 永远保留')
  assert.equal(grown[grown.length - 1].content, 'tail', '最后一块永远保留')

  // 只剩 system + 最后一块时无从裁剪：原样返回、不丢本轮内容。
  const tiny = [{ role: 'system', content: 'S' }, { role: 'user', content: 'u' }]
  assert.equal(trimRequest(tiny, 1), 0)
  assert.equal(tiny.length, 2)

  // 被丢掉的块里不剩孤儿 tool 消息（每一条 tool 都能找到对应的 assistant）。
  const withTools = [
    { role: 'system', content: 'S' },
    { role: 'assistant', content: '', tool_calls: [{ id: 'x' }] },
    { role: 'tool', tool_call_id: 'x', content: '{}' },
    { role: 'user', content: 'now' },
  ]
  trimRequest(withTools, 2)
  const toolIds = new Set(withTools.filter(m => m.tool_calls).flatMap(m => m.tool_calls.map(c => c.id)))
  for (const m of withTools) {
    if (m.role === 'tool') assert.ok(toolIds.has(m.tool_call_id), `孤儿 tool 消息：${m.tool_call_id}`)
  }
})
