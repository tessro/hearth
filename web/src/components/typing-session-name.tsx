import { AnimatePresence, motion, useReducedMotion } from "motion/react"

import { cn } from "@/lib/utils"

interface TypingSessionNameProps {
  name: string
  className?: string
}

export function TypingSessionName({ name, className }: TypingSessionNameProps) {
  const reduceMotion = useReducedMotion()
  const duration = reduceMotion ? 0 : Math.min(1.25, Math.max(0.3, name.length * 0.035))

  return (
    <span className={cn("block min-w-0", className)}>
      <span aria-live="polite" className="sr-only">
        {name}
      </span>
      <AnimatePresence initial={false} mode="wait">
        <motion.span
          animate={{ clipPath: "inset(0 0% 0 0)", opacity: 1 }}
          aria-hidden="true"
          className="block truncate"
          exit={{ opacity: 0 }}
          initial={{ clipPath: "inset(0 100% 0 0)", opacity: 1 }}
          key={name}
          transition={{
            clipPath: { duration, ease: "linear" },
            opacity: { duration: reduceMotion ? 0 : 0.12 },
          }}
        >
          {name}
        </motion.span>
      </AnimatePresence>
    </span>
  )
}
