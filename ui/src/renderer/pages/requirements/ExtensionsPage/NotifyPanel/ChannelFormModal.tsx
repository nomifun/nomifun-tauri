import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Checkbox, Form, Input, Modal, Select, Switch } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { IWebhook } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

type ChannelFormModalProps = {
  visible: boolean;
  editing: IWebhook | null;
  onClose: () => void;
  onSuccess: () => void;
};

/**
 * Notification channel create/edit modal.
 *
 * Ported from the settings WebhookFormModal, with three differences:
 * - Platform Select offers three options: lark / http / slack (backend WebhookPlatform).
 * - The signing secret field + "clear secret" checkbox are LARK-ONLY. When the platform
 *   is not 'lark', both are hidden and the secret is omitted (undefined) on submit.
 * - Uses the project's useArcoMessage wrapper instead of the global Message.
 *
 * Secret edit UX decision (lark only):
 * - On CREATE: user enters the secret directly (optional).
 * - On EDIT: the secret field is empty by default (keeps existing secret).
 *   A "清除密钥" checkbox appears; when checked, the secret is explicitly set to null (clear).
 *   If the user types a new secret value, it replaces the existing one.
 *   This three-state approach (omit=keep, null=clear, string=set) maps directly to the API.
 */
const ChannelFormModal: React.FC<ChannelFormModalProps> = ({ visible, editing, onClose, onSuccess }) => {
  const { t } = useTranslation();
  const [form] = Form.useForm();
  const [message, ctx] = useArcoMessage();
  const [submitting, setSubmitting] = useState(false);
  const [clearSecret, setClearSecret] = useState(false);

  const isEdit = editing != null;

  const platform = Form.useWatch('platform', form);
  const isLark = platform === 'lark';

  useEffect(() => {
    if (visible) {
      if (editing) {
        form.setFieldsValue({
          name: editing.name,
          platform: editing.platform,
          url: editing.url,
          description: editing.description,
          enabled: editing.enabled,
          secret: '',
        });
      } else {
        form.resetFields();
        form.setFieldsValue({ platform: 'lark', enabled: true });
      }
      setClearSecret(false);
    }
  }, [visible, editing, form]);

  const handleSubmit = async () => {
    try {
      const values = await form.validate();
      setSubmitting(true);

      const submitPlatform = values.platform as IWebhook['platform'];
      const larkPlatform = submitPlatform === 'lark';

      if (isEdit) {
        // Determine secret value for update.
        // Non-lark platforms never carry a secret, so omit it (keep existing / none).
        let secretValue: string | null | undefined;
        if (!larkPlatform) {
          secretValue = undefined;
        } else if (clearSecret) {
          secretValue = null;
        } else if (values.secret && values.secret.length > 0) {
          secretValue = values.secret;
        } else {
          secretValue = undefined; // keep existing
        }

        await ipcBridge.webhook.update.invoke({
          webhook_id: editing.webhook_id,
          updates: {
            name: values.name,
            platform: submitPlatform,
            url: values.url,
            description: values.description || '',
            enabled: values.enabled,
            secret: secretValue,
          },
        });
        message.success(t('webhook.messages.updateOk'));
      } else {
        await ipcBridge.webhook.create.invoke({
          name: values.name,
          platform: submitPlatform,
          url: values.url,
          description: values.description || '',
          enabled: values.enabled,
          secret: larkPlatform ? values.secret || undefined : undefined,
        });
        message.success(t('webhook.messages.createOk'));
      }

      onSuccess();
    } catch (e) {
      if (e && typeof e === 'object' && 'errorFields' in e) {
        // form validation error — do not show Message
        return;
      }
      message.error(String(e));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      title={isEdit ? t('webhook.form.editTitle') : t('webhook.form.createTitle')}
      visible={visible}
      onCancel={onClose}
      onOk={() => void handleSubmit()}
      confirmLoading={submitting}
      unmountOnExit
    >
      {ctx}
      <Form form={form} layout='vertical'>
        <Form.Item label={t('webhook.form.name')} field='name' rules={[{ required: true, message: t('webhook.form.nameRequired') }]}>
          <Input placeholder={t('webhook.form.namePlaceholder')} />
        </Form.Item>
        <Form.Item label={t('webhook.form.platform')} field='platform'>
          <Select
            options={[
              { label: t('webhook.platform.lark'), value: 'lark' },
              { label: t('webhook.platform.http'), value: 'http' },
              { label: t('webhook.platform.slack'), value: 'slack' },
            ]}
          />
        </Form.Item>
        <Form.Item label={t('webhook.form.url')} field='url' rules={[{ required: true, message: t('webhook.form.urlRequired') }]}>
          <Input placeholder={t('webhook.form.urlPlaceholder')} />
        </Form.Item>
        {isLark && (
          <Form.Item label={t('webhook.form.secret')} field='secret'>
            <Input.Password
              placeholder={isEdit ? t('webhook.form.secretHint') : t('webhook.form.secretPlaceholder')}
              disabled={clearSecret}
            />
          </Form.Item>
        )}
        {isLark && isEdit && (
          <Form.Item>
            <Checkbox checked={clearSecret} onChange={setClearSecret}>
              {t('webhook.form.clearSecret')}
            </Checkbox>
          </Form.Item>
        )}
        <Form.Item label={t('webhook.form.description')} field='description'>
          <Input.TextArea placeholder={t('webhook.form.descriptionPlaceholder')} />
        </Form.Item>
        <Form.Item label={t('webhook.form.enabled')} field='enabled' triggerPropName='checked'>
          <Switch />
        </Form.Item>
      </Form>
    </Modal>
  );
};

export default ChannelFormModal;
