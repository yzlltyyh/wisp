/**
 * 设置页的通用版式：页标题 + 说明 + 分组（Section / Field 来自组件库，版式以 design.settings-models 为准），
 * 以及几种常用的行：开关行、分段行、下拉行。行的控件改动立即写入（没有「保存」按钮）。
 */
import type { ReactNode } from 'react'
import { Field, SegmentedControl, Select, Switch } from '../../shared/ui'
import type { SegmentOption, SelectOption } from '../../shared/ui'

export function Page({
  title,
  description,
  children,
  'data-testid': testId,
}: {
  title: ReactNode
  description?: ReactNode
  children: ReactNode
  'data-testid'?: string
}) {
  return (
    <div className="st-page" data-testid={testId}>
      <header className="st-page-head">
        <h1 className="st-page-title">{title}</h1>
        {description ? <p className="st-page-desc">{description}</p> : null}
      </header>
      {children}
    </div>
  )
}

interface RowBase {
  label: ReactNode
  description?: ReactNode
  message?: ReactNode
  messageTone?: 'danger' | 'warning' | 'default'
  disabled?: boolean
  /** 标签右侧的状态徽标 */
  badge?: ReactNode
  'data-testid'?: string
}

function RowLabel({ label, badge }: { label: ReactNode; badge?: ReactNode }) {
  if (!badge) return <>{label}</>
  return (
    <span className="st-label-with-badge">
      {label}
      {badge}
    </span>
  )
}

export function SwitchRow({
  label,
  description,
  message,
  messageTone,
  checked,
  onChange,
  disabled,
  badge,
  ariaLabel,
  'data-testid': testId,
}: RowBase & { checked: boolean; onChange: (v: boolean) => void; ariaLabel: string }) {
  return (
    <Field
      label={<RowLabel label={label} badge={badge} />}
      description={description}
      message={message}
      messageTone={messageTone}
      data-testid={testId}
    >
      <Switch
        checked={checked}
        onChange={onChange}
        disabled={disabled}
        aria-label={ariaLabel}
        data-testid={testId ? `${testId}-switch` : undefined}
      />
    </Field>
  )
}

export function SegmentRow<V extends string>({
  label,
  description,
  message,
  messageTone,
  value,
  options,
  onChange,
  disabled,
  badge,
  ariaLabel,
  'data-testid': testId,
}: RowBase & {
  value: V
  options: readonly SegmentOption<V>[]
  onChange: (v: V) => void
  ariaLabel: string
}) {
  return (
    <Field
      label={<RowLabel label={label} badge={badge} />}
      description={description}
      message={message}
      messageTone={messageTone}
      data-testid={testId}
    >
      <SegmentedControl
        label={ariaLabel}
        value={value}
        options={options}
        onChange={onChange}
        disabled={disabled}
        className="st-segmented"
        data-testid={testId ? `${testId}-control` : undefined}
      />
    </Field>
  )
}

export function SelectRow<V extends string>({
  label,
  description,
  message,
  messageTone,
  value,
  options,
  onChange,
  disabled,
  badge,
  ariaLabel,
  placeholder,
  invalid,
  'data-testid': testId,
}: RowBase & {
  value: V | null
  options: readonly SelectOption<V>[]
  onChange: (v: V) => void
  ariaLabel: string
  placeholder?: string
  invalid?: boolean
}) {
  return (
    <Field
      label={<RowLabel label={label} badge={badge} />}
      description={description}
      message={message}
      messageTone={messageTone}
      data-testid={testId}
    >
      <Select
        label={ariaLabel}
        value={value}
        options={options}
        onChange={onChange}
        disabled={disabled}
        placeholder={placeholder}
        invalid={invalid}
        className="st-select"
        data-testid={testId ? `${testId}-select` : undefined}
      />
    </Field>
  )
}
