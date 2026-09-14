import React, { useEffect, useMemo, useState } from 'react';
import {
  Alert,
  Button,
  DatePicker,
  Dialog,
  DialogBody,
  DialogFooter,
  DialogHeader,
  DialogTrigger,
  Flex,
  NumberField,
  Text,
  TextAreaField,
  TextField,
} from '@backstage/ui';
import { getLocalTimeZone, today } from '@internationalized/date';
import { useApi } from '@backstage/core-plugin-api';
import { patApiRef } from '../../api';
import { CreatedToken, PatSettings, TokenScope } from '../../api/types';
import {
  AccessMap,
  accessMapToScopes,
  DESCRIPTION_MAX,
  NAME_MAX,
  nameError,
  sanitizeName,
  ScopeGrid,
} from '../shared';

interface Props {
  settings: PatSettings;
  onClose: () => void;
  onCreated: () => void;
}

/**
 * The calendar value type is taken from the DatePicker props rather than
 * imported, because react-aria resolves its own copy of @internationalized/date
 * and the two class types are not assignable even though the objects are.
 */
type DateValue = NonNullable<React.ComponentProps<typeof DatePicker>['value']>;

function daysBetween(from: DateValue, to: DateValue): number {
  const a = from.toDate(getLocalTimeZone()).getTime();
  const b = to.toDate(getLocalTimeZone()).getTime();
  return Math.round((b - a) / 86_400_000);
}

/**
 * Two-step dialog: collect the token definition, then reveal the secret once.
 * The Create button stays disabled until name, description, a lifetime within
 * the allowed range and at least one scope are all present.
 */
