<script setup>
// 平台视图：多平台免费模型的 Key 与付费白名单管理入口。
// 列表保持精简：一行一家平台（名称 / 注册表 id / 状态徽章 / 管理入口）；申请引导、
// Key 表单与付费白名单全部收进同一张弹窗，九家平台不再各占一大块。
// 凭据纪律：Key 只经 set-provider-key 的请求体进入核心；回显只有**显式**一途——打开
// 已配置平台的弹窗时，经 reveal-provider-key 取回**当前这一家**的已存 Key 并回填输入框
// （默认掩码，点眼睛切明文），值只留在组件内存（drafts，不写偏好 / localStorage /
// IndexedDB），关闭弹窗即清出。provider-status 始终只回「是否已配置」。保存 / 清除后自动触发**该平台的定向重新检测**
// （`probe { providers: [id] }`）：隔离的 OpenCode 子进程重启时重读 providers.json 并重新注入
// 声明段，所以无需重启整个应用，其余平台的条目与探测结果一概不动。
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { action, openExternal } from '../core/bridge.js'
import { successMessage } from '../core/ops.js'

const props = defineProps({
  busy: { type: Boolean, default: false },
  // status.json 顶层 paidModels：付费白名单的全量列表（核心已排序去重）。
  paidModels: { type: Array, default: () => [] },
  // status.json 的模型目录（`id` 是全限定 `<命名空间>/<模型 key>`）：付费白名单的默认候选。
  models: { type: Array, default: () => [] },
})

// 静态平台清单（与核心 providers.rs 注册表的 id/label 一致，不含 Key）：
// 卡片**不依赖核心在线**就渲染 —— 卡片列表若完全由 provider-status 的请求结果驱动，
// 核心未就绪/未运行时整页消失，用户没有地方填 Key。请求只负责刷新「已配置」徽章。
const PLATFORMS = [
  { id: 'modelscope', label: 'ModelScope' },
  { id: 'siliconflow-cn', label: 'SiliconFlow' },
  { id: 'tencent-tokenhub', label: '腾讯混元 TokenHub' },
  { id: 'zhipuai', label: '智谱' },
  { id: 'agnes', label: '爱思 Agnes' },
  { id: 'sensenova', label: '商汤 SenseNova' },
  { id: 'nvidia', label: 'NVIDIA NIM' },
  { id: 'openrouter', label: 'OpenRouter' },
  { id: 'huggingface', label: 'Hugging Face' },
]

// 每家平台的申请引导：官方入口（已核实）+ 步骤指引。链接经壳的 open_external 用系统浏览器
// 打开（WebView 里 target=_blank 默认静默失败），面板内不内嵌网页、不发起任何外部请求。
const PLATFORM_GUIDES = {
  modelscope: {
    url: 'https://www.modelscope.cn/my/access/token',
    steps: [
      '打开下方官方申请页，注册或登录魔搭社区（支持支付宝 / GitHub 登录）。',
      '右上角头像 →「Bind Alibabacloud」绑定阿里云账号，并完成阿里云实名认证——免费推理额度挂在这上面，没绑定时调用一律报 401「Please bind your Alibaba Cloud account」。',
      '回到「访问令牌」页创建令牌（ms- 开头），复制粘贴到下方输入框点「保存」。',
      '保存后会自动只重新检测本平台（魔搭）的模型，其余平台不受影响，无需再点「检测全部」。',
    ],
  },
  'siliconflow-cn': {
    url: 'https://cloud.siliconflow.cn/account/ak',
    steps: [
      '打开下方官方申请页，注册或登录硅基流动（新账号含免费额度）。',
      '进入「API 密钥」页，点击「新建 API 密钥」（sk- 开头）。',
      '复制密钥，粘贴到下方输入框点「保存」。',
    ],
  },
  'tencent-tokenhub': {
    url: 'https://console.cloud.tencent.com/tokenhub/apikey',
    steps: [
      '打开下方官方申请页，登录腾讯云并开通 TokenHub 服务。',
      '进入「API Key 管理」页，点击「创建 API Key」（混元 Hy3 等模型按量计费，以平台标价为准）。',
      '复制 Key，粘贴到下方输入框点「保存」。',
    ],
  },
  zhipuai: {
    url: 'https://open.bigmodel.cn/usercenter/apikeys',
    steps: [
      '打开下方官方申请页，注册或登录智谱开放平台。',
      '进入「API keys」页，点击「创建 API Key」（glm-4.5-flash 等 flash 系列免费）。',
      '复制 Key，粘贴到下方输入框点「保存」。',
    ],
  },
  agnes: {
    url: 'https://platform.agnes-ai.com/',
    steps: [
      '打开下方爱思 Agnes 开发者平台，注册或登录。',
      '在控制台创建 API Key（Bearer 形式，直接粘贴 Key 本身、不要带 Bearer 前缀）。',
      '粘贴到下方输入框点「保存」。端点固定为 https://apihub.agnes-ai.com/v1，推广期文本模型免费（以平台公示为准）。',
    ],
  },
  sensenova: {
    url: 'https://platform.sensenova.cn/console',
    steps: [
      '打开下方商汤日日新控制台，注册或登录。',
      '左侧「管理中心 → API-Key 管理」创建 API-Key（完整值只在创建时显示一次）。',
      '复制粘贴到下方输入框点「保存」。公测期有免费额度与速率限制（以平台公示为准）。',
    ],
  },
  nvidia: {
    url: 'https://build.nvidia.com/settings/api-keys',
    steps: [
      '打开下方 NVIDIA Build，注册并验证账号。',
      '在「API Keys」页生成 Key（nvapi- 开头），复制保存。',
      '粘贴到下方输入框点「保存」。新账号含免费推理额度，超出额度或速率会被限流（以平台公示为准）。',
    ],
  },
  openrouter: {
    url: 'https://openrouter.ai/settings/keys',
    steps: [
      '打开下方 OpenRouter，注册或登录。',
      '在「Keys」页创建 API Key（sk-or- 开头），复制保存。',
      '粘贴到下方输入框点「保存」。只会发现标价为零的 :free 模型，免费档有独立速率限制（以平台公示为准）。',
    ],
  },
  huggingface: {
    url: 'https://huggingface.co/settings/tokens',
    steps: [
      '打开下方 Hugging Face，注册或登录。',
      '在「Access Tokens」页创建 token（read 权限即可），复制保存。',
      '粘贴到下方输入框点「保存」。免费推理路由有速率与额度限制（以平台公示为准）。',
    ],
  },
}

