import { mergeProps } from "@base-ui/react/merge-props"
import { useRender } from "@base-ui/react/use-render"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"

// One row of a menu: a verb, full width, no button chrome. It carries the
// interactive treatment the shell's own rows use, so a menu opened from a table
// and one opened from the sidebar behave the same under the pointer, and a
// disabled row follows the form controls rather than inventing a third look.
const menuItemVariants = cva(
  "w-full rounded-md border border-transparent px-3 py-2 text-left text-sm whitespace-nowrap transition-colors disabled:cursor-not-allowed disabled:text-muted-foreground disabled:opacity-50",
  {
    variants: {
      variant: {
        default:
          "cursor-pointer text-muted-foreground not-disabled:hover:bg-[var(--fx-surface-hover)] not-disabled:hover:text-foreground",
        danger:
          "cursor-pointer text-destructive not-disabled:hover:bg-[var(--fx-surface-hover)]",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  }
)

function MenuItem({
  className,
  variant = "default",
  render,
  ...props
}: useRender.ComponentProps<"button"> & VariantProps<typeof menuItemVariants>) {
  return useRender({
    defaultTagName: "button",
    props: mergeProps<"button">(
      {
        type: "button",
        className: cn(menuItemVariants({ variant }), className),
      },
      props
    ),
    render,
    state: {
      slot: "menu-item",
      variant,
    },
  })
}

export { MenuItem, menuItemVariants }
