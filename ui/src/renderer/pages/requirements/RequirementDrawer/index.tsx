/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Unified right-side requirement drawer: view ↔ edit ↔ create in one surface,
 * replacing the old separate read-only detail drawer + full-page form.
 *
 * - `view`   reads the requirement by id and renders a read-only layout, with an
 *            "edit" affordance in the title that flips the drawer into edit mode
 *            in place (no route change).
 * - `edit`   reads the requirement by id and renders the editable RequirementForm.
 * - `create` renders a blank RequirementForm.
 *
 * `innerMode` is seeded from the `mode` prop and reset whenever the drawer
 * (re)opens or its target changes, so the host can keep the drawer mounted and
 * the view→edit switch stays local.
 */

import React, { useCallback, useEffect, useLayoutEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Descriptions, Drawer, Spin } from '@arco-design/web-react';
import { Edit } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IRequirement } from '@/common/adapter/ipcBridge';
import type { RequirementId } from '@/common/types/ids';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import CopyFullIdButton from '@/renderer/components/base/CopyFullIdButton';
import FilePreview from '@/renderer/components/media/FilePreview';
import StatusPill from '../components/StatusPill';
import RequirementDisplayNumber from '../components/RequirementDisplayNumber';
import RequirementForm, { type RequirementFormPayload } from './RequirementForm';

interface RequirementDrawerProps {
  open: boolean;
  mode: 'view' | 'edit' | 'create';
  /** Required for view / edit; ignored in create. */
  requirementId?: RequirementId;
  onClose: () => void;
  /** Notify the host to refresh its list / board after a create or update. */
  onSaved: () => void;
}

const fmtTime = (ms?: number): string => (ms ? new Date(ms).toLocaleString() : '-');