// 正在打开的申请页（按钮转 spinner）；打开失败在按钮旁给出提示而不是吞掉。
const opening = ref(null)
const openError = ref(null)

async function openGuide(provider) {
  const url = PLATFORM_GUIDES[provider.id]?.url
  if (!url) return
  // 同步防抖：模板 disabled 要到下一次重渲染才生效，双击会在那之前穿透两次，
  // 开出两个浏览器标签。进入时先查在飞标记，同一时刻只放行第一次。
  if (opening.value) return
  opening.value = provider.id
  openError.value = null
  const response = await openExternal(url)
  opening.value = null
  if (!response.ok) openError.value = response.error
}

// 卡片初始即渲染（configured=false 待刷新）；drafts 按同一顺序初始化，
// 模板直接按 id 取，不在渲染期间造对象。
const providers = ref(PLATFORMS.map(p => ({ ...p, configured: false })))
const error = ref(null)
const loading = ref(false)
const drafts = ref(Object.fromEntries(PLATFORMS.map(p => [p.id, { key: '', feedback: null }])))
// 明文切换：input 的动态 type 由它驱动。打开已配置平台的弹窗时自动经
// `reveal-provider-key` 取回该平台已保存的 Key 回填（掩码显示，一次一条）；
// `revealed` 标记「当前内容来自核心回传且未被改动」——此时保存按钮禁用（避免原样重写
// 再触发一轮重检），收起 / 关闭时把回传值清出内存。值只在本组件里，切走视图即随组件销毁。
const revealing = ref({})
const revealingBusy = ref({})
const revealed = ref({})
const saving = ref({})
const clearing = ref({})
// 正在应用（定向重取 + 重检中）的平台：按钮置忙，防连续保存叠加两轮子进程重启。
const applying = ref({})
const applyingBusy = computed(() => Object.values(applying.value).some(Boolean))

// ── 弹窗 ────────────────────────────────────────────────────────────────
const openId = ref(null)
const openProvider = computed(() => providers.value.find(p => p.id === openId.value) || null)
const dialogPanel = ref(null)
let dialogTrigger = null

function openDialog(provider, event) {
  // 记住触发元素：关闭后焦点必须回到「管理配置」，否则键盘用户要重新找位置。
  dialogTrigger = event && event.currentTarget ? event.currentTarget : null
  openError.value = null
  openId.value = provider.id
  // 已保存的平台：打开弹窗即回填已保存的 Key（掩码显示），不必先点眼睛才知道存了什么。
  // 草稿里已有用户输入（关弹窗时不会清掉）则保留草稿，不拿核心里的旧值覆盖。
  // 不按 `configured` 徽章决定是否取回：徽章来自某一次 provider-status 快照，
  // 核心未就绪或刷新失败时会是 false，而 providers.json 里其实已经有 Key。
  if (!(drafts.value[provider.id].key || '').trim()) {
    revealSavedKey(provider, { auto: true })
  }
  // 弹窗是可聚焦容器（tabindex=-1）：打开即取焦点，Tab 从内容开始走。
  nextTick(() => dialogPanel.value && dialogPanel.value.focus())
}

function closeDialog() {
  const id = openId.value
  if (id) {
    const draft = drafts.value[id]
    // 回传的 Key 不再留在内存里（用户自己输入的内容保留，免得误关一下就丢草稿）。
    if (revealed.value[id]) {
      draft.key = ''
      revealed.value[id] = false
    }
    revealing.value[id] = false
  }
  openId.value = null
  const back = dialogTrigger
  dialogTrigger = null
  nextTick(() => {
    if (back && back.isConnected && !back.disabled) back.focus()
  })
}

function toggleReveal(provider) {
  // 显示 / 掩码是纯本地状态切换：不同步等核心、不置忙、不禁用按钮。
  // 之前把「取回」绑在点击上，请求在飞时按钮被禁用，连点全被吞掉，
  // 要等请求回来才会看到一次切换（看起来像「点了没反应」）。重试挪到后台（见下）。
  const shown = !!revealing.value[provider.id]
  revealing.value[provider.id] = !shown
  // 空框（还没回填到值）时顺带后台再要一次；已保存的值到达后按当前的显示形态落进输入框。
  if (!shown && !(drafts.value[provider.id].key || '').trim()) {
    revealSavedKey(provider)
  }
}

// 眼睛在 pointerdown 上切换，不等 click 合成：弹窗入场动画（enter-rise，4px/260ms）
// 还在推动整块面板时，press 与 release 的落点会差开，浏览器把 click 派发到共同祖先、
// 甚至不派发——表现就是「刚打开的弹窗里点眼睛没反应，过一会儿再点才行」。
// 键盘 / 辅助技术没有 pointerdown，仍走 click；两者用时间戳互斥，一次操作只翻一下。
let eyePressedAt = 0
function onEyePress(provider, event) {
  if (event.button !== 0) return
  eyePressedAt = Date.now()
  toggleReveal(provider)
}

function onEyeClick(provider) {
  if (Date.now() - eyePressedAt < 700) return
  toggleReveal(provider)
}

// 输入框占位：取回进行中只用文字提示，不换图标、不插入额外行，弹窗高度不跳。
function keyPlaceholder(provider) {
  if (revealingBusy.value[provider.id] && !(drafts.value[provider.id].key || '').trim()) {
    return '正在读取已保存的 Key…'
  }
  return provider.configured ? '已保存的 Key（掩码显示）；输入新 Key 可替换' : '粘贴平台 Key'
}

