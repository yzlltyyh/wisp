/**
 * 操作注册表的不变量：id 唯一；文案 key 在两种语言里都有；菜单 / 提示条 / 确认条上显示的快捷键就是绑定的那个键
 * （按下它执行的正是同一个操作）；同一键位层里没有两个操作抢同一个组合，也不占用外壳的键；每个界面列出的项都能
 * 解析（有标签、图标，执行的就是它自己）。
 */
import { describe, expect, it } from 'vitest'
import type { ClipInspect, ClipItemMeta, ClipKind, OsKind } from '../../../shared/ipc/types'
import { t } from '../../../shared/i18n/i18n'
import { canonicalCombo, parseCombo } from '../../../shared/keys/combo'
import {
  ACTIONS,
  actionEntries,
  applies,
  displayCombos,
  hintItems,
  KEY_SCOPES,
  keyBindings,
  menuEntries,
  subPage,
  type ActionCtx,
  type ClipAction,
  type ClipKeyLayer,
  type KeySpec,
  type MenuSurface,
} from '../actions'
import { CLIPBOARD_MESSAGES, loadClipboardMessages } from '../i18n'
import { ENTITY_SPECS, TRANSFORM_SPECS } from '../specs'
import { initialState } from '../store'
import { meta, NOW, payload } from './helpers'

loadClipboardMessages()

const OSES: readonly OsKind[] = ['macos', 'windows', 'linux']
const LAYERS = Object.keys(KEY_SCOPES) as ClipKeyLayer[]
const SURFACES: readonly MenuSurface[] = [
  'context',
  'command',
  'more',
  'preview_tools',
  'preview_actions',
  'fragment',
  'toolbar',
]
const KINDS: readonly ClipKind[] = ['text', 'rich_text', 'code', 'link', 'color', 'image', 'files']

/** 各种条目：每种类型 × 收藏与否，外加文件都已不存在的文件条目 */
function targets(): (ClipItemMeta | null)[] {
  const out: (ClipItemMeta | null)[] = [null]
  let id = 1
  for (const kind of KINDS) {
    for (const pinned of [false, true]) {
      out.push(
        meta(id++, {
          kind,
          pinned,
          files: kind === 'files' ? [{ path: '/a', name: 'a', exists: true, size: 1 }] : null,
        }),
      )
    }
  }
  out.push(
    meta(id, { kind: 'files', files: [{ path: '/b', name: 'b', exists: false, size: null }] }),
  )
  return out
}

/** 检视结果：每种片段一个、每种变换都适用 */
const INSPECT: ClipInspect = {
  entities: (Object.keys(ENTITY_SPECS) as (keyof typeof ENTITY_SPECS)[]).map((kind, i) => ({
    kind,
    value: `v${i}`,
    start: i * 4,
    end: i * 4 + 2,
    sensitive: kind === 'id_card' || kind === 'bank_card',
  })),
  transforms: Object.keys(TRANSFORM_SPECS) as (keyof typeof TRANSFORM_SPECS)[],
}

/**
 * 上下文的各种组合：平台 × 条目 × 暂停与否 × 预览是否提供本地状态；另外每个条目各有一份「已检视（芯片条有键盘焦点）」
 * 与「多选（含收藏）」的上下文
 */
function contexts(os: OsKind): ActionCtx[] {
  const out: ActionCtx[] = []
  for (const target of targets()) {
    for (const paused of [false, true]) {
      const s = { ...initialState(payload(target ? [target] : []), NOW), paused }
      const base: ActionCtx = {
        os,
        host: 'panel',
        caps: { float: true, queueUnavailable: null, escapeCloses: true },
        onTop: null,
        source: 'context',
        target,
        marked: [],
        s,
        repeat: false,
      }
      out.push(base, { ...base, preview: { mono: true, setMono: () => {} } })
      // 悬浮剪贴板：置顶开着 / 关着、连续粘贴进行中
      const floating: ActionCtx = {
        ...base,
        host: 'pinned',
        caps: { float: false, queueUnavailable: null, escapeCloses: false },
        onTop: !paused,
        s: { ...s, queue: paused ? null : { active: true, index: 1, total: 3 } },
      }
      out.push(floating)
      if (!target || paused) continue
      out.push({ ...base, s: { ...s, inspect: { id: target.id, result: INSPECT }, chip: 0 } })
      const marks = [target, meta(99, { pinned: !target.pinned })]
      out.push({ ...base, marked: marks, s: { ...s, marks } })
      out.push({ ...floating, marked: marks, s: { ...floating.s, marks } })
      // 当前会话不支持连续粘贴（Wayland）：⌘↩ 仍是合并复制，菜单里的连续粘贴置灰
      out.push({
        ...base,
        caps: {
          float: true,
          queueUnavailable: 'errors.platform.wayland_input_hooks',
          escapeCloses: true,
        },
        marked: marks,
        s: { ...s, marks },
      })
    }
  }
  return out
}

