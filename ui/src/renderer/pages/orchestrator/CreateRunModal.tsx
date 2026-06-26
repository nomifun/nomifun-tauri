/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Form, Input, Select } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { TFleet, TOrchWorkspace } from '@/common/types/orchestrator/orchestratorTypes';
import NomiModal from '@/renderer/components/base/NomiModal';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

type CreateRunFormValues = {
  workspace_id: string;
  fleet_id: string;
  goal: string;
  autonomy: string;
};

/**
 * New-run modal: workspace + fleet selectors, a goal textarea and an autonomy
 * picker, wired to `ipcBridge.orchestrator.runs.create`. On success it reports
 * the created run id back via `onCreated` so the parent can navigate to it.
 */
const CreateRunModal: React.FC<{
  visible: boolean;
  workspaces: TOrchWorkspace[];
  fleets: TFleet[];
  /** Pre-selected workspace (the one currently being viewed in the list). */
  defaultWorkspaceId?: string;
  onClose: () => void;
  onCreated: (runId: string) => void;
}> = ({ visible, workspaces, fleets, defaultWorkspaceId, onClose, onCreated }) => {
  const { t } = useTranslation();
  const [form] = Form.useForm<CreateRunFormValues>();
  const [message, ctx] = useArcoMessage();
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    if (visible) {
      form.resetFields();
      form.setFieldsValue({
        autonomy: 'supervised',
        workspace_id: defaultWorkspaceId ?? workspaces[0]?.id,
      });
    }
  }, [visible, form, defaultWorkspaceId, workspaces]);

  const handleSubmit = async () => {
    try {
      const values = await form.validate();
      setSubmitting(true);
      const run = await ipcBridge.orchestrator.runs.create.invoke({
        workspace_id: values.workspace_id,
        goal: values.goal.trim(),
        fleet_id: values.fleet_id,
        autonomy: values.autonomy,
      });
      message.success(t('orchestrator.run.modal.createSuccess'));
      onCreated(run.id);
      onClose();
    } catch (e) {
      // form.validate() rejects with a field-errors map (no message) on
      // validation failure; only surface real backend errors as a toast.
      if (e instanceof Error || typeof e === 'string') {
        message.error(t('orchestrator.run.modal.createError', { error: String(e) }));
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <>
      {ctx}
      <NomiModal
        visible={visible}
        size='medium'
        header={t('orchestrator.run.modal.title')}
        onCancel={onClose}
        onOk={() => void handleSubmit()}
        confirmLoading={submitting}
        cancelText={t('orchestrator.run.modal.cancel')}
        okText={t('orchestrator.run.modal.confirm')}
        contentStyle={{ padding: '20px 24px 4px' }}
      >
        <Form form={form} layout='vertical'>
          <Form.Item
            field='workspace_id'
            label={t('orchestrator.run.modal.workspace')}
            rules={[{ required: true, message: t('orchestrator.run.modal.workspaceRequired') }]}
          >
            <Select
              placeholder={t('orchestrator.run.modal.workspacePlaceholder')}
              options={workspaces.map((w) => ({ label: w.name, value: w.id }))}
            />
          </Form.Item>
          <Form.Item
            field='fleet_id'
            label={t('orchestrator.run.modal.fleet')}
            rules={[{ required: true, message: t('orchestrator.run.modal.fleetRequired') }]}
          >
            <Select
              placeholder={t('orchestrator.run.modal.fleetPlaceholder')}
              options={fleets.map((f) => ({ label: f.name, value: f.id }))}
            />
          </Form.Item>
          <Form.Item
            field='goal'
            label={t('orchestrator.run.modal.goal')}
            rules={[
              {
                validator: (value: string | undefined, callback) => {
                  if (!value || !value.trim()) {
                    callback(t('orchestrator.run.modal.goalRequired'));
                    return;
                  }
                  callback();
                },
              },
            ]}
          >
            <Input.TextArea
              placeholder={t('orchestrator.run.modal.goalPlaceholder')}
              autoSize={{ minRows: 3, maxRows: 8 }}
            />
          </Form.Item>
          <Form.Item field='autonomy' label={t('orchestrator.run.modal.autonomy')}>
            <Select
              options={[
                { label: t('orchestrator.run.modal.autonomyAutonomous'), value: 'autonomous' },
                { label: t('orchestrator.run.modal.autonomySupervised'), value: 'supervised' },
                { label: t('orchestrator.run.modal.autonomyInteractive'), value: 'interactive' },
              ]}
            />
          </Form.Item>
        </Form>
      </NomiModal>
    </>
  );
};

export default CreateRunModal;