// 取回已保存的 Key：显式、一次一个平台，返回体即凭据本身，只进本组件内存。
// auto = 打开弹窗时的自动回填：掩码显示；只有「尚未配置」静默，其余失败落下可读原因。
async function revealSavedKey(provider, { auto = false } = {}) {
  const draft = drafts.value[provider.id]
  if (revealingBusy.value[provider.id]) return
  revealingBusy.value[provider.id] = true
  const response = await action('reveal-provider-key', { provider: provider.id })
  revealingBusy.value[provider.id] = false
  // 请求期间弹窗被关掉：不要把明文留在内存里，也不要在已卸载的界面上落反馈。
  if (openId.value !== provider.id) return
  if (!response.ok) {
    // 「尚未配置」是正常状态（这家还没存过 Key），包括打开弹窗时的自动回填在内都静默。
    // 其余失败必须说出来：壳命令缺失（应用进程还是旧版）、核心未运行、浏览器预览无 IPC
    // 都会让输入框保持为空，静默会让用户以为「保存的 Key 丢了」。
    if (/尚未配置/.test(response.error)) {
      draft.feedback = auto ? null : { error: false, text: `${provider.label} 还没有保存过 Key：粘贴后点「保存」即可。` }
      return
    }
    draft.feedback = { error: true, text: `读取已保存的 Key 失败：${response.error}${auto ? '（可点输入框右侧的眼睛重试）' : ''}` }
    return
  }
  const key = response.result?.apiKey || ''
  if (!key) {
    draft.feedback = { error: true, text: '核心没有返回 Key：请重新粘贴并保存一次。' }
    return
  }
  // 请求期间用户已经开始输入：不覆盖他的草稿。
  if ((draft.key || '').trim()) return
  draft.key = key
  revealed.value[provider.id] = true
  // 不碰 revealing：用户可能在请求在飞时已经点亮了明文，值到达后直接以该形态显示。
  draft.feedback = null
}

// 弹窗内可聚焦元素（收起 / 禁用 / 不可见的由可见性过滤掉）。
const FOCUSABLE = 'button, input, select, textarea, summary, [tabindex]:not([tabindex="-1"])'

