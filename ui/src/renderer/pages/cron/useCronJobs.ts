/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { ICronJob, ICronJobRun } from '@/common/adapter/ipcBridge';
import {
  indexCronJobsByConversation,
  reconcileCronJobsForConversation,
  upsertCronJobByConversation,
} from './cronJobConversationMap';
import { parseConversationId, type ConversationId, type CronJobId } from '@/common/types/ids';
import { emitter } from '@/renderer/utils/emitter';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { repairCronJobTimeZones } from '@renderer/pages/cron/repairCronJobTimeZone';
import { browserStorageGenerationKey } from '@/common/utils/browserStorageKey';

const isJobErrorLike = (job: ICronJob): boolean => {
  return job.state.last_status === 'error' || job.state.last_status === 'missed';
};

/**
 * Common cron job actions
 */
interface CronJobActionsResult {
  pauseJob: (cron_job_id: CronJobId) => Promise<void>;
  resumeJob: (cron_job_id: CronJobId) => Promise<void>;
  deleteJob: (cron_job_id: CronJobId) => Promise<void>;
  updateJob: (cron_job_id: CronJobId, updates: Partial<ICronJob>) => Promise<ICronJob>;
}

/**
 * Creates common cron job action handlers
 */
function useCronJobActions(
  onJobUpdated?: (cron_job_id: CronJobId, job: ICronJob) => void,
  onJobDeleted?: (cron_job_id: CronJobId) => void
): CronJobActionsResult {
  const pauseJob = useCallback(
    async (cron_job_id: CronJobId) => {
      const updated = await ipcBridge.cron.updateJob.invoke({ cron_job_id, updates: { enabled: false } });
      onJobUpdated?.(cron_job_id, updated);
    },
    [onJobUpdated]
  );

  const resumeJob = useCallback(
    async (cron_job_id: CronJobId) => {
      const updated = await ipcBridge.cron.updateJob.invoke({ cron_job_id, updates: { enabled: true } });
      onJobUpdated?.(cron_job_id, updated);
    },
    [onJobUpdated]
  );

  const deleteJob = useCallback(
    async (cron_job_id: CronJobId) => {
      await ipcBridge.cron.removeJob.invoke({ cron_job_id });
      onJobDeleted?.(cron_job_id);
    },
    [onJobDeleted]
  );

  const updateJob = useCallback(
    async (cron_job_id: CronJobId, updates: Partial<ICronJob>) => {
      const updated = await ipcBridge.cron.updateJob.invoke({ cron_job_id, updates });
      onJobUpdated?.(cron_job_id, updated);
      return updated;
    },
    [onJobUpdated]
  );

  return { pauseJob, resumeJob, deleteJob, updateJob };
}

/**
 * Event handlers for cron job subscription
 */
interface CronJobEventHandlers {
  onJobCreated: (job: ICronJob) => void;
  onJobUpdated: (job: ICronJob) => void;
  onJobRemoved: (data: { cron_job_id: CronJobId }) => void;
}

/**
 * Subscribe to cron job events with unified cleanup
 */
function useCronJobSubscription(handlers: CronJobEventHandlers) {
  useEffect(() => {
    const unsubCreate = ipcBridge.cron.onJobCreated.on(handlers.onJobCreated);
    const unsubUpdate = ipcBridge.cron.onJobUpdated.on(handlers.onJobUpdated);
    const unsubRemove = ipcBridge.cron.onJobRemoved.on(handlers.onJobRemoved);

    return () => {
      unsubCreate();
      unsubUpdate();
      unsubRemove();
    };
  }, [handlers.onJobCreated, handlers.onJobUpdated, handlers.onJobRemoved]);
}

/**
 * Hook for managing cron jobs for a specific conversation
 * @param conversation_id - The conversation ID to fetch jobs for
 */
