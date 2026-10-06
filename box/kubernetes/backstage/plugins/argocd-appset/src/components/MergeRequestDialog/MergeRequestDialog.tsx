import React, { useEffect, useState } from 'react';
import {
  Alert,
  ButtonLink,
  Dialog,
  DialogBody,
  DialogFooter,
  DialogHeader,
  Flex,
  Link,
  Skeleton,
  Text,
  Tooltip,
  TooltipTrigger,
} from '@backstage/ui';
import { RiExternalLinkLine } from '@remixicon/react';
import { useApi } from '@backstage/core-plugin-api';
import {
  argocdAppsetApiRef,
  ApplicationSetResponse,
  MergeRequestListResponse,
  MergeRequestSummary,
} from '../../api';
import { CopyButton } from '../CopyButton';
import './MergeRequestDialog.css';

/** Web URL of the repository's merge request list, null for an SSH remote. */
export const mergeRequestListUrl = (repoUrl: string): string | null => {
  try {
    const url = new URL(repoUrl);
    if (url.protocol !== 'https:' && url.protocol !== 'http:') return null;
    const project = url.pathname.replace(/\.git$/, '').replace(/\/$/, '');
    return `${url.origin}${project}/-/merge_requests`;
  } catch {
    return null;
  }
};

const ageHours = (isoDate: string, now: Date): number => {
  const then = new Date(isoDate).getTime();
  return Number.isNaN(then) ? 0 : Math.max(0, (now.getTime() - then) / 3_600_000);
};

const plural = (count: number, unit: string) => `${count} ${unit}${count === 1 ? '' : 's'} ago`;

/** `5 hours ago`, `3 days ago`. */
export const submittedAgo = (isoDate: string, now: Date = new Date()): string => {
  const hours = ageHours(isoDate, now);
  if (hours < 1 / 60) return 'just now';
  if (hours < 1) return plural(Math.round(hours * 60), 'minute');
  if (hours < 24) return plural(Math.round(hours), 'hour');
  if (hours < 24 * 30) return plural(Math.round(hours / 24), 'day');
  return plural(Math.round(hours / (24 * 30)), 'month');
};