/** 按 `clipboard.a.b` 在原始文案里查找（两种语言各查一次） */
function hasMessage(locale: 'zh-CN' | 'en-US', key: string): boolean {
  const parts = key.split('.')
  if (parts.shift() !== 'clipboard') return false
  let node: unknown = CLIPBOARD_MESSAGES[locale]
  for (const p of parts) {
    if (typeof node !== 'object' || node === null || !(p in node)) return false
    node = (node as Record<string, unknown>)[p]
  }
  return typeof node === 'string'
}

/** 记录执行了哪个操作（不真的执行） */
function recorder() {
  const calls: { id: string; arg: unknown }[] = []
  const exec = (a: ClipAction<unknown>, _c: ActionCtx, arg?: unknown) => {
    calls.push({ id: a.id, arg })
    return true
  }
  return { calls, exec }
}

/** 组合串 → 能让键位表匹配的按键事件（只用到 repeat；匹配由下面按规范串比较完成） */
const press = { repeat: false } as KeyboardEvent

/** 在某一层按下组合：返回执行的操作 id（没有绑定时为 null） */
function pressIn(layer: ClipKeyLayer, combo: string, c: ActionCtx): string | null {
  const { calls, exec } = recorder()
  const bindings = keyBindings(layer, c.os, c.host, () => c, exec)
  const b = bindings.find((x) => canonicalCombo(x.combo, c.os) === canonicalCombo(combo, c.os))
  if (!b) return null
  const ctx = { inInput: true, target: null }
  if (b.when && !b.when(ctx)) return null
  b.run(press, ctx)
  return calls[0]?.id ?? null
}

const boundCache = new Map<string, string[]>()

/** 一层里绑定的全部组合（规范串） */
function bound(layer: ClipKeyLayer, os: OsKind): string[] {
  const key = `${os}:${layer}`
  let combos = boundCache.get(key)
  if (!combos) {
    const c = contexts(os)[0] as ActionCtx
    combos = keyBindings(
      layer,
      os,
      'panel',
      () => c,
      () => true,
    ).map((b) => canonicalCombo(b.combo, os))
    boundCache.set(key, combos)
  }
  return combos
}

/** 声明为外壳全局层所有的组合（只显示、不绑定） */
function shellCombos(os: OsKind): Set<string> {
  const out = new Set<string>()
  for (const a of ACTIONS) {
    for (const k of a.keys ?? []) {
      if (k.owner !== 'shell') continue
      const combo = typeof k.combo === 'function' ? k.combo(os) : k.combo
      if (combo) out.add(canonicalCombo(combo, os))
    }
  }
  return out
}

