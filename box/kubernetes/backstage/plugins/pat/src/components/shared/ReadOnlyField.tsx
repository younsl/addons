import { Flex, Text } from '@backstage/ui';
import { RiLockLine } from '@remixicon/react';

interface Props {
  label: string;
  value: string;
  hint?: string;
  mono?: boolean;
}

export const ReadOnlyField = ({ label, value, hint, mono }: Props) => (
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
    <div className={`pat-readonly-value${mono ? ' pat-mono' : ''}`}>{value}</div>
    {hint && (
      <Text variant="body-x-small" color="secondary">
        {hint}
      </Text>
    )}
  </div>
);
