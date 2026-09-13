import { useState } from "react";
import { CalendarIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useTranslation } from "@/lib/i18n";
import {
  parseISODate,
  startOfLocalDay,
  toISODate,
} from "@/routes/workspace/tokens/-utils/token-expiry";

// A typeable date and a calendar over the same value. Typing is faster for
// someone who knows the date; the calendar is there for someone choosing one.
// The text is held locally rather than derived from the date, because a partly
// typed "2027-0" is not yet a date but must stay on screen.
export function TokenExpiryField({
  value,
  minDate,
  maxDate,
  onChange,
}: {
  value: Date | undefined;
  minDate: Date;
  maxDate: Date;
  onChange: (value: Date | undefined) => void;
}) {
  const { t } = useTranslation();
  const [text, setText] = useState(value ? toISODate(value) : "");
  const [isPickerOpen, setIsPickerOpen] = useState(false);

  return (
    <Field>
      <FieldLabel htmlFor="expires-on">
        {t("token.expires-on")}<span className="text-destructive">*</span>
      </FieldLabel>
      <div className="flex gap-2">
        <Input
          id="expires-on"
          className="flex-1"
          inputMode="numeric"
          placeholder="YYYY-MM-DD"
          value={text}
          // Flagged only once something is typed: an out-of-range or malformed
          // entry clears the value, and the field says why by looking wrong.
          aria-invalid={(text !== "" && !value) || undefined}
          onChange={(event) => {
            const next = event.target.value;
            setText(next);
            const parsed = parseISODate(next);
            onChange(parsed && parsed >= minDate && parsed <= maxDate ? parsed : undefined);
          }}
        />
        <Popover open={isPickerOpen} onOpenChange={setIsPickerOpen}>
          <PopoverTrigger
            render={
              <Button
                type="button"
                variant="outline"
                className="shrink-0"
                aria-label={t("token.select-expiration")}
              />
            }
          >
            <CalendarIcon />
          </PopoverTrigger>
          <PopoverContent align="start" className="w-auto p-0">
            <Calendar
              mode="single"
              selected={value}
              defaultMonth={value ?? minDate}
              disabled={{ before: minDate, after: maxDate }}
              onSelect={(date) => {
                if (!date) return;
                const day = startOfLocalDay(date);
                onChange(day);
                setText(toISODate(day));
                setIsPickerOpen(false);
              }}
            />
          </PopoverContent>
        </Popover>
      </div>
      <FieldDescription>{t("token.expiry-note")}</FieldDescription>
    </Field>
  );
}