const RequirementDrawer: React.FC<RequirementDrawerProps> = ({
  open,
  mode,
  requirementId,
  onClose,
  onSaved,
}) => {
  const { t } = useTranslation();
  const [message, messageCtx] = useArcoMessage();

  const [innerMode, setInnerMode] = useState<'view' | 'edit' | 'create'>(mode);
  const [data, setData] = useState<IRequirement | null>(null);
  const [loading, setLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [formResetSignal, setFormResetSignal] = useState(0);

  // Re-seed the inner mode and clear stale data whenever the drawer (re)opens or
  // its target changes — so reopening a different requirement never flashes the
  // previous one, and a host that keeps the drawer mounted gets a clean slate.
  useLayoutEffect(() => {
    if (!open) return;
    setInnerMode(mode);
    setData(null);
    setFormResetSignal((signal) => signal + 1);
  }, [open, mode, requirementId]);

  // Fetch the requirement for view/edit. Create needs no fetch.
  const fetchRequirement = useCallback(async () => {
    if (requirementId === undefined) return;
    setLoading(true);
    try {
      const full = await ipcBridge.requirements.get.invoke({ requirement_id: requirementId });
      setData(full);
    } catch (e) {
      message.error(String(e));
    } finally {
      setLoading(false);
    }
  }, [requirementId, message]);

  // Fetch the requirement once per open/target — NOT per inner mode. View↔edit
  // flips locally and the data is already in hand, so this effect deliberately
  // does not depend on `innerMode`; re-fetching on a mode flip would flicker and,
  // after a save, double-fetch (the explicit fetchRequirement + an effect re-fire).
  // Create mode needs no fetch.
  useEffect(() => {
    if (!open) return;
    if (mode === 'create') {
      setData(null);
      return;
    }
    if (requirementId === undefined) return;
    let cancelled = false;
    setLoading(true);
    void ipcBridge.requirements.get
      .invoke({ requirement_id: requirementId })
      .then((full) => {
        if (!cancelled) setData(full);
      })
      .catch((e) => {
        if (!cancelled) message.error(String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, mode, requirementId, message]);

  const handleCreate = useCallback(
    async (payload: RequirementFormPayload) => {
      setSubmitting(true);
      try {
        await ipcBridge.requirements.create.invoke({
          title: payload.title,
          content: payload.content,
          tag: payload.tag,
          order_key: payload.order_key,
          attachments: payload.newAttachments,
        });
        message.success(t('requirements.form.createdOk'));
        onSaved();
        onClose();
      } catch (e) {
        message.error(String(e));
      } finally {
        setSubmitting(false);
      }
    },
    [message, t, onSaved, onClose]
  );

  const handleUpdate = useCallback(
    async (payload: RequirementFormPayload) => {
      if (requirementId === undefined) return;
      setSubmitting(true);
      try {
        await ipcBridge.requirements.update.invoke({
          requirement_id: requirementId,
          updates: {
            title: payload.title,
            content: payload.content,
            tag: payload.tag,
            order_key: payload.order_key,
            status: payload.status,
            add_attachments: payload.newAttachments,
            remove_attachment_ids: payload.removeAttachmentIds,
          },
        });
        message.success(t('requirements.form.updatedOk'));
        setInnerMode('view');
        await fetchRequirement();
        onSaved();
      } catch (e) {
        message.error(String(e));
      } finally {
        setSubmitting(false);
      }
    },
    [requirementId, message, t, fetchRequirement, onSaved]
  );

  const drawerTitle = (() => {
    if (innerMode === 'create') return t('requirements.form.createTitle');
    if (innerMode === 'edit') return t('requirements.form.editTitle');
    return (
      <div className='flex items-center justify-between gap-12px pr-8px'>
        <span className='min-w-0 truncate'>{data?.title || t('requirements.drawer.detailTitle')}</span>
        {data ? (
          <div
            role='button'
            tabIndex={0}
            className='inline-flex items-center gap-4px shrink-0 cursor-pointer rounded-full px-10px py-3px text-13px text-primary-6 hover:bg-primary-1 outline-none transition-colors'
            onClick={() => setInnerMode('edit')}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                setInnerMode('edit');
              }
            }}
          >
            <Edit theme='outline' size='14' />
            <span>{t('requirements.actions.edit')}</span>
          </div>
        ) : null}
      </div>
    );
  })();

  return (
    <Drawer width={480} visible={open} onCancel={onClose} footer={null} title={drawerTitle}>
      {messageCtx}
      {innerMode === 'create' ? (
        <RequirementForm
          mode='create'
          onSubmit={handleCreate}
          onCancel={onClose}
          submitting={submitting}
          resetSignal={formResetSignal}
        />
      ) : (
        <Spin loading={loading} className='block w-full'>
          {innerMode === 'edit' ? (
            data ? (
              <RequirementForm
                mode='edit'
                initial={data}
                onSubmit={handleUpdate}
                onCancel={() => setInnerMode('view')}
                submitting={submitting}
                resetSignal={formResetSignal}
              />
            ) : (
              // Keep the Spin tall enough to read while the fetch resolves.
              <div className='min-h-160px' />
            )
          ) : data ? (
            <div className='flex flex-col gap-16px'>
              <Descriptions
                column={1}
                colon=' :'
                labelStyle={{ width: 96, color: 'var(--color-text-3)' }}
                data={[
                  {
                    label: t('requirements.columns.seq'),
                    value: <RequirementDisplayNumber displayNo={data.display_no} />,
                  },
                  // The canonical ID remains available without dominating the layout.
                  { label: t('requirements.columns.id'), value: <CopyFullIdButton id={data.requirement_id} /> },
                  { label: t('requirements.columns.tag'), value: data.tag },
                  { label: t('requirements.columns.order'), value: data.order_key || '-' },
                  {
                    label: t('requirements.columns.status'),
                    value: <StatusPill status={data.status} size='sm' />,
                  },
                  { label: t('requirements.detail.createdBy'), value: data.created_by },
                  { label: t('requirements.columns.createdAt'), value: fmtTime(data.created_at) },
                  { label: t('requirements.detail.completedAt'), value: fmtTime(data.completed_at) },
                  {
                    label: t('requirements.detail.session'),
                    value: (() => {
                      const ownerId = data.owner_conversation_id ?? data.owner_terminal_id;
                      return ownerId ? <CopyFullIdButton id={ownerId} /> : '-';
                    })(),
                  },
                ]}
              />
              <div className='flex flex-col gap-4px'>
                <span className='text-t-tertiary text-12px'>{t('requirements.form.contentLabel')}</span>
                <div className='whitespace-pre-wrap break-words rounded-8px border border-solid border-border-2 bg-fill-1 p-12px text-t-primary text-13px leading-20px min-h-60px'>
                  {data.content || '-'}
                </div>
              </div>
              {(data.attachments?.length ?? 0) > 0 ? (
                <div className='flex flex-col gap-4px'>
                  <span className='text-t-tertiary text-12px'>{t('requirements.detail.attachments')}</span>
                  <div className='flex flex-wrap gap-8px'>
                    {data.attachments?.map((a) => (
                      <FilePreview key={a.id} path={a.abs_path} readonly />
                    ))}
                  </div>
                </div>
              ) : null}
              {data.completion_note ? (
                <div className='flex flex-col gap-4px'>
                  <span className='text-t-tertiary text-12px'>{t('requirements.detail.completionNote')}</span>
                  <div className='whitespace-pre-wrap break-words rounded-8px border border-solid border-border-2 bg-fill-1 p-12px text-t-primary text-13px leading-20px'>
                    {data.completion_note}
                  </div>
                </div>
              ) : null}
            </div>
          ) : (
            <div className='min-h-160px' />
          )}
        </Spin>
      )}
    </Drawer>
  );
};

export default RequirementDrawer;
