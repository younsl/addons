import { ReactNode } from "react";
import { LockKeyhole } from "lucide-react";

// LockNote renders an accent-bordered callout with a lock icon, matching the
// managed-role notice. Used to explain why an action or edit control is locked
// (e.g. the protected default admin account, a predefined repository).
export function LockNote({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="mt-4 rounded-lg border border-accent-ink/70 bg-primary/5 p-4">
      <h2 className="mb-2 flex items-center gap-2 text-[15px] font-semibold">
        <LockKeyhole className="size-4 text-accent-ink" aria-hidden="true" />
        {title}
      </h2>
      <p className="m-0 text-sm leading-relaxed text-muted-foreground">{children}</p>
    </div>
  );
}
