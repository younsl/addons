import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxList,
  ComboboxItem,
} from "@/components/ui/combobox";
import { useTranslation } from "@/lib/i18n";

// The repository-glob entry field. It is a combobox rather than a select
// because the value need not be an existing repository - a pattern may match
// repositories created later - so the names only assist, never constrain.
// Shared by role permissions and token scopes, which enter the same thing.
export function RepositoryPatternCombobox({
  className,
  options,
  types,
  value,
  onValueChange,
}: {
  className?: string;
  options: string[];
  types: Record<string, string>;
  value: string;
  onValueChange: (value: string) => void;
}) {
  const { t } = useTranslation();

  return (
    <Combobox
      items={options}
      inputValue={value}
      // Only a value that is in the list counts as selected; a free-typed
      // pattern is valid input but has nothing to highlight.
      value={options.includes(value) ? value : null}
      onInputValueChange={onValueChange}
      onValueChange={(next) => {
        if (typeof next === "string") onValueChange(next);
      }}
    >
      {/* Enter is how a free-typed pattern is committed, and this field lives
          inside the role and token creation forms. When the typed text matches
          no repository there is nothing for the combobox to select, so Enter
          falls through to the browser's implicit form submission and creates
          the role - with none of the permissions the user was in the middle of
          adding. Preventing the default stops the submit only; the combobox's
          own handler still runs, so picking a listed option with Enter keeps
          working. */}
      <ComboboxInput
        placeholder={t("common.repo-pattern-placeholder")}
        className={className}
        onKeyDown={(event) => {
          if (event.key === "Enter") event.preventDefault();
        }}
      />
      <ComboboxContent>
        <ComboboxEmpty>{t("common.no-repositories-found")}</ComboboxEmpty>
        <ComboboxList>
          {options.map((option) => (
            <ComboboxItem key={option} value={option}>
              <span className="min-w-0 truncate">
                {option}
                {types[option] && (
                  <span className="ml-2 text-xs text-muted-foreground">{types[option]}</span>
                )}
              </span>
            </ComboboxItem>
          ))}
        </ComboboxList>
      </ComboboxContent>
    </Combobox>
  );
}
