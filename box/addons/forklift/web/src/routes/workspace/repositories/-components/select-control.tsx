import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useTranslation } from "@/lib/i18n";

export type SelectOption = { value: string; label: string; description?: string };

export function SelectControl({
  value,
  options,
  onChange,
  placeholder,
}: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  placeholder?: string;
}) {
  const { t } = useTranslation();
  // An empty string is a real value only when an option carries it; otherwise
  // it means "nothing chosen", which the control expresses as null so the
  // placeholder shows.
  const selectValue =
    value === "" && !options.some((option) => option.value === "") ? null : value;

  return (
    <Select items={options} value={selectValue} onValueChange={(next) => onChange(next ?? "")}>
      <SelectTrigger className="w-full">
        <SelectValue placeholder={placeholder ?? ""} />
      </SelectTrigger>
      <SelectContent align="start">
        {options.map((option) => (
          <SelectItem key={option.value} value={option.value}>
            <span className="flex min-w-0 flex-col">
              <span>{option.label}</span>
              {option.description && (
                <span className="text-xs leading-4 text-muted-foreground">
                  {option.description}
                </span>
              )}
            </span>
          </SelectItem>
        ))}
        {options.length === 0 && (
          <div className="px-2 py-1.5 text-sm text-muted-foreground">{t("common.no-options")}</div>
        )}
      </SelectContent>
    </Select>
  );
}