export const CreateTokenDialog = ({ settings, onClose, onCreated }: Props) => {
  const api = useApi(patApiRef);
  const todayDate = useMemo(
    () => today(getLocalTimeZone()) as unknown as DateValue,
    [],
  );
  const maxDays = settings.maxExpiryDays;

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [days, setDays] = useState<number>(NaN);
  const [access, setAccess] = useState<AccessMap>({});
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<CreatedToken | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return undefined;
    const id = setTimeout(() => setCopied(false), 2_000);
    return () => clearTimeout(id);
  }, [copied]);

  const scopes: TokenScope[] = useMemo(
    () => accessMapToScopes(access, settings.scopablePlugins),
    [access, settings.scopablePlugins],
  );

  const trimmedName = name.trim();
  const trimmedDescription = description.trim();
  const daysValid = Number.isInteger(days) && days >= 1 && days <= maxDays;
  const nameProblem = nameError(trimmedName);
  const nameValid = nameProblem === null;
  const descriptionValid =
    trimmedDescription.length > 0 && trimmedDescription.length <= DESCRIPTION_MAX;
  const scopesValid = scopes.length > 0;
  const canSubmit = nameValid && descriptionValid && daysValid && scopesValid && !submitting;

  const expiryDate: DateValue | null = daysValid ? todayDate.add({ days }) : null;
  const minDate = todayDate.add({ days: 1 });
  const maxDate = todayDate.add({ days: maxDays });

  const handleDateChange = (value: DateValue | null) => {
    if (!value) {
      setDays(NaN);
      return;
    }
    setDays(daysBetween(todayDate, value));
  };

  const handleSubmit = async () => {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      const result = await api.createToken({
        name: trimmedName,
        description: trimmedDescription,
        expiresInDays: days,
        scopes,
      });
      setCreated(result);
      onCreated();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to create token');
    } finally {
      setSubmitting(false);
    }
  };

  const handleCopy = async () => {
    if (!created) return;
    try {
      await navigator.clipboard.writeText(created.token);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };

  const missing: string[] = [];
  if (!nameValid) missing.push('a valid name');
  if (!descriptionValid) missing.push('description');
  if (!daysValid) missing.push(`expiry between 1 and ${maxDays} days`);
  if (!scopesValid) missing.push('at least one permission');

  return (
    <DialogTrigger
      defaultOpen
      onOpenChange={open => {
        if (!open) onClose();
      }}
    >
      <button
        aria-hidden
        style={{ position: 'fixed', opacity: 0, pointerEvents: 'none', width: 0, height: 0 }}
      >
        trigger
      </button>
      <Dialog width={620}>
        <DialogHeader>{created ? 'Token created' : 'Create personal access token'}</DialogHeader>
        <DialogBody>
          {created ? (
            <Flex direction="column" gap="3">
              <Alert
                status="warning"
                title="Copy the token now"
                description="This is the only time the secret is shown. Backstage stores a hash, so a lost token must be revoked and reissued."
              />
              <div className="pat-token-secret" data-testid="pat-secret">
                {created.token}
              </div>
              <Flex gap="2" align="center">
                <Button variant="primary" onPress={handleCopy}>
                  {copied ? 'Copied' : 'Copy to clipboard'}
                </Button>
                <Text variant="body-x-small" color="secondary">
                  Expires {new Date(created.record.expiresAt).toLocaleDateString()}
                </Text>
              </Flex>
              <Text variant="body-small" color="secondary">
                Send it as <code>Authorization: Bearer &lt;token&gt;</code> to any
                <code> /api/&lt;plugin&gt;</code> endpoint covered by the granted scopes.
              </Text>
            </Flex>
          ) : (
            <Flex direction="column" gap="4">
              <TextField
                label="Name"
                isRequired
                placeholder="e.g. jenkins-catalog-sync"
                description={`Letters, digits, - and _ only. Shown in the token list and audit log. ${trimmedName.length}/${NAME_MAX}`}
                value={name}
                onChange={value => setName(sanitizeName(value))}
                maxLength={NAME_MAX}
                isInvalid={name.length > 0 && !nameValid}
              />
              {name.length > 0 && nameProblem && (
                <Text variant="body-x-small" color="danger">
                  {nameProblem}
                </Text>
              )}
              <TextAreaField
                label="Description"
                isRequired
                placeholder="Which system uses this token and why"
                description={`Required so every token has an owner and purpose. ${trimmedDescription.length}/${DESCRIPTION_MAX}`}
                value={description}
                onChange={setDescription}
                maxLength={DESCRIPTION_MAX}
                rows={2}
              />
              <div>
                <div className="pat-expiry-row">
                  <NumberField
                    label="Expires in (days)"
                    isRequired
                    placeholder={`1 - ${maxDays}`}
                    minValue={1}
                    maxValue={maxDays}
                    step={1}
                    value={days}
                    onChange={value => setDays(value)}
                    formatOptions={{ maximumFractionDigits: 0 }}
                  />
                  <DatePicker
                    label="Expiry date"
                    isRequired
                    minValue={minDate}
                    maxValue={maxDate}
                    value={expiryDate}
                    onChange={handleDateChange}
                  />
                </div>
                <Text variant="body-x-small" color="secondary">
                  Type a number of days or pick a date. Maximum lifetime is {maxDays} days.
                </Text>
              </div>

              <ScopeGrid
                plugins={settings.scopablePlugins}
                access={access}
                onChange={setAccess}
              />

              {!canSubmit && !submitting && (
                <Text variant="body-x-small" color="secondary">
                  Required before creating: {missing.join(', ')}.
                </Text>
              )}
              {error && <Alert status="danger" title={error} />}
            </Flex>
          )}
        </DialogBody>
        <DialogFooter>
          <Flex gap="2" justify="end">
            {created ? (
              <Button variant="primary" onPress={onClose}>
                Done
              </Button>
            ) : (
              <>
                <Button variant="secondary" onPress={onClose} isDisabled={submitting}>
                  Cancel
                </Button>
                <Button
                  variant="primary"
                  onPress={handleSubmit}
                  isDisabled={!canSubmit}
                  isPending={submitting}
                >
                  Create token
                </Button>
              </>
            )}
          </Flex>
        </DialogFooter>
      </Dialog>
    </DialogTrigger>
  );
};
