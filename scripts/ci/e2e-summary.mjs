#!/usr/bin/env node
/**
 * scripts/ci/e2e-summary.mjs：读 Playwright 的 JSON 报告，把失败与 flaky（重试后通过）的用例列进 Actions 摘要。
 *
 * 用法：
 *   node scripts/ci/e2e-summary.mjs [artifacts/e2e-report.json]   打印并写摘要；有 flaky 时逐条 ::warning，并向
 *                                                                   $GITHUB_OUTPUT 写 flaky=true（上传 trace 的条件）
 *   node scripts/ci/e2e-summary.mjs --self-test                    用构造的报告自检（CI 用）
 *
 * 只汇报、不改变作业结论（用例失败时 Playwright 自己以非零退出）；报告不存在时（用例没跑起来）只提示一句、正常退出。
 */
import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { appendSummary, info, isMain, ok, run, setOutput, warn } from '../lib/common.mjs'

/** 去掉终端颜色码（Playwright 的错误信息带 ANSI 转义） */
const ESC = String.fromCharCode(27)
const stripAnsi = (text) => text.replaceAll(new RegExp(`${ESC}\\[[0-9;]*m`, 'g'), '')

/**
 * JSON 报告 → `{ stats, flaky, failed }`；flaky / failed 的每一项为 `{ title, project, attempts, error }`。
 * 用例的标题由各层 describe 的标题与用例名拼成（文件那一层用文件名）
 */
export function summarize(report) {
  const flaky = []
  const failed = []
  const walk = (suite, trail) => {
    const here = suite.title ? [...trail, suite.title] : trail
    for (const spec of suite.specs ?? []) {
      for (const t of spec.tests ?? []) {
        if (t.status !== 'flaky' && t.status !== 'unexpected') continue
        const results = t.results ?? []
        const firstError = results
          .flatMap((r) => r.errors ?? (r.error ? [r.error] : []))
          .map((e) => (e.message ?? '').split('\n').find((l) => l.trim()) ?? '')
          .find(Boolean)
        const item = {
          title: [...here, spec.title].join(' › '),
          project: t.projectName ?? '',
          attempts: results.length,
          error: stripAnsi(firstError ?? '').trim(),
        }
        ;(t.status === 'flaky' ? flaky : failed).push(item)
      }
    }
    for (const child of suite.suites ?? []) walk(child, here)
  }
  for (const s of report.suites ?? []) walk(s, [])
  return { stats: report.stats ?? {}, flaky, failed }
}

function line(item) {
  return `[${item.project}] ${item.title}（${item.attempts} 次执行）${item.error ? `：${item.error}` : ''}`
}

function selfTest() {
  const report = {
    stats: { expected: 33, skipped: 2, unexpected: 1, flaky: 1 },
    suites: [
      {
        title: 'views/ocr/__e2e__/ocr.e2e.ts',
        specs: [],
        suites: [
          {
            title: '长文本（卡片已到上限）',
            specs: [
              {
                title: 'frame=css',
                tests: [
                  {
                    projectName: 'webkit',
                    status: 'flaky',
                    results: [
                      {
                        status: 'failed',
                        errors: [
                          {
                            message:
                              '\u001b[31mError: expect(received).toBeLessThanOrEqual(expected)\u001b[39m\n…',
                          },
                        ],
                      },
                      { status: 'passed', errors: [] },
                    ],
                  },
                  { projectName: 'chromium', status: 'expected', results: [{ status: 'passed' }] },
                ],
              },
            ],
          },
        ],
      },
      {
        title: 'overlay/__e2e__/overlay.e2e.ts',
        specs: [
          {
            title: '拖拽中右键',
            tests: [
              {
                projectName: 'chromium',
                status: 'unexpected',
                results: [
                  { status: 'failed', error: { message: 'Timeout 5000ms exceeded.' } },
                  { status: 'failed' },
                ],
              },
            ],
          },
        ],
      },
    ],
  }
  const s = summarize(report)
  assert.equal(s.flaky.length, 1)
  assert.equal(
    s.flaky[0].title,
    'views/ocr/__e2e__/ocr.e2e.ts › 长文本（卡片已到上限） › frame=css',
  )
  assert.equal(s.flaky[0].project, 'webkit')
  assert.equal(s.flaky[0].attempts, 2)
  assert.equal(s.flaky[0].error, 'Error: expect(received).toBeLessThanOrEqual(expected)')
  assert.equal(s.failed.length, 1)
  assert.equal(s.failed[0].error, 'Timeout 5000ms exceeded.')
  assert.deepEqual(summarize({ suites: [] }), { stats: {}, flaky: [], failed: [] })
  ok('e2e-summary 自检通过')
}

function main() {
  const args = process.argv.slice(2)
  if (args.includes('--self-test')) return selfTest()
  const path = args[0] ?? 'artifacts/e2e-report.json'
  if (!existsSync(path)) {
    info(`${path} 不存在（用例没有跑起来？），跳过汇总`)
    return
  }
  const { stats, flaky, failed } = summarize(JSON.parse(readFileSync(path, 'utf8')))
  const head = `通过 ${stats.expected ?? 0}、跳过 ${stats.skipped ?? 0}、flaky ${stats.flaky ?? flaky.length}、失败 ${stats.unexpected ?? failed.length}`
  info(`浏览器级用例：${head}`)
  // warn 在 Actions 里同时生成注解：flaky 出现在运行页上（github 报告器只注解失败）
  for (const f of flaky) warn(`e2e flaky：${line(f)}`)
  for (const f of failed) warn(`失败：${line(f)}`)
  const md = ['### 浏览器级用例（e2e）', '', head, '']
  if (flaky.length)
    md.push('**flaky（重试后通过，trace 已上传）**', '', ...flaky.map((f) => `- ${line(f)}`), '')
  if (failed.length) md.push('**失败**', '', ...failed.map((f) => `- ${line(f)}`), '')
  appendSummary(md.join('\n'))
  setOutput('flaky', String(flaky.length > 0))
}

if (isMain(import.meta.url)) await run(main)
