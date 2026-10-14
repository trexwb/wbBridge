// AI 助手引擎：协议封装、工具协议、提示词、快照与受控 Markdown 渲染。
// 与 fastenerTradeWorkbench 的 src/core/ai.js 对齐分工，但按本工程纪律裁剪：
//  • 不落盘任何对话内容与凭据：apiKey 只在会话内存里，请求时随命令参数进壳
//    （壳侧 ai_chat 校验端点白名单，Key 不进日志、不进事件、不进 prefs）；
//  • 网络出口只有一个：bridge.js 的 aiChat()（壳侧 ureq + SSE；面板 CSP 只放行 ipc:）；
//  • 纯函数（prompt / tools / 校验 / 渲染 / 快照）集中导出，node --test 直接测。
import { action, aiAbort, aiChat, dataDir, openExternal, readLog } from './bridge.js'
import { loadAiConfig, saveAiConfig } from './prefs.js'
// 更新走 update.js 的同一台状态机（与「关于与更新」视图共享），AI 不另开一条更新链路。
import { check as checkUpdate, install as installUpdate, restart as restartApp } from './update.js'

// ── 端点预设 ────────────────────────────────────────────────────────
// 预设只回填地址与模型名（不立即保存）；用户也可手填任意 OpenAI 兼容端点。
export const AI_PRESETS = [
  { label: 'DeepSeek', baseUrl: 'https://api.deepseek.com/v1', model: 'deepseek-chat' },
  { label: '智谱', baseUrl: 'https://open.bigmodel.cn/api/paas/v4', model: 'glm-4.5-flash' },
  { label: '通义千问', baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1', model: 'qwen-plus' },
  { label: 'OpenAI', baseUrl: 'https://api.openai.com/v1', model: 'gpt-4o-mini' },
  { label: '本地 Ollama', baseUrl: 'http://127.0.0.1:11434/v1', model: 'qwen2.5' },
]

export const DEFAULT_BASE_URL = AI_PRESETS[0].baseUrl
export const DEFAULT_MODEL = AI_PRESETS[0].model

// 本机大脑的默认模型：`WB · auto` 是核心发布的**合成路由**（src-tauri/core/src/auto.rs
// 的 AUTO_MODEL_ID 逐字一致），按当次请求内容在可用池里选一个真实模型转发——带 tools 的
// 请求只会落到支持工具调用的模型，因此助手代管服务时不必先手动挑模型。
// 它刻意不进 status.json 的模型列表（那是发布给插件看的别名），所以 validateSelfConfig
// 对它单独放行，其余 id 仍按「必须在名单里」拦。
export const AUTO_MODEL_ID = 'WB · auto'
export const DEFAULT_SELF_MODEL = AUTO_MODEL_ID

// 四家平台的官方 Key 申请页（与面板「平台」视图同一批地址）。壳侧 open_external 有
// https + 字符白名单 + EXTERNAL_HOSTS 精确主机名三重校验，名单外的域名一律拒绝，
// 所以这里的值只可能是这四家；模型编造别的地址会在壳侧被挡下。
export const PROVIDER_PAGES = {
  modelscope: 'https://www.modelscope.cn/my/access/token',
  'siliconflow-cn': 'https://cloud.siliconflow.cn/account/ak',
  'tencent-tokenhub': 'https://console.cloud.tencent.com/tokenhub/apikey',
  zhipuai: 'https://open.bigmodel.cn/usercenter/apikeys',
}

// 平台注册表（与核心 providers.rs 的面板视图一致）：set/clear-provider-key 的入参白名单。
export const PROVIDER_IDS = ['modelscope', 'siliconflow-cn', 'tencent-tokenhub', 'zhipuai']
export const PROVIDER_LABELS = {
  modelscope: 'ModelScope',
  'siliconflow-cn': 'SiliconFlow',
  'tencent-tokenhub': '腾讯混元 TokenHub',
  zhipuai: '智谱',
}

// ── 工具定义 ────────────────────────────────────────────────────────
// 每个工具：function schema（发给模型）+ 面板侧元数据（确认弹窗用）。
// write=true 的工具是**提案**：模型发起 → 用户逐条勾选确认 → 才执行；
// 只读工具立即执行，不进确认弹窗。
const TOOL_DEFS = [
  {
    name: 'get_status',
    write: false,
    label: '读取服务状态',
    description: '读取 WB Bridge 当前运行状态快照（服务阶段、模型统计、探测与同步状态、用量汇总）。只读，立即执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'list_models',
    write: false,
    label: '列出模型',
    description: '列出已发现的免费模型及其探测结果。只读，立即执行。',
    parameters: {
      type: 'object',
      properties: {
        filter: {
          type: 'string',
          enum: ['all', 'available', 'unavailable'],
          description: 'all=全部；available=只列探测可用的；unavailable=只列不可用的',
        },
      },
    },
  },
  {
    name: 'get_usage',
    write: false,
    label: '读取用量统计',
    description: '读取核心累计的真实客户端请求统计（总请求/成功/失败与逐模型明细）。只读，立即执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'read_log',
    write: false,
    label: '读取运行日志',
    description: '读取核心运行日志的尾部（固定字节/行数上限）。只读，立即执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'provider_status',
    write: false,
    label: '读取平台配置',
    description: '读取四个免费模型平台的 Key 配置状态（只回「是否已配置」，不回任何 Key 内容）。只读，立即执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'get_environment',
    write: false,
    label: '读取运行环境',
    description:
      '读取本机运行环境：数据目录、应用与核心版本、WorkBuddy / CodeBuddy 配置文件定位结果、最近一次发布状态。只读，立即执行；不回显任何凭据。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'check_update',
    write: false,
    label: '检查更新',
    description:
      '向更新端点检查是否有新版本（只读：不下载、不安装）。返回「已是最新」或新版本号与说明。只读，立即执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'refresh_models',
    write: true,
    label: '重新读取免费模型',
    description: '重新读取免费模型（会重启隔离的模型子进程，随后自动探测）。写入类提案，需用户确认后执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'probe_models',
    write: true,
    label: '检测模型',
    description: '向模型发送简短请求以检测可用性（会消耗少量免费额度）。写入类提案，需用户确认后执行。',
    parameters: {
      type: 'object',
      properties: {
        model: { type: 'string', description: '可选：单个模型 id（须来自 list_models 的结果）；缺省为检测全部模型' },
      },
    },
  },
  {
    name: 'import_models',
    write: true,
    label: '导入模型到插件',
    description: '把探测通过的模型写入 WorkBuddy / CodeBuddy 配置。写入类提案，需用户确认后执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'set_provider_key',
    write: true,
    label: '保存平台 Key',
    description: '把某平台的 Key 保存到本机数据目录（随后自动应用）。写入类提案，需用户确认后执行；执行后不回显 Key。',
    parameters: {
      type: 'object',
      properties: {
        provider: { type: 'string', enum: PROVIDER_IDS, description: '平台 id' },
        apiKey: { type: 'string', description: '用户提供的平台 Key 明文' },
      },
      required: ['provider', 'apiKey'],
    },
  },
  {
    name: 'clear_provider_key',
    write: true,
    label: '清除平台 Key',
    description: '清除某平台已保存的 Key（随后自动应用，该平台模型从列表移除）。写入类提案，需用户确认后执行。',
    parameters: {
      type: 'object',
      properties: { provider: { type: 'string', enum: PROVIDER_IDS, description: '平台 id' } },
      required: ['provider'],
    },
  },
  {
    name: 'set_system_proxy',
    write: true,
    label: '切换系统代理',
    description: '开启或关闭「使用系统代理」。写入类提案，需用户确认后执行。',
    parameters: {
      type: 'object',
      properties: { enabled: { type: 'boolean', description: 'true=开启，false=关闭' } },
      required: ['enabled'],
    },
  },
  {
    name: 'restart_core',
    write: true,
    danger: true,
    label: '重启核心服务',
    description: '重启核心服务（高危：会中断进行中的探测与同步，随后重新读取模型）。写入类提案，需用户确认后执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'shutdown_core',
    write: true,
    danger: true,
    label: '停止核心服务',
    description:
      '停止核心服务（高危：模型立即不再可用，WorkBuddy / CodeBuddy 的请求会失败；随时可用 restart_core 重新拉起）。写入类提案，需用户确认后执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'open_provider_page',
    write: true,
    label: '打开平台申请页',
    description:
      '用系统浏览器打开某平台的 API Key 官方申请页（只在用户说要去申请 / 查看 Key 时调用；本工具不会替用户填写任何内容）。写入类提案，需用户确认后执行。',
    parameters: {
      type: 'object',
      properties: { provider: { type: 'string', enum: PROVIDER_IDS, description: '平台 id' } },
      required: ['provider'],
    },
  },
  {
    name: 'download_update',
    write: true,
    danger: true,
    label: '下载并安装更新',
    description:
      '下载并安装已检查到的新版本（高危：必须先调用 check_update 且确有新版本，否则这一步会失败；安装后需用户确认再执行 apply_update 重启生效）。写入类提案，需用户确认后执行。',
    parameters: { type: 'object', properties: {} },
  },
  {
    name: 'apply_update',
    write: true,
    danger: true,
    label: '重启应用以生效',
    description:
      '重启应用以启用已下载完成的新版本（高危：本应用与核心服务会立即退出重启，进行中的请求全部中断）。写入类提案，需用户确认后执行。',
    parameters: { type: 'object', properties: {} },
  },
]

const DEF_BY_NAME = new Map(TOOL_DEFS.map(def => [def.name, def]))

// 发给模型的 OpenAI tools 数组。
export const AI_TOOLS = TOOL_DEFS.map(def => ({
  type: 'function',
  function: {
    name: def.name,
    description: def.description,
    parameters: def.parameters,
  },
}))

// 面板侧元数据：label（卡片标题）、write（是否需确认）、danger（高危样式）。
export const TOOL_META = Object.fromEntries(
  TOOL_DEFS.map(def => [def.name, { label: def.label, write: def.write, danger: def.danger === true }]),
)

// ── Key 掩码 ────────────────────────────────────────────────────────
// 确认弹窗里会出现 set_provider_key 的 Key：只显示前后各 4 位与长度，绝不完整回显。
export function maskKey(key) {
  const text = String(key ?? '').trim()
  if (!text) return '(空)'
  if (text.length <= 8) return '*'.repeat(text.length)
  return `${text.slice(0, 4)}…${text.slice(-4)}（${text.length} 字符）`
}

export function phaseLabel(phase) {
  const labels = { starting: '启动中', ready: '就绪', error: '故障', stopping: '停止中' }
  return labels[phase] || String(phase || '未知')
}

// ── 状态快照（system prompt 的上下文底座） ──────────────────────────
// 全部取自壳推送的真实快照；拿不到的字段不编，宁缺勿假。
export function buildSnapshot(state) {
  const s = state && typeof state === 'object' ? state : {}
  const models = Array.isArray(s.models) ? s.models : []
  const available = new Set((Array.isArray(s.availableModels) ? s.availableModels : []).map(String))
  const lines = []
  lines.push(`【服务状态】阶段：${phaseLabel(s.phase)}${s.message ? `（${s.message}）` : ''}`)
  lines.push(
    `【模型】共 ${models.length} 个；探测可用 ${available.size} 个；不可用 ${Math.max(0, models.length - available.size)} 个。`,
  )
  // 能力位直接取自壳推送的模型条目（backend.rs::free_models_in 归一成布尔），
  // 这正是核心 WB · auto 选模型时读的同一批字段，助手据此解释「为什么带工具的请求失败了」。
  const toolCapable = models.filter(m => m?.toolcall === true).length
  const imageCapable = models.filter(m => m?.images === true).length
  const chatOnly = models.filter(m => m?.chatOnly === true).length
  lines.push(
    `【能力】支持工具调用 ${toolCapable} 个、支持图片输入 ${imageCapable} 个、仅对话（不能带工具）${chatOnly} 个。` +
      `合成路由 ${AUTO_MODEL_ID} 按请求内容在这些模型里选：带 tools 的请求只会落到支持工具调用的模型。`,
  )
  if (s.probe?.running) {
    const total = models.length
    const pending = Array.isArray(s.probe.pending) ? s.probe.pending.length : null
    const done = total && Number.isFinite(pending) ? Math.max(0, Math.min(total, total - pending)) : null
    lines.push(`【探测】进行中${done !== null ? `（${done}/${total}）` : ''}。`)
  } else {
    lines.push('【探测】空闲。')
  }
  if (s.sync?.error) lines.push(`【同步】失败：${s.sync.error}`)
  else if (s.sync?.time) lines.push(`【同步】已导入 ${s.sync.count ?? 0} 个模型（${s.sync.time}）。`)
  else lines.push('【同步】尚未导入。')
  const usage = s.usage?.total
  if (usage) {
    lines.push(
      `【用量】真实请求 ${usage.requests ?? 0} 次，成功 ${usage.ok ?? 0}，失败 ${usage.failed ?? 0}。`,
    )
  }
  if (s.activity?.message) lines.push(`【最近活动】${s.activity.message}`)
  if (s.useSystemProxy === true) lines.push('【系统代理】已开启。')
  return lines.join('\n')
}

// ── system prompt ───────────────────────────────────────────────────
export function buildSystemPrompt(snapshot = '') {
  return [
    '你是 WB Bridge 控制面板的 AI 助手。WB Bridge 通过隔离的 OpenCode 运行时为 WorkBuddy / CodeBuddy',
    '提供免费模型：自动发现免费模型 → 逐模型探测 → 只把可用的导入插件配置。你既可以回答使用问题，',
    '也可以通过工具帮用户管理它。',
    '',
    '【工具纪律】',
    '1. 只读工具（get_status / list_models / get_usage / read_log / provider_status /',
    '   get_environment / check_update）立即执行，无需确认。',
    '2. 写入类工具（refresh_models / probe_models / import_models / set_provider_key / clear_provider_key /',
    '   set_system_proxy / restart_core / shutdown_core / open_provider_page /',
    '   download_update / apply_update）只是**提案**：前端会要求用户逐条勾选确认，不会自动执行。',
    '   不要在回复里声称操作已完成，用「我将起草…」的语气；用户确认后系统会把执行结果回传给你。',
    '3. 更新是一条固定顺序：先 check_update；只有它返回新版本才能 download_update（下载并安装），',
    '   装完再用 apply_update 重启生效。跳过前两步的 download_update / apply_update 一定失败，不要重试，',
    '   直接把原因告诉用户。apply_update 会立刻退出并重启本应用，必须提醒用户先保存手头工作。',
    '4. 模型 id 必须来自下方快照或 list_models 的返回，禁止编造；平台 id 只能是',
    '   modelscope / siliconflow-cn / tencent-tokenhub / zhipuai 之一。',
    '5. set_provider_key 的 apiKey 只能使用用户在本轮对话里提供的值；不要编造、不要在回复中复述完整 Key。',
    '   用户还没 Key 时可以起草 open_provider_page 打开官方申请页，但不要声称已保存任何 Key。',
    '6. 数字（模型数、可用数、请求数）一律以快照与工具返回为准，不要自行重算或编造。',
    '7. 做不到的事（如提高上游免费额度、修改 OpenCode 侧策略、切换面板视图）如实说明，不要假装执行。',
    '8. 回复用简体中文，结论先行，措辞简洁；可用简单 Markdown（加粗、列表、代码块）。',
    '',
    '【当前状态快照】',
    snapshot || '（暂无快照：核心可能尚未就绪，可调用 get_status 获取。）',
  ].join('\n')
}

// ── 请求组装 ────────────────────────────────────────────────────────
// history：[{ role: 'user' | 'assistant' | 'tool', content }]（tool 消息由调用方追加）。
// 最小上下文：只带最近 historyLimit 条（默认 6，约 3 轮问答），单条超过 messageChars
// 字符就地截断并标注；仍超总预算时再从最早开始丢。省略条数按**完整历史**计数并显式
// 标注——窗口截断与预算丢弃都不能是无声断裂（免费模型窗口小，请求越小首字越快）。
export function buildRequestMessages({
  history = [],
  message = '',
  snapshot = '',
  historyLimit = 6,
  maxChars = 24000,
  messageChars = 4000,
} = {}) {
  const recent = history.slice(-historyLimit)
  const picked = recent.map(m => {
    const content = String(m.content ?? '')
    return content.length > messageChars
      ? { ...m, content: content.slice(0, messageChars) + '…（已截断）' }
      : m
  })
  let total = picked.reduce((sum, m) => sum + String(m.content ?? '').length, 0)
  while (picked.length > 1 && total > maxChars) {
    const dropped = picked.shift()
    total -= String(dropped.content ?? '').length
  }
  const omitted = history.length - picked.length
  const body = []
  if (omitted > 0) {
    body.push({ role: 'user', content: `（为节省上下文，较早的 ${omitted} 条对话历史已省略）` })
  }
  for (const m of picked) {
    body.push({ role: m.role, content: String(m.content ?? '') })
  }
  body.push({ role: 'user', content: String(message ?? '') })
  return [{ role: 'system', content: buildSystemPrompt(snapshot) }, ...body]
}

// ── 工具调用校验与摘要 ──────────────────────────────────────────────
// validateToolCall(name, args, ctx)：ctx.modelIds（可选）用于核对 probe 的单个模型 id。
export function validateToolCall(name, args, ctx = {}) {
  const def = DEF_BY_NAME.get(name)
  if (!def) return { ok: false, error: `未知工具：${name}` }
  const source = args && typeof args === 'object' && !Array.isArray(args) ? args : {}
  const required = def.parameters.required || []
  for (const key of required) {
    const value = source[key]
    if (value === undefined || value === null || (typeof value === 'string' && !value.trim())) {
      return { ok: false, error: `缺少参数：${key}` }
    }
  }
  if (name === 'probe_models' && source.model !== undefined) {
    const model = String(source.model).trim()
    if (!model) return { ok: false, error: 'model 不能为空字符串' }
    if (Array.isArray(ctx.modelIds) && ctx.modelIds.length && !ctx.modelIds.includes(model)) {
      return { ok: false, error: `模型 id 不在当前名单：${model}（可先调 list_models）` }
    }
  }
  if (name === 'set_provider_key') {
    const key = String(source.apiKey ?? '')
    if (/[\r\n]/.test(key)) return { ok: false, error: 'apiKey 不能包含换行' }
    if (key.length > 1024) return { ok: false, error: 'apiKey 过长' }
  }
  if (name === 'clear_provider_key' || name === 'set_provider_key' || name === 'open_provider_page') {
    if (!PROVIDER_IDS.includes(source.provider)) {
      return { ok: false, error: `provider 必须是 ${PROVIDER_IDS.join(' / ')} 之一` }
    }
  }
  if (name === 'set_system_proxy' && typeof source.enabled !== 'boolean') {
    return { ok: false, error: 'enabled 必须是布尔值' }
  }
  if (name === 'list_models' && source.filter !== undefined && !['all', 'available', 'unavailable'].includes(source.filter)) {
    return { ok: false, error: 'filter 只能是 all / available / unavailable' }
  }
  return { ok: true }
}

// 一行人类可读的操作描述（确认弹窗卡片标题）。
export function summarizeToolCall(name, args = {}) {
  const source = args && typeof args === 'object' ? args : {}
  switch (name) {
    case 'get_status':
      return '读取服务状态'
    case 'list_models':
      return `列出模型（${source.filter === 'available' ? '仅可用' : source.filter === 'unavailable' ? '仅不可用' : '全部'}）`
    case 'get_usage':
      return '读取用量统计'
    case 'read_log':
      return '读取运行日志尾部'
    case 'provider_status':
      return '读取平台配置状态'
    case 'get_environment':
      return '读取运行环境（数据目录 / 版本 / 插件配置定位）'
    case 'check_update':
      return '检查是否有新版本（不下载、不安装）'
    case 'refresh_models':
      return '重新读取免费模型（重启隔离子进程）'
    case 'probe_models':
      return source.model ? `检测模型 ${source.model}` : '检测全部模型（消耗少量免费额度）'
    case 'import_models':
      return '导入可用模型到 WorkBuddy / CodeBuddy'
    case 'set_provider_key':
      return `保存 ${PROVIDER_LABELS[source.provider] || source.provider} 的 Key（${maskKey(source.apiKey)}）`
    case 'clear_provider_key':
      return `清除 ${PROVIDER_LABELS[source.provider] || source.provider} 的 Key`
    case 'set_system_proxy':
      return `${source.enabled === true ? '开启' : '关闭'}系统代理`
    case 'restart_core':
      return '重启核心服务'
    case 'shutdown_core':
      return '停止核心服务（模型立即不可用）'
    case 'download_update':
      return '下载并安装新版本'
    case 'apply_update':
      return '重启应用以启用新版本'
    case 'open_provider_page':
      return `打开 ${PROVIDER_LABELS[source.provider] || source.provider} 的官方 Key 申请页`
    default:
      return name
  }
}

// 确认弹窗的字段预览（键值对，已做掩码/截断）。
export function toolCallPreview(name, args = {}) {
  const source = args && typeof args === 'object' ? args : {}
  switch (name) {
    case 'set_provider_key':
      return { 平台: PROVIDER_LABELS[source.provider] || source.provider, Key: maskKey(source.apiKey) }
    case 'clear_provider_key':
      return { 平台: PROVIDER_LABELS[source.provider] || source.provider }
    case 'open_provider_page':
      return { 平台: PROVIDER_LABELS[source.provider] || source.provider, 地址: PROVIDER_PAGES[source.provider] || '不在名单内' }
    case 'probe_models':
      return source.model ? { 模型: source.model } : { 范围: '全部模型' }
    case 'set_system_proxy':
      return { 系统代理: source.enabled === true ? '开启' : '关闭' }
    case 'list_models':
      return { 范围: source.filter === 'available' ? '仅可用' : source.filter === 'unavailable' ? '仅不可用' : '全部' }
    default:
      return {}
  }
}

// ── 模型行（list_models / 快照共用） ────────────────────────────────
function modelRows(state) {
  const models = Array.isArray(state?.models) ? state.models : []
  const results = state?.modelResults && typeof state.modelResults === 'object' ? state.modelResults : {}
  const available = new Set((Array.isArray(state?.availableModels) ? state.availableModels : []).map(String))
  return models.map(m => {
    const id = String(m?.id ?? '')
    const r = results[id]
    return {
      id,
      name: typeof m?.name === 'string' ? m.name : undefined,
      available: available.has(id),
      // 能力位来自模型条目本身（与核心 WB · auto 的选择器读同一批字段），
      // 助手因此能说清「这个模型不能带工具」而不是等请求 400。
      toolcall: m?.toolcall === true,
      images: m?.images === true,
      chatOnly: m?.chatOnly === true || r?.chatOnly === true,
      lastOk: r && typeof r.ok === 'boolean' ? r.ok : null,
      note: r && r.ok === false && typeof r.message === 'string' ? r.message.slice(0, 120) : undefined,
    }
  })
}

function wrap(response) {
  return response.ok ? { ok: true, result: response.result } : { ok: false, error: String(response.error) }
}

// ── 工具执行 ────────────────────────────────────────────────────────
// ctx = { state }：只读工具直接取面板持有的壳推送快照，不发额外请求。
// 写工具经 bridge.action() 走壳的 core_action（与面板按钮同一条链路）。
// 🔴 调用方必须已通过 validateToolCall + 用户确认（写工具）；本函数不再二次拦截。
export async function runTool(name, args = {}, ctx = {}) {
  const state = ctx.state && typeof ctx.state === 'object' ? ctx.state : {}
  switch (name) {
    case 'get_status':
      return {
        ok: true,
        result: {
          phase: state.phase ?? null,
          message: state.message ?? null,
          models: Array.isArray(state.models) ? state.models.length : 0,
          availableModels: Array.isArray(state.availableModels) ? state.availableModels.length : 0,
          probeRunning: state.probe?.running === true,
          useSystemProxy: state.useSystemProxy === true,
          sync: state.sync ?? null,
        },
      }
    case 'list_models': {
      const filter = args.filter === 'available' || args.filter === 'unavailable' ? args.filter : 'all'
      const rows = modelRows(state).filter(row =>
        filter === 'all' ? true : filter === 'available' ? row.available : !row.available,
      )
      return { ok: true, result: { count: rows.length, models: rows.slice(0, 200) } }
    }
    case 'get_usage':
      return { ok: true, result: state.usage ?? null }
    case 'read_log': {
      const response = await readLog()
      if (!response.ok) return { ok: false, error: String(response.error) }
      const log = response.result || {}
      return {
        ok: true,
        result: { text: String(log.text ?? '').slice(-8000), truncated: log.truncated === true, bytes: log.bytes ?? 0 },
      }
    }
    case 'provider_status': {
      const response = await action('provider-status')
      if (!response.ok) return { ok: false, error: String(response.error) }
      const providers = Array.isArray(response.result?.providers) ? response.result.providers : []
      return {
        ok: true,
        result: providers.map(p => ({ id: p.id, label: p.label, configured: p.configured === true })),
      }
    }
    case 'get_environment': {
      // 只组装壳推送的真实快照 + data_dir_path 的返回值：面板版本这类构建期常量
      // （__APP_VERSION__ 是 Vite define）在本模块不可用（node --test 直接 import 本文件），
      // 拿不到的字段不编，宁缺勿假。
      const response = await dataDir()
      return {
        ok: true,
        result: {
          dataDir: response.ok ? String(response.result ?? '') : '',
          coreVersion: state.version ?? null,
          opencodeVersion: state.opencodeVersion ?? null,
          schemaVersion: state.schemaVersion ?? null,
          endpoint: state.endpoint ?? null,
          workBuddyModelsFile: state.modelsFile ?? null,
          codeBuddyModelsFile: state.codeBuddyModelsFile ?? null,
          sync: state.sync ?? null,
          dirError: response.ok ? null : String(response.error),
        },
      }
    }
    case 'check_update': {
      const update = await checkUpdate({ silent: false })
      if (update.status === 'error') return { ok: false, error: String(update.error || '检查更新失败') }
      return {
        ok: true,
        result: {
          status: update.status,
          version: update.version || null,
          notes: update.notes ? String(update.notes).slice(0, 2000) : null,
        },
      }
    }
    case 'download_update': {
      // 状态机要求先有 available：没有就直接报错回给模型，不静默返回"成功但没装"。
      const update = await installUpdate()
      if (update.status === 'error') return { ok: false, error: String(update.error || '下载安装失败') }
      if (update.status !== 'ready') {
        return { ok: false, error: `当前状态为 ${update.status}，没有可安装的更新（请先 check_update）` }
      }
      return { ok: true, result: { status: 'ready', version: update.version || null } }
    }
    case 'apply_update': {
      const response = await restartApp()
      return wrap(response)
    }
    case 'open_provider_page':
      return wrap(await openExternal(PROVIDER_PAGES[args.provider]))
    case 'refresh_models': {
      const response = await action('refresh')
      if (!response.ok) return { ok: false, error: String(response.error) }
      const models = Array.isArray(response.result?.models) ? response.result.models : []
      return { ok: true, result: { models: models.length } }
    }
    case 'probe_models': {
      const response = await action('probe', args.model ? { model: String(args.model) } : null)
      return wrap(response)
    }
    case 'import_models':
      return wrap(await action('import'))
    case 'set_provider_key':
      return wrap(await action('set-provider-key', { provider: args.provider, apiKey: String(args.apiKey ?? '') }))
    case 'clear_provider_key':
      return wrap(await action('clear-provider-key', { provider: args.provider }))
    case 'set_system_proxy':
      return wrap(await action('system-proxy', { enabled: args.enabled === true }))
    case 'restart_core':
      return wrap(await action('restart'))
    case 'shutdown_core':
      return wrap(await action('shutdown'))
    default:
      return { ok: false, error: `未知工具：${name}` }
  }
}

// 工具结果 → 回传给模型的 tool 消息内容（JSON 文本，体积受控）。
export function toolResultMessage(toolCallId, result) {
  return { role: 'tool', tool_call_id: String(toolCallId ?? ''), content: JSON.stringify(result ?? {}) }
}

// ── 对话协议 ────────────────────────────────────────────────────────
// 一轮流式请求。两种大脑来源共用同一条命令：
//  • 外部端点：带 baseUrl + apiKey（Key 只进壳进程内存）；
//  • 自托管（selfHosted = true）：端点与本机 Bearer 由壳自己从正在运行的核心解析，
//    面板既拿不到也不知道 api-key，因此这里显式不传 baseUrl/apiKey。
// tools 缺省空数组 = 本轮不启用工具（纯文本对话），由调用方决定是否声明工具集。
export async function chat({ baseUrl, apiKey, model, messages, tools = [], toolChoice, selfHosted = false }, onDelta) {
  const request = { model, messages, tools, selfHosted }
  if (!selfHosted) {
    request.baseUrl = baseUrl
    request.apiKey = apiKey
  }
  // 只在调用方显式指定时透传：多数端点认 auto，多余的键对老端点是噪声。
  if (toolChoice) request.toolChoice = toolChoice
  return aiChat(request, onDelta)
}

// 单次请求的消息数上限：壳侧 `AI_MAX_MESSAGES`（src-tauri/src/lib.rs）会拒收超限请求，
// 多轮工具循环每轮都往请求里追加 assistant + N 条 tool 结果，长对话必然撞上。
// 取值留了余量：裁剪后的请求加上 tool 消息仍应明显低于那道闸门。
export const REQUEST_MESSAGE_LIMIT = 32

// 就地裁剪请求：从最旧的「整块」开始丢，system 与最后一块永远保留。
// 为什么按块丢：一条 assistant（带 tool_calls）与它后面的全部 role:'tool' 结果是绑定的，
// 只丢 assistant 会让 tool 消息失去对应 id，OpenAI 兼容端点直接判协议错。
// 返回值是丢弃的条数（调用方据此在界面如实说明「为控制上下文丢弃了 N 条」，不静默断裂）。
export function trimRequest(messages, limit = REQUEST_MESSAGE_LIMIT) {
  const list = Array.isArray(messages) ? messages : []
  if (list.length <= limit) return 0
  const blocks = splitToolBlocks(list)
  // 只剩 system + 最后一块时无从裁剪（宁可让壳报一次超限，也不把本轮内容吃掉）。
  if (blocks.length <= 2) return 0
  let size = list.length
  let skip = 0
  for (let index = 1; index < blocks.length - 1 && size > limit; index++) {
    size -= blocks[index].messages.length
    skip += 1
  }
  if (!skip) return 0
  const trimmed = [blocks[0], ...blocks.slice(1 + skip)].flatMap(block => block.messages)
  // 丢弃条数必须在就地重写**之前**算：list 与 messages 是同一数组，裁完再读
  // list.length 拿到的是裁剪后的长度，差值恒为 0（界面会因此永远看不到提示）。
  const dropped = list.length - trimmed.length
  // 原地重写：调用方持有同一数组引用（工具循环每轮 push 进同一个 request）。
  messages.length = 0
  messages.push(...trimmed)
  return dropped
}

// 分块：一条带 tool_calls 的 assistant 起头，吞掉紧随其后的所有 tool 结果；其余消息各自成块。
function splitToolBlocks(list) {
  const blocks = []
  for (const message of list) {
    const last = blocks[blocks.length - 1]
    if (message?.role === 'tool' && last?.open) {
      last.messages.push(message)
      continue
    }
    blocks.push({
      open: Array.isArray(message?.tool_calls) && message.tool_calls.length > 0,
      messages: [message],
    })
  }
  return blocks
}

export function abortChat() {
  return aiAbort()
}

// ── 配置（非凭据部分走 prefs；Key 只在会话内存） ────────────────────
// brain='self'：大脑是本壳核心自己的免费模型（端点与 Bearer 由壳解析，无需任何 Key）；
// brain='external'：外部 OpenAI 兼容端点（baseUrl + 会话内存里的 apiKey）。
// 本机大脑默认 `WB · auto`：合成路由按当次请求内容选模型，带 tools 的助手请求因此
// 天然落在「支持工具调用」的模型上，用户不必先手动挑一个 id 才能开始对话。
export function currentConfig() {
  const stored = loadAiConfig()
  return {
    brain: stored.brain === 'external' ? 'external' : 'self',
    baseUrl: stored.baseUrl || DEFAULT_BASE_URL,
    model: stored.model || DEFAULT_MODEL,
    selfModel: stored.selfModel || DEFAULT_SELF_MODEL,
    tools: stored.tools !== false,
  }
}

export function persistConfig({ brain, baseUrl, model, selfModel, tools }) {
  return saveAiConfig({
    brain: brain === 'external' ? 'external' : 'self',
    baseUrl: String(baseUrl ?? '').trim(),
    model: String(model ?? '').trim(),
    selfModel: String(selfModel ?? '').trim(),
    tools: tools !== false,
  })
}

// 发送前预检（壳侧还会用白名单复核一遍，这里只为了让用户早一步看到问题）。
// 只服务外部端点；本机大脑用 validateSelfConfig（无端点、无 Key）。
export function validateConfig({ baseUrl, model }) {
  const url = String(baseUrl ?? '').trim()
  if (!url) return '请填写 AI 服务地址（Base URL）'
  if (!/^https?:\/\/\S+$/i.test(url)) return 'Base URL 需以 http:// 或 https:// 开头'
  if (/^http:\/\//i.test(url) && !/^http:\/\/(127\.0\.0\.1|localhost|\[::1\])(:|\/|$)/i.test(url)) {
    return 'http:// 仅允许本机地址（127.0.0.1 / localhost），其余请用 https://'
  }
  if (!String(model ?? '').trim()) return '请填写模型名称'
  return null
}

// 本机大脑的模型 id 必须在当前名单里（壳推送快照的口径）：免费模型名单由上游决定，
// 填一个不存在的 id 只会在请求发出后才失败，不如发送前就拦下。
// 例外是 `WB · auto`：它是发布给插件的合成路由，刻意不进 status.json 的模型列表
// （auto.rs 的约定），但在 /v1/chat/completions 上真实可路由，因此单独放行。
export function validateSelfConfig({ selfModel, modelIds }) {
  const model = String(selfModel ?? '').trim()
  if (!model) return `请选择本机模型（默认 ${AUTO_MODEL_ID}，或指定一个可用模型）`
  if (model === AUTO_MODEL_ID) return null
  if (Array.isArray(modelIds) && modelIds.length && !modelIds.includes(model)) {
    return '该模型不在当前名单里：请到「模型与服务」确认 id'
  }
  return null
}

// ── 受控 Markdown 渲染 ──────────────────────────────────────────────
// 不引第三方库（CSP 无 CDN）：先整段转义再解析，只支持围栏代码块、行内代码、加粗、
// 标题行与列表行；其余按纯文本处理。任何原始 HTML 都不可能成为标签。
function escHtml(text) {
  return String(text)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

const FENCE_RE = /```([\w-]*)\n?([\s\S]*?)(?:```|$)/g

function renderInline(escaped) {
  return escaped
    .replace(/`([^`\n]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>')
}

function renderTextSegment(segment) {
  const paragraphs = segment.split(/\n{2,}/)
  return paragraphs
    .map(paragraph => {
      const lines = paragraph.split('\n').map(line => {
        let text = renderInline(escHtml(line))
        text = text.replace(/^(#{1,4})\s+/, '')
        if (/^\s*[-*]\s+/.test(line)) text = '• ' + text.replace(/^\s*[-*]\s+/, '')
        if (/^\s*\d+\.\s+/.test(line)) text = text.replace(/^\s*(\d+)\.\s+/, '$1. ')
        return text
      })
      return `<p>${lines.join('<br>')}</p>`
    })
    .join('')
}

export function renderMarkdownLite(text) {
  const src = String(text ?? '').replace(/\r\n/g, '\n')
  if (!src.trim()) return ''
  let out = ''
  let cursor = 0
  for (const match of src.matchAll(FENCE_RE)) {
    out += renderTextSegment(src.slice(cursor, match.index))
    const code = escHtml(match[2].replace(/\n$/, ''))
    out += `<pre class="ai-code"><code>${code}</code></pre>`
    cursor = match.index + match[0].length
  }
  out += renderTextSegment(src.slice(cursor))
  return out
}
