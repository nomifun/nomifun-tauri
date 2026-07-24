import { ipcBridge } from '@/common';
import { NOMI_AGENT_ID } from '@/common/config/constants';
import { parseAgentId } from '@/common/types/ids';
import type {
  TAgentExecutionTemplate,
  TAgentExecutionTemplateDetail,
} from '@/common/types/agentExecution/agentExecutionTemplateTypes';
import type { TExecutionModelRef } from '@/common/types/agentExecution/agentExecutionTypes';
import type { ExecutionTemplateId } from '@/common/types/ids';
import {
  templateContainsModel,
  toAppliedCollaborationTemplate,
  type AppliedCollaborationTemplate,
} from '@/renderer/components/collaboration/collaborationTemplateModel';
import { Button, Empty, Input, Message, Popconfirm, Spin } from '@arco-design/web-react';
import { Delete, FolderOpen, Save } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

type Props = {
  visible: boolean;
  selectedTemplateId: ExecutionTemplateId | null;
  models: TExecutionModelRef[];
  mainModel?: TExecutionModelRef | null;
  workDir?: string;
  onApply: (template: AppliedCollaborationTemplate) => void;
  onClear: () => void;
};

const pairKey = (value: TExecutionModelRef): string => `${value.provider_id}\u0000${value.model}`;

