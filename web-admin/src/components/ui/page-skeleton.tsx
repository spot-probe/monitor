import { Skeleton } from "@/components/ui/skeleton"

/**
 * What a section shows while its first request is in flight.
 *
 * Sections already showed skeletons, but each wrote its own — and the ones that wrote
 * none showed empty content that then filled in, which is the jolt felt when switching
 * menu items. This is the shared shape, so saying "not yet" costs one line per page.
 *
 * `shape` follows what the page is about to draw: a title line plus rows reads as a
 * list or a table, `cards` as the themes/notify grid, `form` as a settings page.
 */
function PageSkeleton({
  shape = "list",
  rows = 5,
}: {
  shape?: "list" | "cards" | "form"
  rows?: number
}) {
  if (shape === "cards") {
    return (
      <div className="space-y-4" aria-hidden>
        <Skeleton className="h-28" />
        <div className="grid gap-3 sm:grid-cols-2">
          {Array.from({ length: 4 }, (_, i) => (
            <Skeleton key={i} className="h-44" />
          ))}
        </div>
      </div>
    )
  }
  if (shape === "form") {
    return (
      <div className="space-y-6" aria-hidden>
        <Skeleton className="h-8 w-48" />
        {Array.from({ length: 3 }, (_, i) => (
          <div key={i} className="space-y-2 rounded-xl border p-5">
            <Skeleton className="h-5 w-40" />
            <Skeleton className="h-4 w-full max-w-md" />
            <Skeleton className="h-9 w-full max-w-md" />
          </div>
        ))}
      </div>
    )
  }
  return (
    <div className="space-y-4" aria-hidden>
      <Skeleton className="h-8 w-40" />
      <div className="space-y-3 rounded-xl border p-5">
        {Array.from({ length: rows }, (_, i) => (
          <div key={i} className="flex items-center gap-4">
            <Skeleton className="h-5 w-40" />
            <Skeleton className="h-5 flex-1" />
            <Skeleton className="h-5 w-16" />
          </div>
        ))}
      </div>
    </div>
  )
}

export { PageSkeleton }
