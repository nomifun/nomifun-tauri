import { ipcBridge } from '@/common';
import type { IProvider, ModelTask } from '@/common/config/storage';
import ModalHOC from '@/renderer/utils/ui/ModalHOC';
import NomiModal from '@/renderer/components/base/NomiModal';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { Button, Checkbox, Select, Tag } from '@arco-design/web-react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { mutate as mutateSWR } from 'swr';
import useModeModeList from '@renderer/hooks/agent/useModeModeList';
import { MODEL_PROFILES_SWR_KEY } from '@renderer/hooks/agent/useModelProfiles';
import { buildModelProfileUpsertRequest, MODEL_TASK_ORDER } from '@renderer/hooks/agent/modelProfileEditing';
import {
  isNewApiPlatform,
  NEW_API_PROTOCOL_OPTIONS,
  detectNewApiProtocol,
} from '@/renderer/utils/model/modelPlatforms';
import { ContextLimitSelect } from './ContextLimitSelect';

const AddModelModal = ModalHOC<{ data?: IProvider; onSubmit: (model: IProvider) => void }>(
  ({ modalProps, data, onSubmit, modalCtrl }) => {
    const { t } = useTranslation();
    const [message, messageHolder] = useArcoMessage();
    const [model, setModel] = useState('');
    const [modelProtocol, setModelProtocol] = useState<string>('openai');
    const [contextLimit, setContextLimit] = useState<number | undefined>();
    const [tasks, setTasks] = useState<ModelTask[]>([]);
    const [visionInput, setVisionInput] = useState(false);
    const isNewApi = isNewApiPlatform(data?.platform ?? '');
    const taskOptions = useMemo(
      () => MODEL_TASK_ORDER.map((v) => ({ label: t(`settings.modelTask.${v}`), value: v })),
      [t]
    );
    const { data: modelList, isLoading } = useModeModeList(data?.platform ?? '', data?.base_url, data?.api_key);
    const existingModels = data?.models || [];
    const optionsList = useMemo(() => {
      // 处理新的数据格式，可能包含 fix_base_url
      const models = Array.isArray(modelList) ? modelList : modelList?.models || [];
      if (!models || !data?.models) return models;
      return models.map((item) => {
        return { ...item, disabled: data.models.includes(item.value) };
      });
    }, [modelList, data?.models]);
    const previewModels = useMemo(() => existingModels.slice(0, 6), [existingModels]);
    const remainingCount =
      existingModels.length > previewModels.length ? existingModels.length - previewModels.length : 0;

    useEffect(() => {
      if (modalProps.visible) {
        setModel('');
        setModelProtocol('openai');
        setContextLimit(undefined);
        setTasks([]);
        setVisionInput(false);
      }
    }, [modalProps.visible]);

    const handleConfirm = useCallback(async () => {
      if (!model || !data) return;
      const nextContextLimits = { ...data.model_context_limits };
      if (contextLimit && contextLimit > 0) {
        nextContextLimits[model] = contextLimit;
      } else {
        delete nextContextLimits[model];
      }

      const updatedData: IProvider = {
        ...data,
        models: [...existingModels, model],
        model_context_limits: Object.keys(nextContextLimits).length > 0 ? nextContextLimits : undefined,
      };

      // new-api 平台：添加模型协议配置 / new-api platform: add model protocol config
      if (isNewApi) {
        updatedData.model_protocols = { ...data?.model_protocols, [model]: modelProtocol };
      }

      onSubmit(updatedData);

      // Persist the authoritative capability profile for the new model so probing
      // and dispatch pick the correct endpoint (source=user = authoritative).
      try {
        const selectedTraits = tasks.includes('chat') && visionInput ? (['vision_input'] as const) : [];
        await ipcBridge.modelProfile.upsert.invoke({
          ...buildModelProfileUpsertRequest(data.id, model, tasks, [...selectedTraits]),
        });
        void mutateSWR(MODEL_PROFILES_SWR_KEY);
      } catch (e) {
        console.error('model profile upsert failed', e);
        message.warning(t('settings.saveModelConfigFailed', { defaultValue: '模型能力保存失败' }));
      }
      modalCtrl.close();
    }, [contextLimit, data, existingModels, model, modelProtocol, isNewApi, tasks, visionInput, onSubmit, modalCtrl, message, t]);

    return (
      <>
        {messageHolder}
        <NomiModal
          visible={modalProps.visible}
          onCancel={modalCtrl.close}
          header={{ title: t('settings.addModel'), showClose: true }}
          style={{ maxHeight: '90vh' }}
          contentStyle={{
            background: 'var(--dialog-fill-0)',
            borderRadius: 16,
            padding: '20px 24px',
            overflow: 'auto',
          }}
          onOk={handleConfirm}
          okText={t('common.confirm')}
          cancelText={t('common.cancel')}
          okButtonProps={{ disabled: !model }}
        >
        <div className='flex flex-col gap-16px pt-20px'>
          <div className='space-y-8px'>
            <div className='text-13px font-500 text-t-secondary'>{t('settings.addModelPlaceholder')}</div>
            <Select
              showSearch
              options={optionsList}
              loading={isLoading}
              onChange={(value: string) => {
                setModel(value);
                setTasks([]);
                setVisionInput(false);
                if (isNewApi) setModelProtocol(detectNewApiProtocol(value));
              }}
              value={model}
              allowCreate
              placeholder={t('settings.addModelPlaceholder')}
            ></Select>
          </div>

          <div className='space-y-8px'>
            <div className='text-13px font-500 text-t-secondary'>
              {t('settings.contextLimit', { defaultValue: '上下文窗口 (tokens)' })}
            </div>
            <ContextLimitSelect value={contextLimit} onChange={setContextLimit} />
          </div>

          {/* 模态能力 / Modality — declares what the model does; probing & dispatch pick the endpoint from this. */}
          <div className='space-y-8px'>
            <div className='text-13px font-500 text-t-secondary'>{t('settings.modelModality')}</div>
            <Select
              mode='multiple'
              value={tasks}
              onChange={(v: ModelTask[]) => setTasks(v)}
              options={taskOptions}
              placeholder={t('settings.modelModality')}
              triggerProps={{ getPopupContainer: (node) => node.parentElement || document.body }}
            />
            <div className='text-11px text-t-secondary leading-4'>{t('settings.modelModalityTip')}</div>
            {tasks.includes('chat') && (
              <Checkbox checked={visionInput} onChange={setVisionInput} className='!pl-0'>
                <span className='text-12px text-t-secondary'>{t('settings.modelVisionInput')}</span>
              </Checkbox>
            )}
          </div>

          {/* New API 协议选择 / New API Protocol Selection */}
          {isNewApi && (
            <div className='space-y-8px'>
              <div className='text-13px font-500 text-t-secondary'>{t('settings.modelProtocol')}</div>
              <Select
                value={modelProtocol}
                onChange={setModelProtocol}
                options={NEW_API_PROTOCOL_OPTIONS}
                triggerProps={{ getPopupContainer: (node) => node.parentElement || document.body }}
              />
              <div className='text-11px text-t-secondary leading-4'>{t('settings.modelProtocolTip')}</div>
            </div>
          )}

          <div className='space-y-8px'>
            {/* <div className='text-13px font-500 text-t-secondary'>{t('settings.current_modelsLabel')}</div>
          {existingModels.length === 0 ? (
            <div className='text-13px text-t-secondary bg-fill-1 rd-8px px-12px py-14px border border-dashed border-border-2'>{t('settings.addModelNoExisting')}</div>
          ) : (
            <div className='flex flex-wrap gap-8px bg-1 rd-8px px-12px py-10px border border-solid border-border-2'>
              {previewModels.map((item) => (
                <Tag key={item} bordered={false} className='text-12px !bg-primary-1 !text-primary-6'>
                  {item}
                </Tag>
              ))}
              {remainingCount > 0 && <Tag bordered>{t('settings.addModelMoreCount', { count: remainingCount })}</Tag>}
            </div>
          )} */}
          </div>

          {/* <div className='text-12px tet-t-tertiary leading-5 bg-fill-1 rd-8px px-12px py-10px border border-dashed border-border-2'>{t('settings.addModelTips')}</div> */}
        </div>
        {/* <div className='text-12px text-t-secondary leading-5 my-4'>{model ? t('settings.addModelSelectedHint', { model }) : t('settings.addModelHint')}</div> */}
        </NomiModal>
      </>
    );
  }
);

export default AddModelModal;
