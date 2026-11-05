import {
  Select as SelectRoot,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

export interface SelectOption {
  value: string;
  label: string;
  description?: string;
}

export function Select({
  value,
  options,
  onChange,
  placeholder,
  className,
  size,
  disabled,
}: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  placeholder?: string;
  className?: string;
  size?: "sm";
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const hasEmptyOption = options.some((o) => o.value === "");
  const selectValue = value === "" && !hasEmptyOption ? null : value;

  return (
    <SelectRoot
      items={options}
      value={selectValue}
      disabled={disabled}
      onValueChange={(next) => onChange(next ?? "")}
    >
      <SelectTrigger
        size={size ?? "default"}
        className={cn("w-full", className)}
      >
        {/* Pass placeholder through untouched (never coerce to ""): base-ui
            treats an empty-string value as "no selection", so a non-null
            placeholder would hide an explicit ""-valued option's label (e.g.
            "all repositories"). With placeholder undefined it resolves the
            option label instead. */}
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      {/* alignItemWithTrigger={false}: open as a normal dropdown below the
          trigger instead of the macOS-style overlay that positions the popup
          over the trigger (awkward, especially with multi-line description
          options). */}
      <SelectContent align="start" alignItemWithTrigger={false}>
        {options.map((o) => (
          <SelectItem key={o.value} value={o.value}>
            <span className="flex min-w-0 flex-col">
              <span>{o.label}</span>
              {o.description && (
                <span className="text-xs leading-4 text-muted-foreground">
                  {o.description}
                </span>
              )}
            </span>
          </SelectItem>
        ))}
        {options.length === 0 && (
          <div className="px-2 py-1.5 text-sm text-muted-foreground">{t("common.no-options")}</div>
        )}
      </SelectContent>
    </SelectRoot>
  );
}
