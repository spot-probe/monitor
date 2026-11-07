"use client"

import * as React from "react"
import { Progress as ProgressPrimitive } from "radix-ui"

import { cn } from "@/lib/utils"

/**
 * 注册表原版（`new-york-v4/progress`），两处刻意扩展：
 *
 * ① `indicatorClassName` —— 官方把指示器写死成 `bg-primary`，而这个面板的条是**语义色**
 *   （蓝 = 数值、黄 = 该处理、红 = 超限）。与其让调用方拼 `[&>[data-slot=…]]:bg-…`，
 *   不如明说。
 * ② `label` —— 行内进度条没有可见标题，读屏只会听到一个裸的百分比。
 */
function Progress({
  className,
  value,
  indicatorClassName,
  label,
  ...props
}: React.ComponentProps<typeof ProgressPrimitive.Root> & {
  indicatorClassName?: string
  label?: string
}) {
  return (
    <ProgressPrimitive.Root
      data-slot="progress"
      aria-label={label}
      className={cn("relative h-2 w-full overflow-hidden rounded-full bg-primary/20", className)}
      {...props}
    >
      <ProgressPrimitive.Indicator
        data-slot="progress-indicator"
        className={cn("h-full w-full flex-1 bg-primary transition-all", indicatorClassName)}
        style={{ transform: `translateX(-${100 - (value || 0)}%)` }}
      />
    </ProgressPrimitive.Root>
  )
}

export { Progress }