export function useCronJobs(conversation_id?: ConversationId) {
  const [jobs, setJobs] = useState<ICronJob[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<Error | null>(null);

  // Fetch jobs for the conversation
  const fetchJobs = useCallback(async () => {
    if (conversation_id == null) {
      setJobs([]);
      return;
    }

    setLoading(true);
    setError(null);

    try {
      const result = await ipcBridge.cron.listJobsByConversation.invoke({ conversation_id });
      setJobs(await repairCronJobTimeZones(result || []));
    } catch (err) {
      setError(err instanceof Error ? err : new Error('Failed to fetch cron jobs'));
      setJobs([]);
    } finally {
      setLoading(false);
    }
  }, [conversation_id]);

  // Initial fetch
  useEffect(() => {
    void fetchJobs();
  }, [fetchJobs]);

  // Event handlers
  const eventHandlers = useMemo<CronJobEventHandlers>(
    () => ({
      onJobCreated: (job: ICronJob) => {
        if (conversation_id && job.metadata.conversation_id === conversation_id) {
          setJobs((prev) => (prev.some((j) => j.cron_job_id === job.cron_job_id) ? prev : [...prev, job]));
        }
      },
      onJobUpdated: (job: ICronJob) => {
        if (!conversation_id) return;
        setJobs((prev) => reconcileCronJobsForConversation(prev, conversation_id, job));
      },
      onJobRemoved: ({ cron_job_id }: { cron_job_id: CronJobId }) => {
        setJobs((prev) => prev.filter((j) => j.cron_job_id !== cron_job_id));
      },
    }),
    [conversation_id]
  );

  useCronJobSubscription(eventHandlers);

  // Actions (without local state updates, rely on events)
  const actions = useCronJobActions();

  // Computed values
  const hasJobs = jobs.length > 0;
  const activeJobsCount = jobs.filter((j) => j.enabled).length;
  const hasError = jobs.some(isJobErrorLike);

  return {
    jobs,
    loading,
    error,
    hasJobs,
    activeJobsCount,
    hasError,
    refetch: fetchJobs,
    ...actions,
  };
}

/**
 * Hook for managing all cron jobs across all conversations
 */
export function useAllCronJobs() {
  const [jobs, setJobs] = useState<ICronJob[]>([]);
  const [loading, setLoading] = useState(true);

  // Fetch all jobs
  const fetchJobs = useCallback(async () => {
    setLoading(true);
    try {
      const allJobs = await ipcBridge.cron.listJobs.invoke();
      setJobs(await repairCronJobTimeZones(allJobs || []));
    } catch (err) {
      console.error('[useAllCronJobs] Failed to fetch jobs:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  // Initial fetch
  useEffect(() => {
    void fetchJobs();
  }, [fetchJobs]);

  // Event handlers
  const eventHandlers = useMemo<CronJobEventHandlers>(
    () => ({
      onJobCreated: (job: ICronJob) => {
        setJobs((prev) => (prev.some((j) => j.cron_job_id === job.cron_job_id) ? prev : [...prev, job]));
      },
      onJobUpdated: (job: ICronJob) => {
        setJobs((prev) => prev.map((j) => (j.cron_job_id === job.cron_job_id ? job : j)));
      },
      onJobRemoved: ({ cron_job_id }: { cron_job_id: CronJobId }) => {
        setJobs((prev) => prev.filter((j) => j.cron_job_id !== cron_job_id));
      },
    }),
    []
  );

  useCronJobSubscription(eventHandlers);

  // Actions with local state updates
  const handleJobUpdated = useCallback((cron_job_id: CronJobId, job: ICronJob) => {
    setJobs((prev) => prev.map((j) => (j.cron_job_id === cron_job_id ? job : j)));
  }, []);

  const handleJobDeleted = useCallback((cron_job_id: CronJobId) => {
    setJobs((prev) => prev.filter((j) => j.cron_job_id !== cron_job_id));
  }, []);

  const actions = useCronJobActions(handleJobUpdated, handleJobDeleted);

  // Computed values
  const activeCount = useMemo(() => jobs.filter((j) => j.enabled).length, [jobs]);
  const hasError = useMemo(() => jobs.some(isJobErrorLike), [jobs]);

  return {
    jobs,
    loading,
    activeCount,
    hasError,
    refetch: fetchJobs,
    ...actions,
  };
}

/**
 * Hook for getting cron job status for all conversations
 * Used by ChatHistory to show indicators
 */
export function useCronJobsMap() {
  const [jobsMap, setJobsMap] = useState<Map<ConversationId, ICronJob[]>>(new Map());
  const [loading, setLoading] = useState(true);
  const unreadStorageKey = browserStorageGenerationKey('cron-unread');
  // Track conversations with unread cron executions (red dot indicator)
  const [unreadConversations, setUnreadConversations] = useState<Set<ConversationId>>(() => {
    // Restore only from the current backend dataset generation. The old
    // unscoped key is intentionally not read after a hard database reset.
    try {
      const stored = localStorage.getItem(unreadStorageKey);
      if (stored) {
        const parsed = JSON.parse(stored);
        if (Array.isArray(parsed)) {
          const ids = parsed.flatMap((value) => {
            try {
              return [parseConversationId(value)];
            } catch {
              return [];
            }
          });
          return new Set(ids);
        }
      }
    } catch {
      // ignore
    }
    return new Set<ConversationId>();
  });
  // Track last_run_at_ms for each job to detect new executions
  const lastRunAtMapRef = useRef<Map<CronJobId, number>>(new Map());
  // Track current active conversation (use ref to access latest value in event handlers)
  const activeConversationIdRef = useRef<ConversationId | null>(null);

  // Persist unread state to localStorage
  useEffect(() => {
    try {
      localStorage.setItem(unreadStorageKey, JSON.stringify([...unreadConversations]));
    } catch {
      // ignore
    }
  }, [unreadConversations, unreadStorageKey]);

  // Fetch all jobs and group by conversation
  const fetchAllJobs = useCallback(async () => {
    setLoading(true);
    try {
      const allJobs = await repairCronJobTimeZones(await ipcBridge.cron.listJobs.invoke());
      const jobs = allJobs || [];
      const map = indexCronJobsByConversation(jobs);

      for (const job of jobs) {
        // Initialize lastRunAtMap for detecting new executions
        if (job.state.last_run_at_ms) {
          lastRunAtMapRef.current.set(job.cron_job_id, job.state.last_run_at_ms);
        }
      }

      setJobsMap(map);
    } catch (err) {
      console.error('[useCronJobsMap] Failed to fetch jobs:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  // Initial fetch
  useEffect(() => {
    void fetchAllJobs();
  }, [fetchAllJobs]);

  // Event handlers
  const eventHandlers = useMemo<CronJobEventHandlers>(
    () => ({
      onJobCreated: (job: ICronJob) => {
        const convId = job.metadata.conversation_id;
        if (!convId) return;
        setJobsMap((prev) => upsertCronJobByConversation(prev, job));
        // Refresh conversation list to update sorting (modifyTime was updated)
        console.log('[useCronJobsMap] onJobCreated, triggering chat.history.refresh');
        emitter.emit('chat.history.refresh');
      },
      onJobUpdated: (job: ICronJob) => {
        const convId = job.metadata.conversation_id;

        // Check if this is a new execution (last_run_at_ms changed)
        const prevLastRunAt = lastRunAtMapRef.current.get(job.cron_job_id);
        const newLastRunAt = job.state.last_run_at_ms;
        if (convId && newLastRunAt && newLastRunAt !== prevLastRunAt) {
          lastRunAtMapRef.current.set(job.cron_job_id, newLastRunAt);

          // Mark as unread only if user is not currently viewing this conversation
          // Use ref to access the latest activeConversationId value
          if (activeConversationIdRef.current !== convId) {
            setUnreadConversations((prev) => {
              if (prev.has(convId)) return prev;
              const newSet = new Set(prev);
              newSet.add(convId);
              return newSet;
            });
          }

          // Refresh conversation list to update sorting (modifyTime was updated after execution)
          emitter.emit('chat.history.refresh');
        }

        setJobsMap((prev) => upsertCronJobByConversation(prev, job));
      },
      onJobRemoved: ({ cron_job_id }: { cron_job_id: CronJobId }) => {
        setJobsMap((prev) => {
          const newMap = new Map(prev);
          for (const [convId, convJobs] of newMap.entries()) {
            const filtered = convJobs.filter((j) => j.cron_job_id !== cron_job_id);
            if (filtered.length === 0) {
              newMap.delete(convId);
            } else if (filtered.length !== convJobs.length) {
              newMap.set(convId, filtered);
            }
          }
          return newMap;
        });
      },
    }),
    []
  );

  useEffect(() => {
    const unsubCreate = ipcBridge.cron.onJobCreated.on(eventHandlers.onJobCreated);
    const unsubUpdate = ipcBridge.cron.onJobUpdated.on(eventHandlers.onJobUpdated);
    const unsubRemove = ipcBridge.cron.onJobRemoved.on(eventHandlers.onJobRemoved);

    return () => {
      unsubCreate();
      unsubUpdate();
      unsubRemove();
    };
  }, [eventHandlers]);

  // Helper functions
  const hasJobsForConversation = useCallback(
    (conversation_id: ConversationId) => {
      return jobsMap.has(conversation_id) && jobsMap.get(conversation_id)!.length > 0;
    },
    [jobsMap]
  );

  const getJobsForConversation = useCallback(
    (conversation_id: ConversationId): ICronJob[] => {
      return jobsMap.get(conversation_id) || [];
    },
    [jobsMap]
  );

  const getJobStatus = useCallback(
    (conversation_id: ConversationId): 'none' | 'active' | 'paused' | 'error' | 'unread' => {
      const convJobs = jobsMap.get(conversation_id);
      if (!convJobs || convJobs.length === 0) {
        return 'none';
      }

      // Check if conversation has unread cron executions (highest priority for visual indicator)
      if (unreadConversations.has(conversation_id)) return 'unread';

      // Check if any job has error
      if (convJobs.some(isJobErrorLike)) return 'error';

      // Check if all jobs are paused
      if (convJobs.every((j) => !j.enabled)) return 'paused';

      return 'active';
    },
    [jobsMap, unreadConversations]
  );

  // Mark a conversation as read (clear unread status)
  const markAsRead = useCallback((conversation_id: ConversationId) => {
    activeConversationIdRef.current = conversation_id;
    setUnreadConversations((prev) => {
      if (!prev.has(conversation_id)) {
        return prev;
      }
      const newSet = new Set(prev);
      newSet.delete(conversation_id);
      return newSet;
    });
  }, []);

  // Update active conversation ref without triggering state update
  // Use this to sync the ref when route changes (e.g., URL navigation)
  const setActiveConversation = useCallback((conversation_id: ConversationId) => {
    activeConversationIdRef.current = conversation_id;
  }, []);

  // Check if a conversation has unread cron executions
  const hasUnread = useCallback(
    (conversation_id: ConversationId) => {
      return unreadConversations.has(conversation_id);
    },
    [unreadConversations]
  );

  return useMemo(
    () => ({
      jobsMap,
      loading,
      hasJobsForConversation,
      getJobsForConversation,
      getJobStatus,
      markAsRead,
      setActiveConversation,
      hasUnread,
      refetch: fetchAllJobs,
    }),
    [
      jobsMap,
      loading,
      hasJobsForConversation,
      getJobsForConversation,
      getJobStatus,
      markAsRead,
      setActiveConversation,
      hasUnread,
      fetchAllJobs,
    ]
  );
}

/**
 * Hook for fetching lightweight execution records for a specific cron job.
 * Each job is pruned server-side to its latest seven runs.
 */
export function useCronJobRuns(cron_job_id: CronJobId | undefined) {
  const [runs, setRuns] = useState<ICronJobRun[]>([]);
  const [loading, setLoading] = useState(false);

  const fetchRuns = useCallback(async () => {
    if (!cron_job_id) {
      setRuns([]);
      return;
    }

    setLoading(true);
    try {
      const result = await ipcBridge.cron.listRuns.invoke({ cron_job_id: cron_job_id });
      setRuns(result || []);
    } catch (err) {
      console.error('[useCronJobRuns] Failed to fetch:', err);
      setRuns([]);
    } finally {
      setLoading(false);
    }
  }, [cron_job_id]);

  // Initial fetch
  useEffect(() => {
    void fetchRuns();
  }, [fetchRuns]);

  // Refetch when this job executes.
  useEffect(() => {
    if (!cron_job_id) return;
    const unsubExecuted = ipcBridge.cron.onJobExecuted.on((data) => {
      if (data.cron_job_id === cron_job_id) {
        void fetchRuns();
      }
    });
    return () => {
      unsubExecuted();
    };
  }, [cron_job_id, fetchRuns]);

  return { runs, loading };
}
