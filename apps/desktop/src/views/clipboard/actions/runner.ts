/**
 * 视图里使用注册表的入口：按当前 store 构造上下文、执行操作（补上运行环境）、派生菜单 / 键位表 / 提示条。
 * 每个视图实例一份、身份稳定：键位表只编译一次，处理函数在按键时才读最新的状态与运行环境。
 */
import { useLayoutEffect, useMemo, useState } from 'react'
import type { ClipItemMeta, OsKind } from '../../../shared/ipc/types'
import type { TFunction } from '../../../shared/i18n/useT'
import type { KeyBinding } from '../../../shared/keys/keyRouter'
import { useKeyScope } from '../../../shared/keys/useKeyScope'
import type { MenuEntry, MenuPage } from '../../../shared/ui'
import { findHit } from '../model'
import { selectedItem, visibleHits, type ClipStore } from '../store'
import {
  actionEntries,
  applies,
  displayCombo,
  hintItems,
  KEY_SCOPES,
  keyBindings,
  labelOf,
  menuEntries,
  prepareSurface,
  subPage,
  type ExecAction,
} from './derive'
import { actionById } from './registry'
import type {
  ActionArgs,
  ActionCtx,
  ActionEntry,
  ActionEnv,
  ActionSource,
  ClipActionId,
  ClipKeyLayer,
  HintItem,
  HintMode,
  MenuSurface,
} from './types'

const NONE: readonly ClipItemMeta[] = []

export interface ClipRunner {
  readonly os: OsKind
  /**
   * 上下文：`target` 省略时是光标所在条目，给出 id 时是那一条（右键 / 单击的行；已不在列表里时为 null），
   * null 表示没有作用对象（提示条）
   */
  ctx(source: ActionSource, target?: number | null): ActionCtx
  /** 执行（不适用时什么都不做） */
  run<I extends ClipActionId>(id: I, c: ActionCtx, arg?: ActionArgs[I]): boolean | void
  menu(surface: MenuSurface, c: ActionCtx, t: TFunction): MenuEntry[]
  /** 操作的子页（「粘贴为…」、「提取」）；`back` 回到操作菜单。不适用时为 null */
  page(id: ClipActionId, c: ActionCtx, t: TFunction, back: () => void): MenuPage | null
  entries(surface: MenuSurface, c: ActionCtx, t: TFunction): ActionEntry[]
  /** 菜单打开时调用一次 */
  prepare(surface: MenuSurface, c: ActionCtx): void
  bindings(layer: ClipKeyLayer): KeyBinding[]
  hints(mode: HintMode, t: TFunction): HintItem[]
  /** 显示的快捷键（默认按没有作用对象的上下文：提示条、确认条里的说明） */
  shortcut(id: ClipActionId, layer?: ClipKeyLayer, c?: ActionCtx): string | null
  label(id: ClipActionId, t: TFunction, c?: ActionCtx): string
  /** 换成最新的运行环境（视图在每次提交后调用：宿主回调每次渲染可能是新的函数） */
  setEnv(env: ActionEnv): void
}

export function createClipRunner(deps: {
  store: ClipStore
  os: OsKind
  /** 运行环境：命令层、视图的界面能力、宿主 */
  env: ActionEnv
}): ClipRunner {
  const { store, os } = deps
  let current = deps.env
  const env = () => current
  const exec: ExecAction = (a, c, arg) => {
    if (!applies(a, c)) return undefined
    return a.run(c, env(), arg)
  }
  const ctx = (source: ActionSource, target?: number | null, repeat = false): ActionCtx => {
    const s = store.getState()
    const item =
      target === undefined
        ? (selectedItem(s) ?? null)
        : target === null
          ? null
          : (findHit(visibleHits(s), target)?.item ?? null)
    // 右键一个没有被标记的行：菜单只作用于那一行（与 Finder 一致）；其余来源作用于整个选择
    const marked =
      s.marks.length === 0 ||
      ((source === 'context' || source === 'pointer') &&
        item !== null &&
        !s.marks.some((m) => m.id === item.id))
        ? NONE
        : s.marks
    const h = env().host
    return {
      os,
      host: h.kind,
      caps: h.caps,
      onTop: h.onTop,
      source,
      target: item,
      marked,
      s,
      repeat,
    }
  }
  /** 键位层的上下文：删除确认进行中时作用于发起确认的那一条（悬停 / 移动不会改变它） */
  const keyCtx = (layer: ClipKeyLayer, repeat: boolean): ActionCtx => {
    const confirm = store.getState().confirm
    const target = layer === 'confirm' && confirm?.kind === 'delete' ? confirm.id : undefined
    return ctx('key', target, repeat)
  }
  const hintCtx = () => ctx('hint', null)
  return {
    os,
    ctx: (source, target) => ctx(source, target),
    run: (id, c, arg) => exec(actionById(id), c, arg),
    menu: (surface, c, t) => menuEntries(surface, c, t, exec),
    page: (id, c, t, back) => {
      const a = actionById(id)
      const label = labelOf(a, c, t)
      return subPage(
        a,
        c,
        t,
        exec,
        { label, back: label, backAria: t('clipboard.a11y.back') },
        back,
      )
    },
    entries: (surface, c, t) => actionEntries(surface, c, t, exec),
    prepare: (surface, c) => prepareSurface(surface, c, (a) => a.prepare?.(c, env())),
    bindings: (layer) =>
      keyBindings(layer, os, env().host.kind, (repeat) => keyCtx(layer, repeat), exec),
    hints: (mode, t) => hintItems(mode, hintCtx(), t),
    shortcut: (id, layer = 'list', c) => displayCombo(actionById(id), c ?? hintCtx(), layer),
    label: (id, t, c) => labelOf(actionById(id), c ?? hintCtx(), t),
    setEnv: (next) => {
      current = next
    },
  }
}

/** 视图里用：runner 身份在实例内不变，运行环境在每次提交后更新（按键与菜单都发生在提交之后） */
export function useClipRunner(store: ClipStore, os: OsKind, env: ActionEnv): ClipRunner {
  const [runner] = useState(() => createClipRunner({ store, os, env }))
  useLayoutEffect(() => {
    runner.setEnv(env)
  })
  return runner
}

/** 注册一个键位层（作用域名、层级与模态见 `KEY_SCOPES`）；绑定只派生一次，按键时才读最新状态 */
export function useActionKeys(runner: ClipRunner, layer: ClipKeyLayer, enabled: boolean): void {
  const bindings = useMemo(() => runner.bindings(layer), [runner, layer])
  const scope = KEY_SCOPES[layer]
  useKeyScope(scope.scope, bindings, { layer: scope.layer, modal: scope.modal, enabled })
}
