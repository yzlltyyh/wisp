/**
 * 邻居应用冲突（Easydict 等也用同一组合）与键盘布局建议（AltGr、多输入语言——macOS 不显示、QQ），只在有内容时出现。
 * 设置「快捷键」与引导「试一试」共用。
 *
 * 数据在窗口打开后空闲时预取（`prefetchAdvice`，悬停「快捷键」导航项时也会触发），页面首帧就能同步渲染，
 * 不会在进入页面后才插进一条横幅把整页往下推；进入页面时再后台探测一次（打开快捷键页 / 引导页时探测），
 * 结果变了才更新。还没取到、或结果变化时用 sectionExpand 的高度展开，而不是跳一下。
 */
import { Keyboard } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useSyncExternalStore, type ReactNode } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { KeyboardAdvice, NeighborHit } from '../../shared/ipc/types'
import { formatHotkey, formatModifiers } from '../../shared/keys/formatHotkey'
import { usePreset } from '../../shared/motion/presets'
import { Banner } from '../../shared/ui'

interface Advice {
  neighbors: readonly NeighborHit[]
  keyboard: KeyboardAdvice | null
}

let cache: Advice | null = null
let request: Promise<void> | null = null
const listeners = new Set<() => void>()

async function probe(): Promise<void> {
  const [n, a] = await Promise.all([
    api.neighborApps().catch(() => [] as NeighborHit[]),
    api.keyboardAdvice().catch(() => null),
  ])
  const next: Advice = { neighbors: Array.isArray(n) ? n : [], keyboard: a ?? null }
  if (JSON.stringify(next) === JSON.stringify(cache)) return
  cache = next
  for (const notifyListener of listeners) notifyListener()
}

/** 预取邻居应用与键盘布局建议（窗口生命周期内一次） */
export function prefetchAdvice(): Promise<void> {
  request ??= probe()
  return request
}

/** 测试用 */
export function resetAdviceCache(): void {
  cache = null
  request = null
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** 邻居应用冲突与键盘布局建议（只在有内容时出现） */
export function AdviceBanners() {
  const t = useT()
  const os = usePlatform()
  const advice = useSyncExternalStore(
    subscribe,
    () => cache,
    () => cache,
  )
  const motion = usePreset('sectionExpand')
  // 进入页面：已有缓存时后台再探测一次（结果变了才更新），没有时就是第一次取
  useEffect(() => {
    if (cache) void probe()
    else void prefetchAdvice()
  }, [])
  const tips: Array<{ id: string; text: ReactNode }> = []
  for (const n of advice?.neighbors ?? []) {
    tips.push({
      id: `n-${n.app_name}-${n.combo}`,
      text: t('settings.hotkeys.neighbor', {
        app: n.app_name,
        combo: n.display,
        action: t(`hotkeys.action.${n.action}`),
      }),
    })
  }
  const kb = advice?.keyboard
  // 文案里的组合一律由 formatHotkey / formatModifiers 生成（与键帽、快捷键列表同一种写法，例如 Windows「Ctrl+Alt+A」）
  const mods = (ctrl: boolean, alt: boolean, shift: boolean) =>
    formatModifiers({ ctrl, alt, shift, meta: false }, os).join(os === 'macos' ? '' : '+')
  const ctrlAlt = mods(true, true, false)
  const altShift = mods(false, true, true)
  if (kb?.has_altgr)
    tips.push({
      id: 'altgr',
      text: t('settings.hotkeys.advice_altgr', { ctrl_alt: ctrlAlt, alt_shift: altShift }),
    })
  // 「Alt + Shift 切换输入语言」只是 Windows（与部分 X11 布局）的行为：macOS 切换输入源用 ⌃Space / Fn / 大写锁定，
  // ⌥⇧ 不切换（而且与 Wisp 自己的默认键 ⌥⇧D / ⌥⇧S 矛盾），即使 Rust 报告了多个输入源也不显示
  if (kb?.multi_input_lang && os !== 'macos')
    tips.push({ id: 'ime', text: t('settings.hotkeys.advice_multi_lang', { alt_shift: altShift }) })
  if (kb?.qq_running)
    tips.push({
      id: 'qq',
      text: t('settings.hotkeys.advice_qq', { combo: formatHotkey('Control+Alt+KeyA', os) }),
    })
  return (
    <AnimatePresence initial={false}>
      {tips.length > 0 ? (
        <m.div key="advice" className="st-clip st-advice" {...motion} data-testid="hotkeys-advice">
          <div className="st-global-banner">
            <Banner tone="info" icon={Keyboard} className="st-banner-top">
              <ul className="st-banner-list">
                {tips.map((tip) => (
                  <li key={tip.id}>{tip.text}</li>
                ))}
              </ul>
            </Banner>
          </div>
        </m.div>
      ) : null}
    </AnimatePresence>
  )
}
