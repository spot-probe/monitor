import { cn } from "@/lib/utils"

/**
 * 分段控制器（页签）。
 *
 * 这套外观原本写在「通知」页里（通知 / IP 信息 两个页签），现在「设置」与「维护」也要用，
 * 所以抽出来一份 —— 两处各写一遍迟早会走样。
 *
 * ⚠️ 颜色一律走 token ✓（同事给的 `bg-slate-100/80` / `bg-blue-600` 是写死的 ✓✗）：
 * 这个面板要能换肤 ⇒ 结构尺寸照旧、颜色映射到 `muted` / `primary` ✓。
 */

export type SegmentedOption<T extends string> = { value: T; label: string }

export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label,
  className,
}: {
  value: T
  onChange: (next: T) => void
  options: readonly SegmentedOption<T>[]
  /** 给读屏用的组名（"设置分区"这类）。 */
  label: string
  className?: string
}) {
  return (
    <div
      role="tablist"
      aria-label={label}
      className={cn("inline-flex items-center rounded-lg bg-muted p-1", className)}
    >
      {options.map((o) => {
        const active = o.value === value
        return (
          <button
            key={o.value}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(o.value)}
            className={cn(
              "rounded-md px-3.5 py-1 text-[13px] font-medium transition-all",
              active
                ? "bg-primary text-primary-foreground shadow-xs"
                : "text-muted-foreground hover:bg-muted-foreground/10 hover:text-foreground",
            )}
          >
            {o.label}
          </button>
        )
      })}
    </div>
  )
}
