/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * The editable requirement form, reused by RequirementDrawer for both the
 * `create` and `edit` flows. Persistence is owned by the host: this component
 * only validates the fields, gathers the attachment refs, and hands a flat
 * payload to `onSubmit`. It never calls requirement create/update IPC itself.
 *
 * Attachments are delegated to the sibling `AttachmentsField` (drag / paste /
 * click-to-upload, image-only validation, temp→persistent refs). We track the
 * newly-added refs and the ids of existing attachments queued for removal, and
 * wire AttachmentsField's `onUploadingChange` to disable Save while uploads are
 * in flight.
 */

import React, { useCallback, useLayoutEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Form, Input, Select } from '@arco-design/web-react';
import type {
  IAttachment,
  INewAttachmentRef,
  IRequirement,
  RequirementStatus,
} from '@/common/adapter/ipcBridge';
import { useRequirementTags } from '../useRequirements';
import AttachmentsField from './AttachmentsField';
import type { AttachmentId } from '@/common/types/ids';

/**
 * User-editable requirement statuses, in stable display order.
 * `in_progress` is minted and settled exclusively by the execution lease.
 */
const EDITABLE_STATUSES: RequirementStatus[] = [
  'pending',
  'needs_review',
  'done',
  'failed',
  'cancelled',
];

export interface RequirementFormPayload {
  title: string;
  content: string;
  tag: string;
  order_key?: string;
  /** Only meaningful in edit mode. */
  status?: RequirementStatus;
  newAttachments: INewAttachmentRef[];
  removeAttachmentIds: AttachmentId[];
}

interface RequirementFormProps {
  mode: 'create' | 'edit';
  /** Present in edit mode — seeds the fields and the existing-attachments list. */
  initial?: IRequirement;
  onSubmit: (payload: RequirementFormPayload) => Promise<void>;
  onCancel: () => void;
  submitting?: boolean;
  resetSignal?: number;
}

interface FormValues {
  title?: string;
  content?: string;
  tag?: string;
  order_key?: string;
  status?: RequirementStatus;
}

const RequirementForm: React.FC<RequirementFormProps> = ({
  mode,
  initial,
  onSubmit,
  onCancel,
  submitting = false,
  resetSignal = 0,
}) => {
  const { t } = useTranslation();
  const [form] = Form.useForm<FormValues>();
  const { tags } = useRequirementTags();

  const isEdit = mode === 'edit';
  const isExecutionOwnedStatus = isEdit && initial?.status === 'in_progress';

  // Newly-uploaded attachment refs (temp paths). Existing attachments come from
  // `initial.attachments`; ids queued for removal live in `removeAttachmentIds`.
  const [newAttachments, setNewAttachments] = useState<INewAttachmentRef[]>([]);
  const [removeAttachmentIds, setRemoveAttachmentIds] = useState<AttachmentId[]>([]);
  const [uploading, setUploading] = useState(false);

  // Existing attachments still shown (i.e. not queued for removal).
  const existing = useMemo<IAttachment[]>(() => {
    const all = initial?.attachments ?? [];
    return all.filter((a) => !removeAttachmentIds.includes(a.id));
  }, [initial?.attachments, removeAttachmentIds]);

  const initialValues = useMemo<FormValues>(() => {
    if (isEdit && initial) {
      return {
        title: initial.title,
        content: initial.content,
        tag: initial.tag,
        order_key: initial.order_key,
        status: initial.status,
      };
    }
    return { status: 'pending' };
  }, [isEdit, initial]);

  useLayoutEffect(() => {
    const nextValues: FormValues = {
      title: undefined,
      content: undefined,
      tag: undefined,
      order_key: undefined,
      status: undefined,
      ...initialValues,
    };
    form.resetFields();
    form.setFieldsValue(nextValues);
    setNewAttachments([]);
    setRemoveAttachmentIds([]);
    setUploading(false);
  }, [form, initialValues, resetSignal]);

  const tagOptions = useMemo(() => tags.map((tg) => ({ label: tg.tag, value: tg.tag })), [tags]);

  const handleRemoveExisting = useCallback((id: AttachmentId) => {
    setRemoveAttachmentIds((prev) => (prev.includes(id) ? prev : [...prev, id]));
  }, []);

  const handleSave = useCallback(async () => {
    let values: FormValues;
    try {
      values = await form.validate();
    } catch {
      // arco surfaces the per-field validation messages itself.
      return;
    }
    const payload: RequirementFormPayload = {
      title: (values.title ?? '').trim(),
      content: values.content ?? '',
      tag: values.tag ?? '',
      order_key: values.order_key,
      newAttachments,
      removeAttachmentIds,
    };
    // Status is execution authority, not ordinary form metadata.  Sending the
    // value that merely seeded this form can race an AutoWork verdict and
    // replay a stale `in_progress`/`pending` value after the row is already
    // terminal.  Omit unchanged status so the backend's dedicated status CAS
    // is entered only for an explicit user transition.
    if (isEdit && !isExecutionOwnedStatus && values.status !== initial?.status) {
      payload.status = values.status;
    }
    await onSubmit(payload);
  }, [
    form,
    initial?.status,
    isEdit,
    isExecutionOwnedStatus,
    newAttachments,
    removeAttachmentIds,
    onSubmit,
  ]);

  return (
    <Form<FormValues> form={form} layout='vertical' initialValues={initialValues} className='w-full'>
      <Form.Item
        label={t('requirements.form.titleLabel')}
        field='title'
        rules={[{ required: true, message: t('requirements.form.titleRequired') }]}
      >
        <Input placeholder={t('requirements.form.titlePlaceholder')} />
      </Form.Item>

      <Form.Item label={t('requirements.form.contentLabel')} field='content'>
        <Input.TextArea
          autoSize={{ minRows: 4, maxRows: 16 }}
          placeholder={t('requirements.form.contentPlaceholder')}
        />
      </Form.Item>

      <Form.Item
        label={t('requirements.form.tagLabel')}
        field='tag'
        rules={[{ required: true, message: t('requirements.form.tagRequired') }]}
      >
        <Select allowCreate placeholder={t('requirements.form.tagPlaceholder')} options={tagOptions} />
      </Form.Item>

      <Form.Item
        label={t('requirements.form.orderLabel')}
        field='order_key'
        extra={t('requirements.form.orderHelp')}
      >
        <Input placeholder='1.2' />
      </Form.Item>

      {isEdit ? (
        <Form.Item label={t('requirements.form.statusLabel')} field='status'>
          <Select
            disabled={isExecutionOwnedStatus}
            options={(isExecutionOwnedStatus ? ['in_progress'] : EDITABLE_STATUSES).map((s) => ({
              label: t(`requirements.status.${s}`),
              value: s,
            }))}
          />
        </Form.Item>
      ) : null}

      <Form.Item>
        <AttachmentsField
          key={`attachments-${mode}-${initial?.requirement_id ?? 'new'}-${resetSignal}`}
          value={newAttachments}
          onChange={setNewAttachments}
          existing={existing}
          onRemoveExisting={handleRemoveExisting}
          onUploadingChange={setUploading}
        />
      </Form.Item>

      <div className='flex gap-8px'>
        <Button
          type='primary'
          shape='round'
          loading={submitting}
          disabled={uploading || submitting}
          onClick={handleSave}
        >
          {t('requirements.form.submit')}
        </Button>
        <Button shape='round' disabled={submitting} onClick={onCancel}>
          {t('requirements.form.cancel')}
        </Button>
      </div>
    </Form>
  );
};

export default RequirementForm;
