import React, { useState } from 'react';
import {
  Alert,
  Button,
  Dialog,
  DialogBody,
  DialogFooter,
  DialogHeader,
  DialogTrigger,
  Flex,
  Text,
  TextField,
} from '@backstage/ui';
import { useApi } from '@backstage/core-plugin-api';
import { patApiRef } from '../../api';
import { PatToken } from '../../api/types';

interface Props {
  token: PatToken;
  mode: 'revoke' | 'delete';
  onClose: () => void;
  onDone: () => void;
}

export const RevokeTokenDialog = ({ token, mode, onClose, onDone }: Props) => {
  const api = useApi(patApiRef);
  const [confirmation, setConfirmation] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const matches = confirmation === token.name;
  const mismatch = confirmation.length > 0 && !matches;
  const verb = mode === 'revoke' ? 'Revoke' : 'Delete';

  const handleSubmit = async () => {
    if (!matches || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      if (mode === 'revoke') {
        await api.revokeToken(token.id);
      } else {
        await api.deleteToken(token.id);
      }
      onDone();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : `${verb} failed`);
    } finally {
      setSubmitting(false);
    }
  };

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
      <Dialog width={480}>
        <DialogHeader>
          {verb} token "{token.name}"
        </DialogHeader>
        <DialogBody>
          <Flex direction="column" gap="3">
            {mode === 'revoke' ? (
              <Alert
                status="warning"
                title="Every system using this token loses access immediately"
                description="Revocation cannot be undone. The token record stays in the list and in the audit log so past calls remain attributable."
              />
            ) : (
              <Alert
                status="danger"
                title={
                  token.state === 'active'
                    ? 'Every system using this token loses access immediately'
                    : 'The token record is removed from the list'
                }
                description="Deletion cannot be undone. Audit events already written keep the token id and name, so history is preserved."
              />
            )}
            <Text variant="body-small" color="secondary">
              Type <span className="pat-mono">{token.name}</span> exactly to confirm. The match is
              case-sensitive.
            </Text>
            <TextField
              aria-label="Confirm token name"
              value={confirmation}
              onChange={setConfirmation}
              autoComplete="off"
              spellCheck="false"
              isInvalid={mismatch}
              description={mismatch ? 'Name does not match.' : undefined}
            />
            {error && <Alert status="danger" title={error} />}
          </Flex>
        </DialogBody>
        <DialogFooter>
          <Flex gap="2" justify="end">
            <Button variant="secondary" onPress={onClose} isDisabled={submitting}>
              Cancel
            </Button>
            <Button
              variant="primary"
              destructive
              onPress={handleSubmit}
              isDisabled={!matches || submitting}
              isPending={submitting}
            >
              {verb} token
            </Button>
          </Flex>
        </DialogFooter>
      </Dialog>
    </DialogTrigger>
  );
};
