import { RotateCw } from "lucide-react"

import { Button } from "@/components/ui/button"

/**
 * What a page shows when its first request failed.
 *
 * A toast is not enough: it disappears, and the page behind it keeps rendering an
 * empty list that reads as "you have no nodes" rather than "we could not ask". The
 * message stays until it is retried, and the retry is one click.
 */
function RetryState({
  message,
  onRetry,
  busy = false,
}: {
  message: string
  onRetry: () => void
  busy?: boolean
}) {
  return (
    <div
      data-slot="retry-state"
      role="alert"
      className="flex flex-col items-center justify-center gap-3 rounded-xl border border-destructive/30 bg-destructive/5 px-6 py-12 text-center"
    >
      <p className="font-medium text-destructive-fg">没能取到数据</p>
      <p className="max-w-md text-sm text-muted-foreground">{message}</p>
      <Button variant="outline" size="sm" onClick={onRetry} disabled={busy}>
        <RotateCw className={busy ? "size-4 animate-spin" : "size-4"} /> 重试
      </Button>
    </div>
  )
}

export { RetryState }