describe('操作注册表', () => {
  it('id 唯一；每个快捷键所在的层都有对应的键盘作用域', () => {
    const ids = ACTIONS.map((a) => a.id)
    expect(new Set(ids).size).toBe(ids.length)
    for (const a of ACTIONS) {
      for (const k of a.keys ?? []) {
        for (const layer of k.layers ?? ['list']) expect(LAYERS).toContain(layer)
      }
      for (const h of Object.values(a.hints ?? {})) expect(LAYERS).toContain(h?.layer ?? 'list')
    }
  })

  it('标签与提示文案的 key 在 zh-CN 与 en-US 里都有', () => {
    const keys = new Set<string>()
    for (const os of OSES) {
      for (const c of contexts(os)) {
        for (const a of ACTIONS) if (a.label) keys.add(a.label(c))
      }
    }
    for (const os of OSES) {
      for (const c of contexts(os)) {
        for (const a of ACTIONS) {
          for (const h of Object.values(a.hints ?? {})) {
            if (h) keys.add(typeof h.label === 'function' ? h.label(c) : h.label)
          }
        }
      }
    }
    expect(keys.size).toBeGreaterThan(20)
    for (const key of keys) {
      expect(hasMessage('zh-CN', key), `zh-CN 缺少 ${key}`).toBe(true)
      expect(hasMessage('en-US', key), `en-US 缺少 ${key}`).toBe(true)
    }
  })

  it('显示的快捷键就是绑定的键：在同一层按下它，执行的正是这个操作（外壳的键只显示）', () => {
    for (const os of OSES) {
      const shell = shellCombos(os)
      for (const c of contexts(os)) {
        for (const a of ACTIONS) {
          for (const layer of LAYERS) {
            for (const combo of displayCombos(a, c, layer)) {
              const key = canonicalCombo(combo, os)
              if (shell.has(key)) continue
              expect(bound(layer, os), `${os} ${layer} ${a.id} 显示 ${combo} 却没有绑定`).toContain(
                key,
              )
              // 操作不适用于这个上下文时按键拦下但不执行（例如列表为空时的 ⌘P）
              if (!applies(a, c)) continue
              expect(pressIn(layer, combo, c), `${os} ${layer} 按 ${combo}`).toBe(a.id)
            }
          }
        }
        // 菜单的快捷键列：就是这个操作在视图层第一个可显示的键（上面已核对按下它执行的是同一个操作）
        for (const surface of SURFACES) {
          for (const e of actionEntries(surface, c, t, () => {})) {
            if (!e.shortcut) continue
            const a = ACTIONS.find((x) => x.id === e.id)
            expect(a && displayCombos(a, c)[0], `${surface} ${e.id}`).toBe(e.shortcut)
          }
        }
        // 提示条：按提示里的键（对选中的文本条目）执行的是提示的那个操作
        if (c.target?.kind !== 'text' || c.target.pinned) continue
        for (const mode of ['list', 'preview'] as const) {
          for (const h of hintItems(mode, { ...c, target: null }, t)) {
            const layer = ACTIONS.find((a) => a.id === h.id)?.hints?.[mode]?.layer ?? 'list'
            expect(h.combos.length, `${mode} 提示 ${h.id} 没有按键`).toBeGreaterThan(0)
            for (const combo of h.combos) expect(pressIn(layer, combo, c), combo).toBe(h.id)
          }
        }
      }
    }
  })

  it('同一键位层里没有两个操作抢同一个组合（共用的组合按条件互斥），也不占用外壳全局层的键', () => {
    for (const os of OSES) {
      const shell = shellCombos(os)
      const ctxs = contexts(os)
      for (const layer of LAYERS) {
        const seen = new Map<string, { id: string; k: KeySpec<unknown> }[]>()
        for (const a of ACTIONS) {
          for (const k of a.keys ?? []) {
            if (k.owner === 'shell' || !(k.layers ?? ['list']).includes(layer)) continue
            const combo = typeof k.combo === 'function' ? k.combo(os) : k.combo
            if (!combo) continue
            // 组合串本身要能解析出主键（写错的组合永远匹配不上）
            expect(parseCombo(combo, os).code, `${a.id} 的 ${combo}`).not.toBe('')
            const key = canonicalCombo(combo, os)
            seen.set(key, [...(seen.get(key) ?? []), { id: a.id, k }])
            expect(shell.has(key), `${a.id} 占用了外壳的 ${combo}`).toBe(false)
          }
        }
        // 共用一个组合的几个键（⌘↩：选了两条以上是连续粘贴，否则是仅复制）都要带条件，任何上下文里至多一个生效
        for (const [key, specs] of seen) {
          if (specs.length < 2) continue
          const ids = specs.map((x) => x.id).join(' 与 ')
          for (const x of specs)
            expect(x.k.when, `${os} ${layer} ${key}：${ids} 没有条件`).toBeDefined()
          for (const c of ctxs) {
            const live = specs.filter((x) => x.k.when?.(c)).map((x) => x.id)
            expect(
              live.length,
              `${os} ${layer} ${key}：${live.join(' 与 ')} 同时生效`,
            ).toBeLessThanOrEqual(1)
          }
        }
      }
    }
  })

  it('每个界面列出的项都能解析：有标签与图标，执行的就是它自己；菜单的分组连续（分隔线不重复）', () => {
    for (const os of OSES) {
      for (const c of contexts(os)) {
        for (const surface of SURFACES) {
          const { calls, exec } = recorder()
          const entries = actionEntries(surface, c, t, exec)
          for (const e of entries) {
            expect(e.label, `${surface} ${e.id}`).not.toMatch(/^clipboard\./)
            expect(e.label.length).toBeGreaterThan(0)
            expect(e.icon, `${surface} ${e.id} 没有图标`).toBeDefined()
            e.run()
            expect(calls.at(-1)?.id).toBe(e.id)
          }
          const groups = entries.map((e) => e.group)
          const runs = groups.filter((g, i) => i === 0 || groups[i - 1] !== g)
          expect(new Set(runs).size, `${surface} 的分组不连续：${groups.join(',')}`).toBe(
            runs.length,
          )
          const menu = menuEntries(surface, c, t, () => {})
          expect(menu.filter((m) => m.type !== 'separator')).toHaveLength(entries.length)
        }
      }
    }
  })

  it('子页（「粘贴为」「提取」）：菜单项带「›」；每项执行注册表里的另一个操作，标签来自展示注册表（两种语言都有）', () => {
    for (const key of [
      ...Object.values(ENTITY_SPECS).map((x) => x.label),
      ...Object.values(TRANSFORM_SPECS).map((x) => x.label),
    ]) {
      expect(hasMessage('zh-CN', key), key).toBe(true)
      expect(hasMessage('en-US', key), key).toBe(true)
    }
    const paged = ACTIONS.filter((a) => a.page)
    expect(paged.map((a) => a.id).sort()).toEqual(['extract', 'paste_as'])
    for (const c of contexts('macos')) {
      for (const e of actionEntries('context', c, t, () => {})) {
        expect(e.submenu, e.id).toBe(paged.some((a) => a.id === e.id))
      }
      if (c.s.inspect === null || c.marked.length > 0) continue
      for (const a of paged) {
        const { calls, exec } = recorder()
        const page = subPage(a, c, t, exec, { label: 'x', back: 'x', backAria: 'x' }, () => {})
        if (!page) continue
        expect(page.items.length, a.id).toBeGreaterThan(0)
        for (const item of page.items) {
          if (item.type !== undefined && item.type !== 'item') continue
          expect(item.label).not.toMatch(/^clipboard\./)
          item.onSelect?.()
        }
        const want = a.id === 'paste_as' ? 'paste_transform' : 'paste_fragment'
        expect(new Set(calls.map((x) => x.id))).toEqual(new Set([want]))
      }
    }
  })

  it('多选：菜单只列能作用于多条的操作；提示条换成多选的一组，按提示里的键执行的就是提示的操作', () => {
    for (const os of OSES) {
      for (const c of contexts(os).filter((x) => x.marked.length > 0)) {
        for (const surface of ['context', 'command'] as const) {
          for (const e of actionEntries(surface, c, t, () => {})) {
            expect(ACTIONS.find((a) => a.id === e.id)?.arity, `${surface} ${e.id}`).not.toBe('one')
          }
        }
        const hints = hintItems('marks', { ...c, target: null }, t)
        // 选了两条以上：⌘↩ 是连续粘贴（当前会话不支持时不占提示条）
        const queue = c.caps.queueUnavailable === null ? ['queue_start'] : []
        expect(hints.map((h) => h.id)).toEqual(['paste', ...queue, 'favorite', 'delete', 'escape'])
        for (const h of hints) {
          for (const combo of h.combos) {
            // ⌫ 在收藏上显示的是 ⌘⌫（菜单直接删）；提示条按没有作用对象的上下文显示 ⌫
            expect(pressIn('list', combo, c), `${os} ${combo}`).toBe(h.id)
          }
        }
      }
    }
  })

  it('芯片条：←/→ 选择、↩ 粘贴片段、Esc 返回（芯片条自己的键位层）', () => {
    for (const os of OSES) {
      const c = contexts(os).find((x) => x.s.chip !== null && x.target?.kind === 'text')
      if (!c) throw new Error('no chip context')
      const hints = hintItems('chips', { ...c, target: null }, t)
      expect(hints.map((h) => [h.id, h.combos])).toEqual([
        ['chip_move', ['ArrowLeft', 'ArrowRight']],
        ['paste_fragment', ['Enter']],
        ['chip_exit', ['Escape']],
      ])
      expect(pressIn('chips', 'Enter', c)).toBe('paste_fragment')
      expect(pressIn('chips', 'Alt+Enter', c)).toBe('paste_fragment')
      expect(pressIn('chips', 'ArrowRight', c)).toBe('chip_move')
      expect(pressIn('chips', 'Escape', c)).toBe('chip_exit')
      // 列表层的 ↩ 仍是粘贴整条
      expect(pressIn('list', 'Enter', c)).toBe('paste')
    }
  })

  it('列在界面上的操作都有标签（提示条用各自的短标签）；⌘K 操作菜单 = 右键菜单 + ⋯ 菜单（顺序不变）', () => {
    for (const a of ACTIONS.filter((x) => Object.keys(x.surfaces ?? {}).length > 0)) {
      expect(a.label, a.id).toBeDefined()
    }
    for (const os of OSES) {
      for (const c of contexts(os)) {
        if (!c.target) continue
        const ids = (s: MenuSurface) => actionEntries(s, c, t, () => {}).map((e) => e.id)
        expect(ids('command')).toEqual([...ids('context'), ...ids('more')])
      }
    }
  })

  it('提示条：列表里必留 粘贴 / 纯文本 / 筛选 / 关闭，其余按 仅复制 → 删除 → 收藏 → 预览 → 操作 让位', () => {
    const c = contexts('macos')[0] as ActionCtx
    const priority = Object.fromEntries(hintItems('list', c, t).map((h) => [h.id, h.priority]))
    expect(priority).toEqual({
      paste: 0,
      paste_plain: 1,
      open_filter_menu: 2,
      escape: 3,
      copy_only: 4,
      delete: 5,
      favorite: 6,
      preview: 7,
      open_actions: 8,
    })
    expect(hintItems('preview', c, t).map((h) => [h.id, h.combos])).toEqual([
      ['preview', ['Space']],
      ['paste', ['Enter']],
      ['move', ['ArrowUp', 'ArrowDown']],
    ])
  })
})
