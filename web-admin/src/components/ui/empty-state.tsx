import * as React from "react"

import { cn } from "@/lib/utils"

/**
 * The one way a list says "there is nothing here yet".
 *
 * Each list used to improvise this: some drew nothing at all, which reads as a page
 * that failed to load rather than one that is simply empty. A short line plus the
 * action that fills it is what an operator needs, and it should look the same
 * everywhere.
 */
function EmptyState({
  icon,
  title,
  hint,
  action,
  className,
}: {
  icon?: React.ReactNode
  title: string
  hint?: string
  action?: React.ReactNode
  className?: string
}) {
  return (
    <div
      data-slot="empty-state"
      className={cn(
        "flex flex-col items-center justify-center gap-2 rounded-xl border border-dashed px-6 py-12 text-center",
        className
      )}
    >
      {icon && <span className="grid size-9 place-items-center rounded-full bg-accent text-muted-foreground" aria-hidden>{icon}</span>}
      <p className="font-medium">{title}</p>
      {hint && <p className="max-w-md text-sm text-muted-foreground">{hint}</p>}
      {action && <div className="mt-2">{action}</div>}
    </div>
  )
}

export { EmptyState }
