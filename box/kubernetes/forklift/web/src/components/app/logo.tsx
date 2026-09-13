import { cn } from "@/lib/utils";

// Logo renders the web public copy of the official forklift artwork.
export function Logo({ size = 34 }: { size?: number }) {
  const compact = size <= 32;
  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center overflow-hidden rounded-md bg-black",
        compact ? "size-8" : "size-[34px]"
      )}
    >
      <img src="/forklift-logo.png" alt="" className="size-full object-cover" />
    </span>
  );
}