const GuidCollaborationTemplatePicker: React.FC<Props> = ({
  visible,
  selectedTemplateId,
  models,
  mainModel,
  workDir,
  onApply,
  onClear,
}) => {
  const { t } = useTranslation();
  const [templates, setTemplates] = useState<TAgentExecutionTemplate[]>([]);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [name, setName] = useState('');

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setTemplates(await ipcBridge.agentExecutionTemplate.list.invoke());
    } catch (error) {
      console.error('[GuidCollaborationTemplatePicker] Failed to list templates:', error);
      Message.error(
        t('collaboration.template.loadError', {
          defaultValue: '协作方案加载失败',
        }),
      );
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    if (visible) void refresh();
  }, [refresh, visible]);

  const currentModels = useMemo(() => {
    const all = mainModel ? [mainModel, ...models] : models;
    const seen = new Set<string>();
    return all.filter((item) => {
      const key = pairKey(item);
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
  }, [mainModel, models]);

  const apply = useCallback(
    async (template: TAgentExecutionTemplate) => {
      try {
        const detail: TAgentExecutionTemplateDetail = await ipcBridge.agentExecutionTemplate.get.invoke({
          execution_template_id: template.execution_template_id,
        });
        if (mainModel && !templateContainsModel(detail, mainModel)) {
          Message.error(
            t('collaboration.template.leadMismatch', {
              defaultValue: '该方案不包含当前主模型 {{model}}，请先切换主模型或选择其他方案。',
              model: mainModel.model,
            }),
          );
          return;
        }
        onApply(toAppliedCollaborationTemplate(detail));
        Message.success(
          t('collaboration.template.applied', {
            defaultValue: '已应用协作方案：{{name}}',
            name: detail.name,
          }),
        );
      } catch (error) {
        console.error('[GuidCollaborationTemplatePicker] Failed to apply template:', error);
        Message.error(
          t('collaboration.template.applyError', {
            defaultValue: '协作方案应用失败',
          }),
        );
      }
    },
    [mainModel, onApply, t],
  );

  const save = useCallback(async () => {
    const trimmed = name.trim();
    if (!trimmed) return;
    setSaving(true);
    try {
      const created = await ipcBridge.agentExecutionTemplate.create.invoke({
        name: trimmed,
        max_parallel: Math.min(4, Math.max(1, currentModels.length)),
        work_dir: workDir?.trim() || undefined,
        participants: currentModels.map((model, index) => ({
          source_agent_id: parseAgentId(NOMI_AGENT_ID),
          provider_id: model.provider_id,
          model: model.model,
          sort_order: index,
        })),
      });
      setName('');
      onApply(toAppliedCollaborationTemplate(created));
      await refresh();
      Message.success(
        t('collaboration.template.saved', {
          defaultValue: '协作方案已保存',
        }),
      );
    } catch (error) {
      console.error('[GuidCollaborationTemplatePicker] Failed to save template:', error);
      Message.error(
        t('collaboration.template.saveError', {
          defaultValue: '协作方案保存失败',
        }),
      );
    } finally {
      setSaving(false);
    }
  }, [currentModels, name, onApply, refresh, t, workDir]);

  const remove = useCallback(
    async (template: TAgentExecutionTemplate) => {
      try {
        await ipcBridge.agentExecutionTemplate.remove.invoke({
          execution_template_id: template.execution_template_id,
          expected_version: template.version,
        });
        if (selectedTemplateId === template.execution_template_id) onClear();
        await refresh();
        Message.success(
          t('collaboration.template.deleted', {
            defaultValue: '协作方案已删除',
          }),
        );
      } catch (error) {
        console.error('[GuidCollaborationTemplatePicker] Failed to delete template:', error);
        Message.error(
          t('collaboration.template.deleteError', {
            defaultValue: '协作方案删除失败',
          }),
        );
      }
    },
    [onClear, refresh, selectedTemplateId, t],
  );

  return (
    <div className='mt-10px border-t border-t-[var(--color-border-2)] pt-10px' data-testid='collaboration-template-picker'>
      <div className='mb-7px flex items-center justify-between gap-8px'>
        <div className='text-12px font-600 text-t-primary'>
          {t('collaboration.template.title', { defaultValue: '协作方案' })}
        </div>
        {selectedTemplateId && (
          <Button type='text' size='mini' onClick={onClear}>
            {t('collaboration.template.clear', { defaultValue: '取消使用' })}
          </Button>
        )}
      </div>

      <Spin loading={loading} className='w-full'>
        <div className='max-h-132px overflow-y-auto'>
          {templates.length === 0 && !loading ? (
            <Empty
              description={t('collaboration.template.empty', {
                defaultValue: '暂无已保存方案',
              })}
            />
          ) : (
            templates.map((template) => (
              <div
                key={template.execution_template_id}
                className='mb-4px flex items-center gap-4px rd-7px px-5px py-4px hover:bg-fill-2'
                data-selected={selectedTemplateId === template.execution_template_id}
              >
                <Button
                  type='text'
                  size='mini'
                  className='min-w-0 flex-1 justify-start'
                  icon={<FolderOpen theme='outline' size='13' />}
                  onClick={() => void apply(template)}
                >
                  <span className='truncate'>{template.name}</span>
                </Button>
                <Popconfirm
                  title={t('collaboration.template.deleteConfirm', {
                    defaultValue: '删除这个协作方案？',
                  })}
                  onOk={() => remove(template)}
                >
                  <Button
                    type='text'
                    size='mini'
                    status='danger'
                    aria-label={t('collaboration.template.delete', { defaultValue: '删除方案' })}
                    icon={<Delete theme='outline' size='13' />}
                  />
                </Popconfirm>
              </div>
            ))
          )}
        </div>
      </Spin>

      <div className='mt-7px flex gap-6px'>
        <Input
          size='mini'
          value={name}
          onChange={setName}
          onPressEnter={() => void save()}
          placeholder={t('collaboration.template.namePlaceholder', {
            defaultValue: '保存当前配置为方案',
          })}
          maxLength={80}
        />
        <Button
          size='mini'
          type='primary'
          loading={saving}
          disabled={!name.trim() || currentModels.length === 0}
          icon={<Save theme='outline' size='13' />}
          onClick={() => void save()}
        >
          {t('collaboration.template.save', { defaultValue: '保存' })}
        </Button>
      </div>
    </div>
  );
};

export default GuidCollaborationTemplatePicker;
