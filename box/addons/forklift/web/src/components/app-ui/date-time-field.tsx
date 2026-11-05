import { useEffect, useState } from "react";
import { CalendarIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useTranslation } from "@/lib/i18n";

const pad = (n: number) => String(n).padStart(2, "0");
const dateText = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const timeText = (d: Date) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;

// parseDateTime reads a typed YYYY-MM-DD and HH:mm as a local instant, or
// null while either is incomplete or not a real date.
export function parseDateTime(date: string, time: string): Date | null {
  const d = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date.trim());
  const t = /^(\d{1,2}):(\d{2})$/.exec(time.trim());
  if (!d || !t) return null;
  const [y, m, day, h, min] = [d[1], d[2], d[3], t[1], t[2]].map(Number);
  if (h > 23 || min > 59) return null;
  const out = new Date(y, m - 1, day, h, min);
  return out.getFullYear() === y && out.getMonth() === m - 1 && out.getDate() === day ? out : null;
}

// DateTimeField picks a local date and time. The date can be typed or chosen
// on the console's calendar, and the time is typed. The texts are held locally
// rather than derived from the value, because a partly typed "2026-1" is not
// yet a date but must stay on screen.
export function DateTimeField({ id, value, onChange, minDate, invalid }: {
  id?: string;
  value: Date | null;
  onChange: (value: Date | null) => void;
  minDate?: Date;
  invalid?: boolean;
}) {
  const { t } = useTranslation();
  const [date, setDate] = useState(value ? dateText(value) : "");
  const [time, setTime] = useState(value ? timeText(value) : "");
  const [open, setOpen] = useState(false);

  // A value set from outside (loaded or saved) replaces what is typed.
  useEffect(() => {
    if (!value) return;
    setDate(dateText(value));
    setTime(timeText(value));
  }, [value?.getTime()]); // eslint-disable-line react-hooks/exhaustive-deps

  const update = (nextDate: string, nextTime: string) => {
    setDate(nextDate);
    setTime(nextTime);
    onChange(parseDateTime(nextDate, nextTime));
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      <div className="flex gap-2">
        <Input id={id} className="w-36" inputMode="numeric" placeholder="YYYY-MM-DD" value={date}
          aria-label={t("datetime.date")} aria-invalid={invalid || undefined}
          onChange={(e) => update(e.target.value, time)} />
        <Popover open={open} onOpenChange={setOpen}>
          <PopoverTrigger render={<Button type="button" variant="outline" className="shrink-0" aria-label={t("datetime.pick-date")} />}>
            <CalendarIcon />
          </PopoverTrigger>
          <PopoverContent align="start" className="w-auto p-0">
            <Calendar
              mode="single"
              selected={value ?? undefined}
              defaultMonth={value ?? minDate ?? new Date()}
              disabled={minDate ? { before: minDate } : undefined}
              onSelect={(day) => {
                if (!day) return;
                update(dateText(day), time || "09:00");
                setOpen(false);
              }}
            />
          </PopoverContent>
        </Popover>
      </div>
      <Input className="w-24" inputMode="numeric" placeholder="HH:mm" value={time}
        aria-label={t("datetime.time")} aria-invalid={invalid || undefined}
        onChange={(e) => update(date, e.target.value)} />
    </div>
  );
}
