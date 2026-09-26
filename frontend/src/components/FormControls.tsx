import { forwardRef, useId, useRef, type InputHTMLAttributes, type SelectHTMLAttributes, type ReactNode } from 'react'
import { CalendarDays, ChevronDown } from 'lucide-react'

interface FieldProps {
  label: string
  hint?: string | undefined
  error?: string | undefined
  compact?: boolean | undefined
}

function FieldFrame({ id, label, hint, error, compact, children }: FieldProps & { id: string; children: ReactNode }) {
  return <div className={`field ${compact ? 'field-compact' : ''} ${error ? 'field-invalid' : ''}`}>
    <label htmlFor={id} className={compact ? 'sr-only' : undefined}>{label}</label>
    {children}
    {(error || hint) && <small id={`${id}-help`} className={error ? 'field-error' : undefined}>{error || hint}</small>}
  </div>
}

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input({ className = '', type = 'text', ...props }, ref) {
  return <input {...props} ref={ref} type={type} className={`ui-input ${className}`} />
})

export function InputField({ label, hint, error, compact, unit, action, id: suppliedId, value, onChange, ...props }: FieldProps & Omit<InputHTMLAttributes<HTMLInputElement>, 'onChange'> & {
  unit?: string
  action?: ReactNode
  onChange: (value: string) => void
}) {
  const generatedId = useId()
  const id = suppliedId ?? generatedId
  return <FieldFrame {...{ id, label, hint, error, compact }}>
    <div className="input-wrap">
      <Input {...props} id={id} value={value} onChange={(e) => onChange(e.target.value)}
        aria-invalid={error ? true : undefined} aria-describedby={error || hint ? `${id}-help` : undefined} />
      {unit && <span className="unit">{unit}</span>}
      {action}
    </div>
  </FieldFrame>
}

export function SelectField({ label, hint, error, compact, id: suppliedId, onChange, options, ...props }: FieldProps & Omit<SelectHTMLAttributes<HTMLSelectElement>, 'onChange' | 'children'> & {
  onChange: (value: string) => void
  options: readonly { value: string; label: string }[]
}) {
  const generatedId = useId()
  const id = suppliedId ?? generatedId
  return <FieldFrame {...{ id, label, hint, error, compact }}>
    <div className="input-wrap select-wrap">
      <select {...props} id={id} onChange={(e) => onChange(e.target.value)}
        aria-invalid={error ? true : undefined} aria-describedby={error || hint ? `${id}-help` : undefined}>
        {options.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
      </select>
      <ChevronDown size={16} aria-hidden="true" />
    </div>
  </FieldFrame>
}

/** 原生日期/月历保留系统键盘与移动端体验；值始终是 YYYY-MM[-DD]。 */
export function DateField({ label, hint, error, value, onChange, min, max, mode = 'date', id: suppliedId }: FieldProps & {
  id?: string
  value: string
  onChange: (value: string) => void
  min?: string
  max?: string
  mode?: 'date' | 'month'
}) {
  const generatedId = useId()
  const id = suppliedId ?? generatedId
  const ref = useRef<HTMLInputElement>(null)
  return <FieldFrame {...{ id, label, hint, error }}>
    <div className="input-wrap date-wrap">
      <Input ref={ref} id={id} type={mode} value={value} min={min} max={max}
        placeholder={mode === 'month' ? 'YYYY-MM' : 'YYYY-MM-DD'}
        aria-invalid={error ? true : undefined} aria-describedby={error || hint ? `${id}-help` : undefined}
        onChange={(e) => onChange(e.target.value)} />
      <button type="button" className="date-trigger" aria-label={`选择${label}`} onClick={() => {
        ref.current?.focus()
        try { ref.current?.showPicker?.() } catch { /* 不支持系统日历时保留键盘输入。 */ }
      }}><CalendarDays size={16} aria-hidden="true" /></button>
    </div>
  </FieldFrame>
}
