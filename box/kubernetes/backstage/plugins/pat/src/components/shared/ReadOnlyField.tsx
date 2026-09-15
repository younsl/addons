import { useEffect, useState } from 'react';
import { ButtonIcon, Flex, Text } from '@backstage/ui';
import { RiCheckLine, RiFileCopyLine, RiLockLine } from '@remixicon/react';

interface Props {
  label: string;
  value: string;
  hint?: string;
  mono?: boolean;
}

export const ReadOnlyField = ({ label, value, hint, mono }: Props) => {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return undefined;
    const id = setTimeout(() => setCopied(false), 2_000);
    return () => clearTimeout(id);
  }, [copied]);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };

  return (
    <div className="pat-readonly-field">
      <Flex align="center" gap="1">
        <Text variant="body-x-small" weight="bold" color="secondary">
          {label}
        </Text>
        <RiLockLine size={12} aria-hidden />
        <Text variant="body-x-small" color="secondary">
          Read-only
        </Text>
      </Flex>
      <div className="pat-readonly-value">
        <span className={mono ? 'pat-mono' : undefined}>{value}</span>
        <ButtonIcon
          aria-label={copied ? `${label} copied` : `Copy ${label.toLowerCase()}`}
          variant="tertiary"
          size="small"
          onPress={handleCopy}
          icon={copied ? <RiCheckLine /> : <RiFileCopyLine />}
        />
      </div>
      {hint && (
        <Text variant="body-x-small" color="secondary">
          {hint}
        </Text>
      )}
    </div>
  );
};