/** `2026-10-06 (Tue) 17:59:57 GMT+9` in the reader's time zone. */
export const submittedAt = (isoDate: string): string => {
  const date = new Date(isoDate);
  if (Number.isNaN(date.getTime())) return '-';
  const pad = (n: number) => String(n).padStart(2, '0');
  const weekday = date.toLocaleDateString('en-US', { weekday: 'short' });
  const zone =
    new Intl.DateTimeFormat('en-US', { timeZoneName: 'short' })
      .formatToParts(date)
      .find(part => part.type === 'timeZoneName')?.value ?? '';
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} (${weekday}) ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())} ${zone}`
  ).trim();
};

const DIFFSTAT_BLOCKS = 5;

/** Green, red and empty block counts, split by the share of added and removed lines. */
export const diffstatBlocks = (
  additions: number,
  deletions: number,
): { added: number; removed: number; empty: number } => {
  const total = additions + deletions;
  if (total === 0) return { added: 0, removed: 0, empty: DIFFSTAT_BLOCKS };
  let added = Math.round((additions / total) * DIFFSTAT_BLOCKS);
  // Any change on a side gets at least one block, so a one-line removal still shows.
  if (additions > 0 && added === 0) added = 1;
  if (deletions > 0 && added === DIFFSTAT_BLOCKS) added = DIFFSTAT_BLOCKS - 1;
  return { added, removed: DIFFSTAT_BLOCKS - added, empty: 0 };
};

const Diffstat = ({ additions, deletions }: { additions: number; deletions: number }) => {
  const blocks = diffstatBlocks(additions, deletions);
  const kinds = [
    ...Array(blocks.added).fill('added'),
    ...Array(blocks.removed).fill('removed'),
    ...Array(blocks.empty).fill('empty'),
  ];

  return (
    <span
      className="appset-mr-diffstat"
      aria-label={`${additions} lines added, ${deletions} lines removed`}
    >
      <span className="appset-mr-diffstat-add">+{additions}</span>
      <span className="appset-mr-diffstat-del">−{deletions}</span>
      <span className="appset-mr-diffstat-blocks" aria-hidden="true">
        {kinds.map((kind, i) => (
          <span key={i} className={`appset-mr-diffstat-block is-${kind}`} />
        ))}
      </span>
    </span>
  );
};

type LoadState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; result: MergeRequestListResponse };

export const MergeRequestDialog = (props: {
  appSet: ApplicationSetResponse | null;
  onClose: () => void;
}) => {
  const { appSet, onClose } = props;
  const api = useApi(argocdAppsetApiRef);
  const [state, setState] = useState<LoadState>({ status: 'loading' });

  const namespace = appSet?.namespace;
  const name = appSet?.name;

  useEffect(() => {
    if (!namespace || !name) return undefined;

    let cancelled = false;
    setState({ status: 'loading' });
    api.listMergeRequests(namespace, name).then(
      result => {
        if (!cancelled) setState({ status: 'loaded', result });
      },
      error => {
        if (!cancelled) {
          setState({
            status: 'error',
            message: error instanceof Error ? error.message : String(error),
          });
        }
      },
    );

    return () => {
      cancelled = true;
    };
  }, [api, namespace, name]);

  const listUrl = appSet?.repoUrl ? mergeRequestListUrl(appSet.repoUrl) : null;
  const sourcePaths =
    state.status === 'loaded' ? state.result.sourcePaths : (appSet?.sourcePaths ?? []);
  const now = new Date();

  return (
    <Dialog
      isOpen={!!appSet}
      onOpenChange={open => {
        if (!open) onClose();
      }}
      className="appset-mr-dialog"
    >
      <DialogHeader>{appSet?.name ?? 'Merge Requests'}</DialogHeader>
      <DialogBody>
        {appSet && (
          <Flex direction="column" gap="3">
            {/* What the match is made against, since it is by path, not by name. */}
            <dl className="appset-mr-scope">
              <div className="appset-mr-scope-row">
                <dt>Repository</dt>
                <dd>
                  <span className="appset-mr-scope-value">{appSet.repoName || '-'}</span>
                  {appSet.repoName && (
                    <CopyButton
                      value={appSet.repoName}
                      subject="repository"
                      className="appset-mr-scope-copy"
                      iconSize={12}
                    />
                  )}
                </dd>
              </div>
              <div className="appset-mr-scope-row">
                <dt>Path</dt>
                <dd>
                  {sourcePaths.length > 0 ? (
                    <>
                      <span className="appset-mr-scope-value appset-mr-scope-path">
                        {sourcePaths[0]}/
                      </span>
                      {sourcePaths.length > 1 && (
                        <span className="appset-mr-scope-more">+{sourcePaths.length - 1}</span>
                      )}
                      {/* Every path, one per line, not only the one shown. */}
                      <CopyButton
                        value={sourcePaths.join('\n')}
                        subject={sourcePaths.length > 1 ? 'paths' : 'path'}
                        className="appset-mr-scope-copy"
                        iconSize={12}
                      />
                    </>
                  ) : (
                    <span className="appset-mr-scope-value">No source path known</span>
                  )}
                </dd>
              </div>
            </dl>

            {state.status === 'loading' && (
              <Flex direction="column" gap="2">
                <Skeleton width="100%" height={36} />
                <Skeleton width="100%" height={44} />
              </Flex>
            )}

            {state.status === 'error' && (
              <Flex direction="column" gap="1">
                <Alert status="danger" title="Failed to load merge requests" />
                <Text variant="body-small" color="secondary">
                  {state.message}
                </Text>
              </Flex>
            )}

            {state.status === 'loaded' &&
              (state.result.mergeRequests.length === 0 ? (
                <div className="appset-mr-empty">
                  <Text variant="body-small" color="secondary">
                    No open merge requests
                  </Text>
                </div>
              ) : (
                <ul className="appset-mr-list">
                  {state.result.mergeRequests.map(mr => (
                    <MergeRequestRow key={mr.iid} mr={mr} now={now} />
                  ))}
                </ul>
              ))}
          </Flex>
        )}
      </DialogBody>
      {listUrl && (
        <DialogFooter>
          <Flex justify="end">
            <ButtonLink
              href={listUrl}
              target="_blank"
              rel="noopener noreferrer"
              variant="tertiary"
              size="small"
              iconEnd={<RiExternalLinkLine size={14} />}
            >
              GitLab
            </ButtonLink>
          </Flex>
        </DialogFooter>
      )}
    </Dialog>
  );
};

const MergeRequestRow = (props: { mr: MergeRequestSummary; now: Date }) => {
  const { mr, now } = props;

  /*
    Two sibling links rather than one around the whole row: the time is the
    tooltip trigger, which must be focusable on its own, and links cannot nest.
  */
  return (
    <li className={`appset-mr-row${mr.draft ? ' is-draft' : ''}`}>
      <a href={mr.webUrl} target="_blank" rel="noopener noreferrer" className="appset-mr-main">
        <span className="appset-mr-title-row">
          <span className="appset-mr-iid">!{mr.iid}</span>
          <span className="appset-mr-title">
            {mr.draft && <span className="appset-mr-draft">Draft</span>}
            {mr.title}
          </span>
        </span>
        <span className="appset-mr-branch">{mr.sourceBranch}</span>
      </a>
      <span className="appset-mr-side">
        <TooltipTrigger delay={200}>
          <Link
            href={mr.webUrl}
            target="_blank"
            rel="noopener noreferrer"
            className="appset-mr-when"
            aria-label={`Opened ${submittedAt(mr.createdAt)}`}
          >
            {submittedAgo(mr.createdAt, now)}
          </Link>
          <Tooltip style={{ maxWidth: 'none', whiteSpace: 'nowrap' }}>
            {submittedAt(mr.createdAt)}
          </Tooltip>
        </TooltipTrigger>
        <Diffstat additions={mr.additions ?? 0} deletions={mr.deletions ?? 0} />
      </span>
    </li>
  );
};