// 焦点陷阱：弹窗打开时 Tab 不能跑到遮罩后面的侧栏上，否则看不见焦点、也关不掉弹窗。
// 可见性用 getClientRects() 判定：弹窗整块在 position:fixed 的遮罩里，
// offsetParent 在这条链上的取值各引擎历史上不一致，拿它当「有没有渲染」会误判。
function trapTab(event, panel) {
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

function onWindowKeydown(event) {
  if (!openProvider.value) return
  if (event.key === 'Tab' && dialogPanel.value) {
    trapTab(event, dialogPanel.value)
    return
  }
  if (event.key !== 'Escape') return
  closeDialog()
}

// ── 付费模型白名单 ────────────────────────────────────────────────────────
// 免费发现按「标价全 0」过滤，付费模型永远进不来；白名单是显式例外，按**全限定 id**
// （`<provider>/<model>`）逐条放行。写入是全量替换（核心侧 set-paid-models），
// 因此本地必须握有完整列表：以 status.json 的 paidModels 为准，保存成功后立刻用
// 自己算出的下一版覆盖，避免等下一帧快照的这段时间里界面显示旧状态。
const paid = ref([...props.paidModels])
watch(() => props.paidModels, next => { paid.value = [...(next || [])] })
const paidDrafts = ref(Object.fromEntries(PLATFORMS.map(p => [p.id, { model: '', feedback: null }])))
const paidSaving = ref({})
// 每个平台只展示自己命名空间下的条目；跨平台条目仍由对应卡片管理。
function paidFor(provider) {
  return paid.value.filter(id => id.startsWith(`${provider.id}/`))
}

// 付费白名单的默认候选：取最近一次发现里**本平台**已知的模型（免费 + 已放行的付费），
// 按模型 key 去重排序。典型对象（尚未放行的付费模型）本来就不会被发现，所以这里只是
// 可选提示，输入框必须继续接受任意手动输入；模型 key 里可能还有 `/`（如 OpenRouter），
// 只剥掉开头的平台命名空间，不按斜杠再切。
const paidSuggestions = computed(() => {
  const namespace = openProvider.value?.id
  if (!namespace) return []
  const prefix = `${namespace}/`
  const seen = new Map()
  for (const model of props.models) {
    const id = typeof model?.id === 'string' ? model.id : ''
    if (!id.startsWith(prefix)) continue
    const key = id.slice(prefix.length)
    if (!key || seen.has(key)) continue
    seen.set(key, typeof model.name === 'string' ? model.name : '')
  }
  return [...seen.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([key, name]) => ({ key, name }))
})

// 「重新应用」的成功反馈：成功 5 秒后自动消隐，失败走上方 error 展示。
const applyFeedback = ref(null)
let applyFeedbackTimer = 0
function noteSuccess(name, result) {
  const text = successMessage(name, result)
  if (!text) return
  applyFeedback.value = { error: false, text }
  if (applyFeedbackTimer) clearTimeout(applyFeedbackTimer)
  applyFeedbackTimer = setTimeout(() => {
    applyFeedbackTimer = 0
    applyFeedback.value = null
  }, 5000)
}

// 兜底手动应用：等价于「模型与服务」页的「读取免费模型」；成功与否都弹回反馈区。
async function applyAll() {
  // 节流：一次应用 = 重启隔离运行时 + 重新发现模型，动辄几十秒。两个入口同时触发就是两次
  // 重启，后一轮会把前一轮的模型结果覆盖掉，按钮上的 spinner 也说不清在等哪一次。
  if (applyingBusy.value) {
    error.value = '另一次应用还在进行中，请等它结束。'
    return
  }
  applying.value.__all = true
  const response = await action('refresh')
  applying.value.__all = false
  if (response.ok) {
    // 成功反馈：文案与「模型与服务」页同源（ops.js），不各自造句子；5 秒后自动消隐。
    error.value = null
    noteSuccess('refresh', response.result)
  } else {
    error.value = response.error
  }
}

// 用响应快照按 id 合并「已配置」徽章；快照缺失/平台集合不一致时以本地清单为准，
// 卡片数量与形态永远不会因核心返回异常而缩水或消失。
function mergeStatuses(snapshot) {
  const byId = new Map((snapshot?.providers || []).map(item => [item.id, item]))
  providers.value = providers.value.map(provider => {
    const known = byId.get(provider.id)
    return known ? { ...provider, configured: known.configured === true } : provider
  })
  return (snapshot?.providers || []).some(item => item.configured === true)
}

// 错误提示的克制原则：核心没跑 + 用户还没配过任何 Key 时，连接失败是**预期状态**
// （provider-status 只在核心进程内读盘，核心未启动自然 refused），弹一条 os error 61
// 只会吓到第一次打开平台页的用户。此时徽章保持「未配置」即可；一旦有平台配置过
// （状态真实与否开始影响用户决策）或用户点了刷新/保存，失败才值得显示。
async function load({ silent = false } = {}) {
  // 同步防抖：双击「刷新」在重渲染前会穿透模板 disabled，叠发两次 provider-status。
  if (loading.value) return
  loading.value = true
  if (!silent) error.value = null
  const response = await action('provider-status')
  if (response.ok) {
    error.value = null
    mergeStatuses(response.result)
  } else if (silent) {
    // 静默探测：失败不打扰；若快照拿不到就维持全部「未配置」，等下一次显式刷新。
    error.value = null
  } else {
    error.value = response.error
  }
  loading.value = false
}

// 定向应用：只重启隔离运行时以载入新 Key、只重取并重检**这一个平台**的模型，
// 其余平台的目录与探测结果原样留着（2026-10-09 用户裁定：改一家 Key 不该把全部模型
// 重新获取重新检测一遍）。走既有 /admin/probe 的 `providers` 形态，不新增动作。
// 该端点无论成败都是 2xx（202），拒绝信息藏在 body 里，必须自己拆开：
// `{ error: { message } }` = 核心正在忙别的（不叠第二轮），`{ started: false, message }`
// = 探测闸门拒绝，两者都不能当成「已应用」报给用户。
async function applyScoped(provider, appliedLabel = 'Key 已写入') {
  applying.value[provider.id] = true
  const response = await action('probe', { providers: [provider.id] })
  applying.value[provider.id] = false
  if (!response.ok) return { error: true, text: `应用失败：${response.error}。` }
  const refusal = response.result?.error?.message
    || (response.result?.started === false ? response.result?.message : '')
  if (refusal) return { error: true, text: `${appliedLabel} ✓，但这一家没能重新检测：${refusal}。` }
  return { error: false, text: '' }
}

async function save(provider) {
  const draft = drafts.value[provider.id]
  const key = (draft.key || '').trim()
  if (!key) {
    draft.feedback = { error: true, text: '请先粘贴平台 Key' }
    return
  }
  // 节流：本卡片保存后会自动触发一次应用，别的应用还在空中就不能再叠一轮（见 applyAll）。
  if (applyingBusy.value) {
    draft.feedback = { error: true, text: '另一次应用还在进行中，请等它结束后再保存。' }
    return
  }
  saving.value[provider.id] = true
  draft.feedback = null
  const response = await action('set-provider-key', { provider: provider.id, apiKey: key })
  saving.value[provider.id] = false
  if (!response.ok) {
    draft.feedback = { error: true, text: `保存失败：${response.error}` }
    return
  }
  // ── 到这里 Key 已确定落盘（providers.json 已写入）────────────────────────
  // 徽章立即翻转：响应快照优先，快照缺失时本地翻——用户此刻最需要的就是「保存成功了」。
  mergeStatuses(response.result)
  if (!response.result?.providers) {
    providers.value = providers.value.map(p => p.id === provider.id ? { ...p, configured: true } : p)
  }
  // 把刚写入的 Key 回填成掩码显示：弹窗里始终看得见「已保存的 Key」，
  // 无需为了确认再点一次眼睛（值本来就在本组件内存里）。
  draft.key = key
  revealed.value[provider.id] = true
  revealing.value[provider.id] = false
  draft.feedback = { error: false, text: `已保存 ✓ 正在只重新检测 ${provider.label} 的模型…` }
  const applied = await applyScoped(provider)
  // 失败不回滚已保存的 Key：提示给出重试入口。
  draft.feedback = applied.error
    ? { error: true, text: `${applied.text}可点下方「读取免费模型（重新应用）」重试。` }
    : { error: false, text: `已保存并应用 ✓ 只重新检测 ${provider.label} 的模型，其余平台的目录与结果没动。切到「模型与服务」查看进度。` }
}

async function clear(provider) {
  // 节流：与 save 同一条闸门——清除后同样会自动跑一轮应用。
  if (applyingBusy.value) {
    drafts.value[provider.id].feedback = { error: true, text: '另一次应用还在进行中，请等它结束后再清除。' }
    return
  }
  clearing.value[provider.id] = true
  const draft = drafts.value[provider.id]
  const response = await action('clear-provider-key', { provider: provider.id })
  clearing.value[provider.id] = false
  if (!response.ok) {
    draft.feedback = { error: true, text: `清除失败：${response.error}` }
    return
  }
  mergeStatuses(response.result)
  if (!response.result?.providers) {
    providers.value = providers.value.map(p => p.id === provider.id ? { ...p, configured: false } : p)
  }
  draft.key = ''
  revealed.value[provider.id] = false
  revealing.value[provider.id] = false
  draft.feedback = { error: false, text: `已清除 ✓ 正在只重取 ${provider.label} 的模型…` }
  // 与保存同一条定向路径：重启子进程时该平台的声明段不再注入，发现结果里就没有它了，
  // 其余平台的条目与探测结果一概不动（这一家现在没有模型可检，核心会直接按现有目录重发配置）。
  const applied = await applyScoped(provider)
  draft.feedback = applied.error
    ? { error: true, text: `${applied.text}可点下方「读取免费模型（重新应用）」重试。` }
    : { error: false, text: `已清除并应用 ✓ ${provider.label} 的模型已从「模型与服务」中移除，其余平台不受影响。` }
}

// 白名单写入：核心按全量替换（空数组即清空），所以每次都要把**整张表**发过去。
// 保存成功后立刻触发本平台的定向重新检测：新放行的付费模型在下一轮发现里才会出现，
// 不重启整个应用、也不碰其余平台的目录与探测结果。
async function writePaid(provider, next, successText) {
  const draft = paidDrafts.value[provider.id]
  if (applyingBusy.value) {
    draft.feedback = { error: true, text: '另一次应用还在进行中，请等它结束后再改白名单。' }
    return
  }
  paidSaving.value[provider.id] = true
  draft.feedback = null
  const response = await action('set-paid-models', { models: next })
  paidSaving.value[provider.id] = false
  if (!response.ok) {
    draft.feedback = { error: true, text: `白名单保存失败：${response.error}` }
    return
  }
  // 核心回的是同一份列表（已排序）；缺失时用本地算出的值，界面不等下一帧快照。
  paid.value = [...(response.result?.paidModels || next)]
  draft.feedback = { error: false, text: `${successText} 正在只重新检测 ${provider.label} 的模型…` }
  const applied = await applyScoped(provider, '白名单已保存')
  draft.feedback = applied.error
    ? { error: true, text: `${applied.text}可点下方「读取免费模型（重新应用）」重试。` }
    : { error: false, text: `${successText} 已应用 ✓ 只重新检测 ${provider.label}，其余平台不受影响。切到「模型与服务」查看。` }
}

// 输入既可以是完整 `provider/model`，也可以只写模型 id（按当前卡片的平台补前缀）。
// 模型 key 自身允许带 `/`（如 `Qwen/Qwen3-8B`、OpenRouter 的 `meta-llama/...`），所以只有
// **首段命中别家注册表平台 id** 才当跨平台输入拒绝——否则会把正常模型 key 误判成前缀。
function normalizePaidInput(provider, raw) {
  const typed = (raw || '').trim()
  if (!typed) return { error: '请选择或输入模型 id（例如 hy4），也可以填完整的 provider/model。' }
  const ownPrefix = `${provider.id}/`
  if (typed.startsWith(ownPrefix)) {
    const key = typed.slice(ownPrefix.length).trim()
    if (!key) return { error: '模型 id 不能为空：标识形如 provider/model。' }
    return { error: null, id: `${provider.id}/${key}`, key }
  }
  const slash = typed.indexOf('/')
  const head = slash < 0 ? '' : typed.slice(0, slash)
  if (head && PLATFORMS.some(p => p.id === head)) {
    return { error: `这里只能添加 ${provider.label} 的模型：前缀必须是 ${provider.id}/。` }
  }
  return { error: null, id: `${provider.id}/${typed}`, key: typed }
}

async function addPaidModel(provider) {
  const draft = paidDrafts.value[provider.id]
  const parsed = normalizePaidInput(provider, draft.model)
  if (parsed.error) {
    draft.feedback = { error: true, text: parsed.error }
    return
  }
  if (paid.value.includes(parsed.id)) {
    draft.feedback = { error: true, text: `${parsed.id} 已在白名单中。` }
    return
  }
  if (paid.value.length >= 64) {
    draft.feedback = { error: true, text: '白名单最多 64 条，请先移除不再使用的条目。' }
    return
  }
  draft.model = ''
  await writePaid(provider, [...paid.value, parsed.id].sort(), `已加入白名单 ✓（${parsed.id}）`)
}

async function removePaidModel(provider, id) {
  if (!paid.value.includes(id)) return
  await writePaid(provider, paid.value.filter(item => item !== id), `已移出白名单 ✓（${id}）`)
}

onMounted(() => {
  window.addEventListener('keydown', onWindowKeydown)
  // 首次进入页面用静默探测：核心未运行时（最典型 = 全新安装还没配过 Key）不弹连接失败。
  // 之后任何显式动作（刷新按钮 / 保存 / 清除后的自动应用失败）都走非静默路径，照常报错。
  load({ silent: true })
})

onUnmounted(() => {
  window.removeEventListener('keydown', onWindowKeydown)
  // 切走视图时这个组件就没了：残留定时器会在 5 秒后往已卸载的 ref 上写状态。
  if (applyFeedbackTimer) clearTimeout(applyFeedbackTimer)
})
</script>

<template>
  <section class="view" aria-label="平台">
    <header>
      <div>
        <h2>平台</h2>
        <p class="subtitle">配置自有平台 Key，免费模型随注册表扩展；付费模型需逐条加入白名单。每家平台点「管理配置」进入弹窗，Key 只保存在本机数据目录。</p>
      </div>
      <button id="reload-providers" :disabled="loading" @click="load()">
        <span v-if="loading" class="spinner" />刷新
      </button>
    </header>

    <div v-if="error" class="notice error" role="alert">
      <svg class="notice-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.4" />
        <path d="M12 7.6v5.6M12 16.4v.4" />
      </svg>
      <span class="notice-text">状态刷新失败（{{ error }}）：仍可填写并保存，待核心运行后点「刷新」同步状态。</span>
      <button type="button" class="ghost" :disabled="loading" @click="load()">
        <span v-if="loading" class="spinner" />重试
      </button>
    </div>

    <ul class="providers">
      <li v-for="(provider, i) in providers" :key="provider.id" class="provider" :style="{ '--i': i }">
        <div class="provider-head">
          <span class="provider-label">{{ provider.label }}<span class="provider-id">（{{ provider.id }}）</span></span>
          <div class="provider-tags">
            <span v-if="paidFor(provider).length" class="badge paid">{{ paidFor(provider).length }} 个付费已放行</span>
            <span class="badge" :class="provider.configured ? 'ok' : 'off'">
              {{ provider.configured ? '已配置' : '未配置' }}
            </span>
            <button type="button" class="manage-btn primary" aria-haspopup="dialog" @click="openDialog(provider, $event)">
              管理配置
            </button>
          </div>
        </div>
      </li>
    </ul>

    <!-- 平台配置弹窗：申请引导 / Key 管理 / 付费白名单三块同屏，列表只留一行摘要 -->
    <div v-if="openProvider" class="mask" @click.self="closeDialog">
      <div
        ref="dialogPanel"
        class="dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="providerDialogTitle"
        tabindex="-1"
      >
        <header class="dialog-head">
          <h3 id="providerDialogTitle">
            {{ openProvider.label }}<span class="provider-id">（{{ openProvider.id }}）</span>
            <span class="badge" :class="openProvider.configured ? 'ok' : 'off'">
              {{ openProvider.configured ? '已配置' : '未配置' }}
            </span>
          </h3>
          <button type="button" class="dialog-close ghost" aria-label="关闭平台配置" @click="closeDialog">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" aria-hidden="true">
              <path d="M7 7l10 10M17 7L7 17" />
            </svg>
          </button>
        </header>

        <div class="dialog-body">
          <div class="guide">
            <p class="guide-title">申请 Key</p>
            <ol class="guide-steps">
              <li v-for="(step, i) in PLATFORM_GUIDES[openProvider.id]?.steps || []" :key="i">{{ step }}</li>
            </ol>
            <div class="guide-actions">
              <button type="button" class="guide-open" :disabled="opening === openProvider.id" @click="openGuide(openProvider)">
                <span v-if="opening === openProvider.id" class="spinner" />{{ opening === openProvider.id ? '正在打开…' : '打开官方申请页 ↗' }}
              </button>
              <!-- 单击全选整段网址（user-select: all 选中 DOM 全文，即使视觉上被省略号截断），
                   作为「打开浏览器失败」时的手动复制兜底 -->
              <span class="guide-url" :title="PLATFORM_GUIDES[openProvider.id]?.url">{{ PLATFORM_GUIDES[openProvider.id]?.url }}</span>
            </div>
            <p v-if="openError" class="feedback error" role="alert">打开浏览器失败（{{ openError }}）。单击上方网址可全选复制，再到浏览器打开。</p>
          </div>

          <section class="section">
            <h4 class="section-title">平台 Key</h4>
            <form class="key-form" @submit.prevent="save(openProvider)">
              <div class="key-input-wrap">
                <input
                  v-model="drafts[openProvider.id].key"
                  :type="revealing[openProvider.id] ? 'text' : 'password'"
                  autocomplete="off"
                  spellcheck="false"
                  :placeholder="keyPlaceholder(openProvider)"
                  :aria-label="`${openProvider.label} 的平台 Key`"
                  :disabled="busy || saving[openProvider.id]"
                  @input="revealed[openProvider.id] = false"
                >
                <button
                  type="button"
                  class="reveal-btn"
                  :aria-pressed="!!revealing[openProvider.id]"
                  :aria-label="revealing[openProvider.id] ? `隐藏 ${openProvider.label} 的 Key 明文` : `显示 ${openProvider.label} 的 Key 明文`"
                  :title="revealing[openProvider.id] ? '隐藏明文' : (openProvider.configured ? '查看已保存的 Key' : '显示明文')"
                  :disabled="saving[openProvider.id]"
                  @pointerdown="onEyePress(openProvider, $event)"
                  @click="onEyeClick(openProvider)"
                >
                  <!-- 眼睛图标：闭眼 = 当前隐藏（点击显示）；睁眼 = 当前明文（点击隐藏）。
                       取回中的状态不进按钮：图标与尺寸保持不变，避免点击时整块跳动。
                       内联 SVG 继承 currentColor，CSP 无外部资源 -->
                  <svg v-if="!revealing[openProvider.id]" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
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
              <!-- 保存后还有一轮自动应用（重启模型子进程 + 重新发现，几十秒）：这一段忙态必须落在
                   按钮上，否则用户只看到 feedback 那行字停在「正在应用…」，像卡住了 -->
              <button
                type="submit"
                class="primary"
                :disabled="busy || saving[openProvider.id] || applyingBusy || revealed[openProvider.id] || !drafts[openProvider.id].key.trim()"
                :aria-busy="String(!!(saving[openProvider.id] || applying[openProvider.id]))"
              >
                <span v-if="saving[openProvider.id] || applying[openProvider.id]" class="spinner" />{{ applying[openProvider.id] ? '应用中' : '保存' }}
              </button>
              <button
                v-if="openProvider.configured"
                type="button"
                class="ghost"
                :disabled="busy || clearing[openProvider.id] || applyingBusy"
                :aria-busy="String(!!(clearing[openProvider.id] || applying[openProvider.id]))"
                @click="clear(openProvider)"
              >
                <span v-if="clearing[openProvider.id] || applying[openProvider.id]" class="spinner" />{{ applying[openProvider.id] ? '应用中' : '清除' }}
              </button>
            </form>
            <p class="key-hint">已保存的 Key 会在打开弹窗时由本机核心取回并掩码显示，点眼睛可临时查看明文；值只留在当前窗口，关闭弹窗即清出。</p>
            <p
              v-if="drafts[openProvider.id].feedback"
              class="feedback"
              :class="{ error: drafts[openProvider.id].feedback.error }"
              role="status"
            >
              {{ drafts[openProvider.id].feedback.text }}
            </p>
          </section>

          <!-- 付费白名单：免费发现按标价全 0 过滤，付费模型只能由用户逐条显式放行。 -->
          <section class="section paid">
            <div class="paid-head">
              <h4 class="section-title">付费模型白名单</h4>
              <span v-if="paidFor(openProvider).length" class="badge paid">{{ paidFor(openProvider).length }} 个已放行</span>
            </div>
            <p v-if="!openProvider.configured" class="paid-hint">先保存 Key，放行的模型才会被发现。</p>
            <ul v-if="paidFor(openProvider).length" class="paid-list">
              <li v-for="id in paidFor(openProvider)" :key="id">
                <code>{{ id }}</code>
                <button
                  type="button"
                  class="ghost paid-remove"
                  :disabled="busy || paidSaving[openProvider.id] || applyingBusy"
                  :aria-busy="String(!!(paidSaving[openProvider.id] || applying[openProvider.id]))"
                  @click="removePaidModel(openProvider, id)"
                >
                  <span v-if="paidSaving[openProvider.id] || applying[openProvider.id]" class="spinner" />{{ applying[openProvider.id] ? '应用中' : '移出' }}
                </button>
              </li>
            </ul>
            <form class="paid-form" @submit.prevent="addPaidModel(openProvider)">
              <!-- 可编辑选择器：datalist 给出本平台已知模型，选中即填入模型 key；
                   付费白名单的典型对象尚未被发现、不会出现在候选里，所以仍可手动输入 -->
              <input
                v-model="paidDrafts[openProvider.id].model"
                type="text"
                :list="`paid-models-${openProvider.id}`"
                autocomplete="off"
                spellcheck="false"
                :placeholder="paidSuggestions.length ? '选择已知模型，或手动输入模型 id' : `模型 id，例如 hy4 或 ${openProvider.id}/hy4`"
                :aria-label="`${openProvider.label} 的付费模型 id`"
                :disabled="busy || !openProvider.configured || paidSaving[openProvider.id] || applyingBusy"
              >
              <datalist :id="`paid-models-${openProvider.id}`">
                <option v-for="item in paidSuggestions" :key="item.key" :value="item.key">{{ item.name }}</option>
              </datalist>
              <button
                type="submit"
                class="ghost"
                :disabled="busy || !openProvider.configured || paidSaving[openProvider.id] || applyingBusy || !paidDrafts[openProvider.id].model.trim()"
                :aria-busy="String(!!(paidSaving[openProvider.id] || applying[openProvider.id]))"
              >
                <span v-if="paidSaving[openProvider.id] || applying[openProvider.id]" class="spinner" />{{ applying[openProvider.id] ? '应用中' : '加入白名单' }}
              </button>
            </form>
            <p v-if="paidSuggestions.length" class="paid-hint">
              候选来自最近一次发现（本平台已知 {{ paidSuggestions.length }} 个模型，含免费与已放行）；不在列表里的模型可直接输入模型 id。
            </p>
            <p
              v-if="paidDrafts[openProvider.id].feedback"
              class="feedback"
              :class="{ error: paidDrafts[openProvider.id].feedback.error }"
              role="status"
            >
              {{ paidDrafts[openProvider.id].feedback.text }}
            </p>
          </section>
        </div>
      </div>
    </div>

    <div class="apply-note">
      <p>Key 保存或清除后<strong>自动应用</strong>：只重启隔离的模型子进程以载入新 Key（不动应用窗口），并<strong>只重新获取、重新检测这一个平台</strong>的模型，其余平台的目录与结果原样保留。若自动应用失败，可点下方按钮重试全量读取。</p>
      <button type="button" :disabled="busy || !!applyingBusy" @click="applyAll">
        <span v-if="busy || applyingBusy" class="spinner" />读取免费模型（重新应用）
      </button>
    </div>

    <p v-if="applyFeedback" class="feedback" :class="{ error: applyFeedback.error }" role="status">
      {{ applyFeedback.text }}
    </p>

    <p class="note">额度与可用性由各平台决定，本应用不缓存额度；探测与对话都会消耗平台侧额度（免费模型的额度为 0 标价，仍受平台限额约束）。模型发现只覆盖注册表内的平台；上游新增免费模型需随本应用更新出现。</p>
  </section>
</template>

<style scoped>
header { display: flex; justify-content: space-between; gap: var(--sp-3); align-items: center; }
.subtitle { margin: 0; color: var(--muted-strong); font-size: var(--fs-sm); }
.providers { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
/* 一行一家平台：细节全部收进弹窗，列表只承担「哪家已配置、哪家有付费放行、去哪管理」 */
.provider {
  border: 1px solid var(--line);
  border-radius: var(--radius-m);
  background: var(--row);
  padding: 10px 14px;
  /* 平台行按顺序浮现：一次性压进九行会看不清从哪读起 */
  animation: enter-rise var(--dur-3) var(--ease-enter) backwards;
  animation-delay: calc(var(--i, 0) * var(--stagger-step));
}
.provider-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.provider-label { font-size: 14px; font-weight: 600; }
/* 注册表 id：弱化为括注 */
.provider-id { font-size: 11px; font-weight: 400; color: var(--muted-strong); font-family: ui-monospace, monospace; }
.provider-tags { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.manage-btn { font-size: 12px; padding: 6px 12px; }
/* 已配置 / 未配置翻转时渐变，不硬切 */
.badge {
  font-size: 11px; font-weight: 500; white-space: nowrap;
  padding: 4px 9px; border-radius: 999px;
  transition: background-color var(--dur-2) var(--ease-standard),
              color var(--dur-2) var(--ease-standard);
}
.badge.ok { background: var(--green-bg); color: var(--green); }
.badge.off { background: var(--panel); color: var(--muted-strong); }
/* 付费放行 = 真实账单：用高危写入的语义色对，和「已配置」的绿色徽章分属两个色相 */
.badge.paid { background: var(--danger-bg); color: var(--danger); }

/* ── 弹窗 ─────────────────────────────────────────────────────────── */
.mask {
  position: fixed; inset: 0; z-index: 20;
  /* 45% 黑：与 AI 设置弹窗同一层遮罩，弹窗内容才是焦点 */
  background: rgb(0 0 0 / 45%);
  display: flex; align-items: center; justify-content: center;
  padding: var(--sp-5);
  animation: enter-fade var(--dur-2) var(--ease-standard);
}
.dialog {
  width: min(620px, 100%);
  max-height: 100%;
  display: flex; flex-direction: column;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius-l);
  box-shadow: var(--shadow-m);
  overflow: hidden;
  animation: enter-rise var(--dur-3) var(--ease-enter);
}
.dialog-head {
  display: flex; align-items: center; justify-content: space-between;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--line);
}
/* 弹窗标题用 h3 并挂 aria-labelledby：role=dialog 的名字由标题元素提供 */
.dialog-head h3 { margin: 0; font-size: var(--fs-base); font-weight: 600; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.dialog-close { width: 30px; height: 30px; flex-shrink: 0; display: flex; align-items: center; justify-content: center; padding: 0; }
.dialog-close > svg { width: 15px; height: 15px; }
.dialog-body { padding: var(--sp-4); overflow-y: auto; display: flex; flex-direction: column; gap: var(--sp-3); }
.section { display: flex; flex-direction: column; gap: 8px; }
.section-title { margin: 0; font-size: 12px; font-weight: 600; color: var(--muted-strong); }
.guide {
  border: 1px dashed var(--line);
  border-radius: var(--radius-s);
  padding: 10px 12px;
  background: color-mix(in srgb, var(--panel) 60%, transparent);
}
.guide-title { margin: 0 0 6px; font-size: 12px; font-weight: 600; color: var(--muted-strong); }
.guide-steps { margin: 0; padding-left: 18px; font-size: 12px; line-height: 1.8; color: var(--muted-strong); }
.guide-actions { display: flex; align-items: center; gap: 10px; margin-top: 8px; }
.guide-open {
  flex-shrink: 0;
  font-size: 12px;
  font-weight: 500;
  padding: 5px 12px;
  border-radius: 999px;
  border: 1px solid color-mix(in srgb, var(--green) 45%, transparent);
  background: var(--green-bg);
  color: var(--green);
  cursor: pointer;
  transition: opacity var(--dur-1) var(--ease-standard);
}
.guide-open:hover:not(:disabled) { opacity: 0.78; }
.guide-open:disabled { cursor: not-allowed; opacity: 0.5; }
.guide-open:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
/* 网址与按钮同行：占满剩余宽度、溢出省略；user-select: all 让单击即全选全文（复制兜底） */
.guide-url {
  margin: 0; flex: 1; min-width: 0;
  font-size: 11px; color: var(--muted-strong); font-family: ui-monospace, monospace;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  user-select: all; cursor: text;
}
.key-hint { margin: 0; font-size: 11px; line-height: 1.7; color: var(--muted-strong); }
.paid {
  border: 1px solid color-mix(in srgb, var(--danger) 30%, var(--line));
  border-radius: var(--radius-s);
  padding: 10px 12px;
}
.paid-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.paid-hint { margin: 0; font-size: 11px; color: var(--muted-strong); }
.paid-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 6px; }
.paid-list li { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.paid-list code {
  font-family: ui-monospace, monospace;
  font-size: 12px;
  color: var(--text);
  overflow-wrap: anywhere;
}
.paid-remove { font-size: 11px; padding: 4px 10px; }
.paid-form { display: flex; gap: 8px; flex-wrap: wrap; }
.paid-form input {
  flex: 1; min-width: 180px;
  font-size: 13px; padding: 7px 10px;
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--panel); color: var(--text);
}
.paid-form input:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.paid-form input:disabled { opacity: .5; cursor: not-allowed; }
.paid-form button { font-size: 12px; padding: 6px 12px; }
.key-form { display: flex; gap: 8px; flex-wrap: wrap; }
.key-input-wrap { position: relative; flex: 1; min-width: 200px; display: flex; }
.key-form input {
  flex: 1; min-width: 0; width: 100%;
  font-size: 13px; padding: 8px 44px 8px 10px; /* 右侧留出眼睛按钮位 */
  border: 1px solid var(--line); border-radius: var(--radius-s);
  background: var(--panel); color: var(--text);
}
.key-form input:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.key-form input:disabled { opacity: .5; cursor: not-allowed; }
.reveal-btn {
  position: absolute; right: 3px; top: 50%; transform: translateY(-50%);
  width: 34px; height: 34px;
  display: flex; align-items: center; justify-content: center;
  border: none; background: transparent; border-radius: var(--radius-s);
  color: var(--muted-strong); cursor: pointer;
  transition: color var(--dur-1) var(--ease-standard), background var(--dur-1) var(--ease-standard);
}
.reveal-btn:hover:not(:disabled) { color: var(--text); background: var(--nav-hover-bg); }
.reveal-btn:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); }
.reveal-btn:disabled { opacity: .4; cursor: not-allowed; }
.reveal-btn svg { width: 22px; height: 22px; }
/* 保存 / 清除的结果行是插入式出现的：淡入一次，避免弹窗高度「啪」地一跳 */
.feedback { margin: 0; font-size: 12px; color: var(--green); animation: enter-fade var(--dur-2) var(--ease-standard); }
.feedback.error { color: var(--badge-unavailable-fg); }
.apply-note {
  border: 1px solid var(--line); border-radius: var(--radius-m);
  background: var(--panel); padding: var(--sp-3) var(--sp-4);
  display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap;
}
.apply-note p { margin: 0; font-size: 12px; line-height: 1.7; color: var(--muted-strong); }
.note { margin: 0; font-size: 11px; color: var(--muted-strong); line-height: 1.7; }
.notice { display: flex; align-items: center; gap: 8px; }
.notice-icon { width: 16px; height: 16px; flex-shrink: 0; }
.notice-text { font-size: 13px; }
</style>
