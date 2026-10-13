// 会话持久化的纯函数层：内存态 ↔ 存储态。
// 只测不需要 IndexedDB 的部分（node 里没有 indexedDB，真实读写在 WebView 内跑）；
// 钉住的三件事：凭据不落盘、未完成回复不留待定态、坏数据读回来不炸。
import assert from 'node:assert/strict'
import test from 'node:test'
import {
  MAX_STORED_MESSAGES,
  clipDeep,
  fromStorageMessages,
  toStorageMessages,
} from './chatSession.js'

function userMessage(id, content) {
  return { id, role: 'user', content }
}

function assistantMessage(id, extra = {}) {
  return { id, role: 'assistant', content: '好的', pending: false, tools: [], ...extra }
}

test('toStorageMessages 只留 user / assistant，其余角色一律不进库', () => {
  const rows = toStorageMessages([
    userMessage('u1', '你好'),
    { id: 't1', role: 'tool', content: '{"ok":true}' },
    { id: 's1', role: 'system', content: '你是助手' },
    assistantMessage('a1'),
    null,
    'not-an-object',
  ])
  assert.deepEqual(rows.map(row => row.id), ['u1', 'a1'])
})

test('toStorageMessages 丢弃工具入参：平台 Key 明文绝不能落盘', () => {
  const rows = toStorageMessages([
    assistantMessage('a1', {
      tools: [{
        id: 'call-1',
        name: 'set_provider_key',
        args: { provider: 'zhipuai', apiKey: 'sk-THIS-IS-A-REAL-KEY-0123456789' },
        label: '保存平台 Key',
        write: true,
        danger: false,
        summary: '保存 智谱 的 Key（sk-T…6789）',
        preview: { 平台: '智谱', Key: 'sk-T…6789（32 字符）' },
        validation: { ok: true },
        status: 'done',
        result: { ok: true, result: {} },
      }],
    }),
  ])
  const serialized = JSON.stringify(rows)
  assert.ok(!serialized.includes('THIS-IS-A-REAL-KEY'), 'Key 明文不得出现在落盘内容里')
  const tool = rows[0].tools[0]
  assert.equal(tool.args, undefined)
  assert.equal(tool.name, 'set_provider_key')
  // preview 是 ai.js 算好的掩码字段，可以留着给界面显示
  assert.equal(tool.preview.平台, '智谱')
})

test('toStorageMessages 把未完成的回复落成终态：不留一个永远待定的气泡', () => {
  const [hasContent, empty] = toStorageMessages([
    assistantMessage('a1', { content: '已经写了一半', pending: true }),
    assistantMessage('a2', { content: '', pending: true }),
  ])
  assert.equal(hasContent.pending, false)
  assert.equal(hasContent.stopped, true)
  assert.equal(hasContent.partial, true)
  assert.equal(hasContent.failed, false)

  assert.equal(empty.pending, false)
  assert.equal(empty.stopped, false)
  assert.equal(empty.failed, true)
  assert.match(empty.error, /还没生成完/)
})

test('toStorageMessages 把执行中 / 待确认的工具卡片收成终态', () => {
  const rows = toStorageMessages([
    assistantMessage('a1', {
      tools: [
        { id: 'c1', name: 'read_log', status: 'running', validation: { ok: true }, result: null },
        { id: 'c2', name: 'restart_core', status: 'pending', validation: { ok: true }, result: null },
        { id: 'c3', name: 'get_status', status: 'running', validation: { ok: true }, result: { ok: true } },
        { id: 'c4', name: 'get_status', status: 'done', validation: { ok: true }, result: { ok: true } },
      ],
    }),
  ])
  assert.deepEqual(rows[0].tools.map(t => t.status), ['skipped', 'skipped', 'done', 'done'])
})

test('toStorageMessages 截断超长正文，并按上限只留最近的若干条', () => {
  const long = 'x'.repeat(40000)
  const rows = toStorageMessages([userMessage('u1', long)])
  assert.ok(rows[0].content.length < 40000)
  assert.match(rows[0].content, /已截断/)

  const many = Array.from({ length: MAX_STORED_MESSAGES + 30 }, (_, index) => userMessage(`m${index}`, `第 ${index} 条`))
  const kept = toStorageMessages(many)
  assert.equal(kept.length, MAX_STORED_MESSAGES)
  assert.equal(kept[0].content, `第 ${30} 条`)
})

test('工具返回被递归裁剪：长字符串截断、长数组只留前若干项、深度封顶', () => {
  const rows = toStorageMessages([
    assistantMessage('a1', {
      tools: [{
        id: 'c1',
        name: 'list_models',
        status: 'done',
        validation: { ok: true },
        result: { ok: true, result: { count: 300, models: Array.from({ length: 300 }, (_, i) => `模型${i}`), text: 'y'.repeat(9000) } },
      }],
    }),
  ])
  const payload = rows[0].tools[0].result.result
  assert.ok(payload.models.length < 300, '长数组必须被截断')
  assert.ok(String(payload.text).length < 9000, '长字符串必须被截断')
  assert.equal(payload.count, 300)
})

test('clipDeep 保留标量、把不可序列化的形状压平', () => {
  assert.equal(clipDeep(42), 42)
  assert.equal(clipDeep(true), true)
  assert.equal(clipDeep(null), null)
  assert.equal(clipDeep(undefined), null)
  assert.deepEqual(clipDeep([1, 'two']), [1, 'two'])
  // 深度到顶：不再展开，压成空串而不是把整棵子树搬进库里
  const deep = { a: { b: { c: { d: { e: '太深了' } } } } }
  assert.equal(JSON.stringify(clipDeep(deep)).includes('太深了'), false)
})

test('fromStorageMessages 过滤坏数据并补齐界面要用的字段', () => {
  const restored = fromStorageMessages([
    { id: 'u1', role: 'user', content: '问题' },
    { id: 'x1', role: 'tool', content: '协议消息' },
    { role: 'assistant', content: 123 },
    null,
    'garbage',
    {
      id: 'a1',
      role: 'assistant',
      content: '回答',
      tools: [{ id: 'c1', name: 'read_log', status: 'weird', validation: { ok: false, error: '参数不对' } }],
    },
  ])
  assert.deepEqual(restored.map(m => m.role), ['user', 'assistant', 'assistant'])
  assert.equal(restored[1].content, '')
  assert.equal(restored[2].content, '回答')
  const tool = restored[2].tools[0]
  assert.equal(tool.status, 'done') // 未知状态回落成已结束
  assert.equal(tool.args !== undefined, true) // 界面读到的工具条目始终有 args 形状
  assert.equal(tool.validation.ok, false)
  assert.equal(restored[0].pending, false)
})

test('往返稳定：读回来的内容再落一次盘不会变样', () => {
  const rows = toStorageMessages([
    userMessage('u1', '帮我看看状态'),
    assistantMessage('a1', {
      content: '**当前**：就绪',
      tools: [{
        id: 'c1',
        name: 'get_status',
        label: '读取服务状态',
        write: false,
        danger: false,
        summary: '读取服务状态',
        preview: {},
        validation: { ok: true },
        status: 'done',
        result: { ok: true, result: { phase: 'ready' } },
      }],
    }),
  ])
  assert.deepEqual(toStorageMessages(fromStorageMessages(rows)), rows)
})
