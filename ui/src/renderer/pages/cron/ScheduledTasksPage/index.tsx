/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import classNames from 'classnames';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { Button, Switch, Message, Empty, Spin, Tooltip, Input } from '@arco-design/web-react';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { useAllCronJobs } from '@renderer/pages/cron/useCronJobs';
import { formatSchedule, formatNextRun } from '@renderer/pages/cron/cronUtils';
import { type ICronJob } from '@/common/adapter/ipcBridge';
import { useKeepAwake } from '@renderer/hooks/ui/useKeepAwake';
import { useConversationAgents } from '@renderer/pages/conversation/hooks/useConversationAgents';
import CronStatusTag from './CronStatusTag';
import CreateTaskDialog from './CreateTaskDialog';
import { getJobAgentMeta } from './jobAgentMeta';
import { shortSessionId } from '@renderer/utils/ui/shortId';
import { filterCronJobsByQuery } from './cronJobSearch';
import { parseScheduledCreateTarget } from './scheduledCreateTarget';
import { DESKTOP_SCHEDULED_TASK_COLUMNS } from './scheduledTaskLayout';
import ScheduledTaskActions from './ScheduledTaskActions';

const ScheduledTasksPage: React.FC = () => {
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const { jobs, loading, pauseJob, resumeJob, deleteJob } = useAllCronJobs();
  const { cliAgents } = useConversationAgents();
  const [createDialogVisible, setCreateDialogVisible] = useState(false);
  const [lockedCreateConversationId, setLockedCreateConversationId] = useState<number | undefined>(undefined);
  const [searchQuery, setSearchQuery] = useState('');
  const { keepAwake, setKeepAwake } = useKeepAwake();

  useEffect(() => {
    const createTarget = parseScheduledCreateTarget(searchParams);
    if (!createTarget) return;

    setLockedCreateConversationId(createTarget.conversationId);
    setCreateDialogVisible(true);

    const next = new URLSearchParams(searchParams);
    next.delete('create');
    next.delete('conversation_id');
    next.delete('conversationId');
    setSearchParams(next, { replace: true });
  }, [searchParams, setSearchParams]);

  const filteredJobs = useMemo(() => filterCronJobsByQuery(jobs, searchQuery), [jobs, searchQuery]);

  const handleOpenCreateDialog = useCallback(() => {
    setLockedCreateConversationId(undefined);
    setCreateDialogVisible(true);
  }, []);

  const handleCloseCreateDialog = useCallback(() => {
    setCreateDialogVisible(false);
    setLockedCreateConversationId(undefined);
  }, []);

  const handleKeepAwakeChange = useCallback(async (enabled: boolean) => {
    try {
      await setKeepAwake(enabled);
    } catch (err) {
      Message.error(String(err));
    }
  }, [setKeepAwake]);

  const handleGoToDetail = useCallback(
    (job: ICronJob) => {
      navigate(`/scheduled/${job.id}`);
    },
    [navigate]
  );

  const handleToggleEnabled = useCallback(
    async (job: ICronJob) => {
      try {
        if (job.enabled) {
          await pauseJob(job.id);
          Message.success(t('cron.pauseSuccess'));
        } else {
          await resumeJob(job.id);
          Message.success(t('cron.resumeSuccess'));
        }
      } catch (err) {
        Message.error(String(err));
      }
    },
    [pauseJob, resumeJob, t]
  );

  const handleRemoveJob = useCallback(
    async (job: ICronJob) => {
      try {
        await deleteJob(job.id);
        Message.success(t('cron.deleteSuccess'));
      } catch (err) {
        Message.error(String(err));
      }
    },
    [deleteJob, t]
  );

  return (
    <div
      className={classNames(
        'w-full min-h-full box-border overflow-y-auto',
        isMobile ? 'px-16px py-14px' : 'px-12px py-24px md:px-40px md:py-32px'
      )}
    >
      <div
        className={classNames(
          'mx-auto flex w-full max-w-800px box-border flex-col',
          isMobile ? 'gap-14px' : 'gap-16px'
        )}
      >
        <div className={classNames('flex w-full flex-col', isMobile ? 'gap-6px' : 'gap-8px')}>
          <div className='flex w-full items-start justify-between gap-12px sm:gap-16px max-[520px]:flex-wrap'>
            <h1
              className={classNames(
                'm-0 min-w-0 flex-1 font-bold text-t-primary',
                isMobile ? 'text-24px leading-[1.2]' : 'text-28px leading-[1.15]'
              )}
            >
              {t('cron.scheduledTasks')}
            </h1>
            <Button type='primary' shape='round' className='shrink-0' onClick={handleOpenCreateDialog}>
              {t('cron.page.newTask')}
            </Button>
          </div>
          <p
            className={classNames(
              'm-0 w-full text-t-secondary',
              isMobile ? 'text-13px leading-20px' : 'text-14px leading-22px'
            )}
          >
            {t('cron.page.description')}
          </p>
        </div>

        <div className='grid w-full box-border grid-cols-[minmax(0,1fr)_auto] items-center gap-x-12px gap-y-10px rounded-12px border border-solid border-[var(--color-border-2)] bg-fill-2 px-14px py-12px sm:rounded-14px sm:px-16px max-[520px]:grid-cols-1'>
          <span
            className={classNames(
              'min-w-0 text-t-primary',
              isMobile ? 'text-12px leading-18px' : 'text-13px leading-20px'
            )}
          >
            {t('cron.page.awakeBanner')}
          </span>
          <div className='justify-self-end max-[520px]:justify-self-start'>
            <Tooltip content={t('cron.page.keepAwakeTooltip')}>
              <div className='flex items-center gap-8px text-t-secondary text-12px leading-18px sm:text-13px'>
                <span>{t('cron.page.keepAwake')}</span>
                <Switch size='small' checked={keepAwake} onChange={handleKeepAwakeChange} />
              </div>
            </Tooltip>
          </div>
        </div>

        {jobs.length > 0 && (
          <Input.Search
            allowClear
            value={searchQuery}
            onChange={setSearchQuery}
            placeholder={t('cron.page.searchPlaceholder', { defaultValue: '搜索任务名称、会话、指令或调度规则' })}
            className='w-full [&_.arco-input-inner-wrapper]:!rounded-full [&_.arco-input-inner-wrapper]:!border [&_.arco-input-inner-wrapper]:!border-solid [&_.arco-input-inner-wrapper]:!border-[var(--color-border-2)] [&_.arco-input-inner-wrapper:hover]:!border-[var(--color-border-3)] [&_.arco-input-inner-wrapper-focus]:!border-[rgb(var(--primary-6))]'
          />
        )}

        {loading ? (
          <div className='flex min-h-220px items-center justify-center rounded-16px border border-dashed border-border-2 bg-fill-1'>
            <Spin />
          </div>
        ) : jobs.length === 0 ? (
          <div className='flex min-h-220px items-center justify-center rounded-16px border border-dashed border-border-2 bg-fill-1'>
            <Empty description={t('cron.noTasks')} />
          </div>
        ) : filteredJobs.length === 0 ? (
          <div className='flex min-h-220px items-center justify-center rounded-16px border border-dashed border-border-2 bg-fill-1'>
            <Empty
              description={t('cron.page.noSearchResults', {
                defaultValue: '没有匹配的定时任务',
              })}
            />
          </div>
        ) : (
          <div className='w-full'>
            <div
              className='hidden items-center gap-16px border-b border-b-solid border-b-[var(--color-border-2)] px-18px py-10px text-12px font-medium leading-18px text-t-tertiary md:grid'
              style={{ gridTemplateColumns: DESKTOP_SCHEDULED_TASK_COLUMNS }}
            >
              <span>{t('cron.page.list.task')}</span>
              <span>{t('cron.nextRun')}</span>
              <span>{t('cron.page.list.status')}</span>
              <span>{t('cron.page.form.executionMode')}</span>
            </div>

            <div className='grid w-full grid-cols-1 items-start gap-12px md:block md:divide-y md:divide-solid md:divide-[var(--color-border-2)]'>
              {filteredJobs.map((job) => {
                const agentMeta = getJobAgentMeta(job, cliAgents);
                const isManualOnly = job.schedule.kind === 'cron' && !job.schedule.expr;
                const executionModeLabel =
                  job.target.execution_mode === 'new_conversation'
                    ? t('cron.page.form.newConversation')
                    : t('cron.page.form.existingConversation');

                return (
                  <div
                    key={job.id}
                    className='group flex cursor-pointer flex-col rounded-12px border border-solid border-[var(--color-border-2)] bg-fill-1 px-16px py-16px transition-colors duration-200 hover:border-[var(--color-border-3)] hover:shadow-sm md:grid md:min-h-48px md:items-center md:gap-16px md:rounded-0 md:border-0 md:bg-transparent md:px-18px md:py-8px md:hover:bg-fill-2 md:hover:shadow-none'
                    style={{ gridTemplateColumns: DESKTOP_SCHEDULED_TASK_COLUMNS }}
                    onClick={() => handleGoToDetail(job)}
                  >
                    <div className='mb-12px flex items-center justify-between gap-8px md:mb-0 md:contents'>
                      {/* Name truncates on its own; #N sits outside the truncating span so it never gets clipped. */}
                      <span className='mr-8px flex min-w-0 flex-1 items-center gap-6px md:mr-0' title={job.name}>
                        <span className='min-w-0 truncate text-14px font-medium leading-20px text-t-primary'>
                          {job.name}
                        </span>
                        <span className='shrink-0 text-12px font-normal text-t-tertiary'>
                          {shortSessionId(job.id)}
                        </span>
                      </span>
                      <div className='shrink-0 md:[grid-column:3] md:[grid-row:1] md:justify-self-start'>
                        <CronStatusTag job={job} />
                      </div>
                    </div>

                    <div
                      className='min-w-0 break-words text-13px leading-20px text-t-secondary md:hidden'
                      title={formatSchedule(job, t)}
                    >
                      {formatSchedule(job, t)}
                    </div>

                    <div
                      className='mt-16px min-w-0 break-words text-13px leading-20px text-t-secondary md:mt-0 md:truncate md:[grid-column:2] md:[grid-row:1]'
                      title={
                        job.state.next_run_at_ms
                          ? `${t('cron.nextRun')} ${formatNextRun(job.state.next_run_at_ms)}`
                          : '-'
                      }
                    >
                      {job.state.next_run_at_ms ? (
                        <>
                          <span className='md:hidden'>{t('cron.nextRun')} </span>
                          {formatNextRun(job.state.next_run_at_ms)}
                        </>
                      ) : (
                        '-'
                      )}
                    </div>

                    <div className='mt-14px flex items-center justify-between gap-10px md:mt-0 md:contents'>
                      <div className='flex min-w-0 items-center gap-6px text-12px leading-18px text-t-secondary md:[grid-column:4] md:[grid-row:1] md:text-13px md:leading-20px'>
                        {agentMeta.name ? (
                          <Tooltip content={agentMeta.name}>
                            <div className='flex h-16px w-16px shrink-0 items-center justify-center text-t-secondary md:h-18px md:w-18px'>
                              {agentMeta.logo ? (
                                <img
                                  src={agentMeta.logo}
                                  alt={agentMeta.name}
                                  className='h-16px w-16px shrink-0 rounded-50% md:h-18px md:w-18px'
                                />
                              ) : (
                                <span className='flex h-16px w-16px items-center justify-center rounded-50% text-10px font-medium text-t-secondary md:h-18px md:w-18px'>
                                  {agentMeta.name.slice(0, 1)}
                                </span>
                              )}
                            </div>
                          </Tooltip>
                        ) : null}
                        <span className='min-w-0 truncate' title={executionModeLabel}>
                          {executionModeLabel}
                        </span>
                      </div>

                      <div className='shrink-0 md:hidden' onClick={(event) => event.stopPropagation()}>
                        {!isManualOnly && (
                          <Switch size='small' checked={job.enabled} onChange={() => handleToggleEnabled(job)} />
                        )}
                      </div>

                      <ScheduledTaskActions
                        enabled={job.enabled}
                        isManualOnly={isManualOnly}
                        onToggle={() => handleToggleEnabled(job)}
                        onRemove={() => handleRemoveJob(job)}
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}

        <CreateTaskDialog
          visible={createDialogVisible}
          onClose={handleCloseCreateDialog}
          initialSpecifiedConversationId={lockedCreateConversationId}
          lockInitialTarget={lockedCreateConversationId != null}
        />
      </div>
    </div>
  );
};

export default ScheduledTasksPage;
