import type { IProvider, ModelTask } from '@/common/config/storage';
import type { ProtocolDetectionResponse, ProtocolType } from '@/common/utils/protocolDetector';
import { ipcBridge } from '@/common';
import { prefixedId } from '@/common/utils';
import { normalizeApiKeyList, validateApiKeysForSave } from '@/common/utils/apiKeys';
import { platformHasNoModelsEndpoint } from '@/common/utils/platformConstants';
import { isGoogleApisHost } from '@/common/utils/urlValidation';
import ModalHOC from '@/renderer/utils/ui/ModalHOC';
import { copyText } from '@/renderer/utils/ui/clipboard';
import { Button, Checkbox, Form, Input, Popover, Select, Switch } from '@arco-design/web-react';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { LinkCloud, Edit, Search, Loading, Info, Copy } from '@icon-park/react';
import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { mutate as mutateSWR } from 'swr';
import useModeModeList from '@renderer/hooks/agent/useModeModeList';
import useProtocolDetection from '@renderer/hooks/system/useProtocolDetection';
import NomiModal from '@/renderer/components/base/NomiModal';
import ApiKeyEditorModal from './ApiKeyEditorModal';
import {
  MODEL_PLATFORMS,
  NEW_API_PROTOCOL_OPTIONS,
  detectNewApiProtocol,
  getPlatformByValue,
  isCustomOption,
  isGeminiPlatform,
  isNewApiPlatform,
  type PlatformConfig,
} from '@/renderer/utils/model/modelPlatforms';
import type { DeepLinkAddProviderDetail } from '@/renderer/hooks/system/useDeepLink';
import { ContextLimitSelect } from './ContextLimitSelect';
import { MODEL_PROFILES_SWR_KEY } from '@renderer/hooks/agent/useModelProfiles';
import { buildModelProfileUpsertRequest, MODEL_TASK_ORDER } from '@renderer/hooks/agent/modelProfileEditing';

/**
 * 预设供应商的 API 地址示例（用于 base_url 旁的 tips 弹层）
 * Preset provider API endpoint examples (for the tips popover beside base_url).
 * Derived from MODEL_PLATFORMS entries that carry a base_url.
 */
const API_URL_TIPS: Array<{ name: string; logo: string | null; base_url: string }> = MODEL_PLATFORMS.filter(
  (p) => !!p.base_url
).map((p) => ({ name: p.name, logo: p.logo, base_url: p.base_url as string }));

/**
 * Protocol icon configurations
 */
const PROTOCOL_ICONS: Record<ProtocolType, { color: string; bgColor: string }> = {
  openai: { color: '#10A37F', bgColor: 'rgba(16, 163, 127, 0.1)' },
  gemini: { color: '#4285F4', bgColor: 'rgba(66, 133, 244, 0.1)' },
  anthropic: { color: '#D97757', bgColor: 'rgba(217, 119, 87, 0.1)' },
  unknown: { color: '#9CA3AF', bgColor: 'rgba(156, 163, 175, 0.1)' },
};

/**
 * Get translated suggestion message
 */
const getSuggestionMessage = (
  suggestion: ProtocolDetectionResponse['suggestion'],
  t: (key: string, params?: Record<string, string>) => string
): string => {
  if (!suggestion) return '';

  // Use i18n key for translation if available
  if (suggestion.i18nKey) {
    const translated = t(suggestion.i18nKey, suggestion.i18nParams);
    // If translation result equals the key, translation failed, fallback to message
    if (translated !== suggestion.i18nKey) {
      return translated;
    }
  }

  // Fallback to original message
  return suggestion.message;
};

/**
 * Protocol Detection Status Component
 * Display protocol detection status, result, and suggestions
 */
interface ProtocolDetectionStatusProps {
  /** Whether detecting */
  isDetecting: boolean;
  /** Detection result */
  result: ProtocolDetectionResponse | null;
  /** Currently selected platform */
  currentPlatform?: string;
  /** Switch platform callback */
  onSwitchPlatform?: (platform: string) => void;
}

const ProtocolDetectionStatus: React.FC<ProtocolDetectionStatusProps> = ({
  isDetecting,
  result,
  currentPlatform,
  onSwitchPlatform,
}) => {
  const { t } = useTranslation();

  // Detecting
  if (isDetecting) {
    return (
      <div className='flex items-center gap-6px text-12px text-t-secondary py-4px'>
        <Loading theme='outline' size={14} className='animate-spin' />
        <span>{t('settings.protocolDetecting')}</span>
      </div>
    );
  }

  // No detection result
  if (!result) {
    return null;
  }

  const { protocol, success, suggestion, multiKeyResult, detectedProtocols } = result;
  const iconConfig = PROTOCOL_ICONS[protocol] || PROTOCOL_ICONS.unknown;
  const protocolName = (p: ProtocolType): string =>
    p === 'openai' ? 'OpenAI' : p === 'gemini' ? 'Gemini' : p === 'anthropic' ? 'Anthropic' : 'Unknown';
  const probed = detectedProtocols ?? [];
  const hasMulti = probed.length > 0;

  // Nothing to show
  if (!success && !result.error && !hasMulti) {
    return null;
  }

  const showSwitchButton =
    suggestion?.type === 'switch_platform' &&
    !!suggestion?.suggestedPlatform &&
    suggestion?.suggestedPlatform !== currentPlatform;

  return (
    <div className='flex flex-col gap-4px py-4px'>
      {/* 检测到的所有协议（聚合网关可能在同一地址同时支持多个，如 gpt + claude）
          All detected protocols — aggregator gateways may serve several on one base_url */}
      {hasMulti && (
        <div className='flex items-center gap-6px flex-wrap text-11px'>
          <span className='text-t-tertiary'>{t('settings.modelProtocol')}:</span>
          {probed.map((dp) => {
            const cfg = PROTOCOL_ICONS[dp.protocol] || PROTOCOL_ICONS.unknown;
            return (
              <span
                key={dp.protocol}
                className='inline-flex items-center px-6px py-1px rounded-4px font-medium'
                style={{ backgroundColor: cfg.bgColor, color: cfg.color }}
              >
                {protocolName(dp.protocol)}
                {dp.models && dp.models.length > 0 ? ` · ${dp.models.length}` : ''}
              </span>
            );
          })}
        </div>
      )}

      {/* Suggestion line (detection succeeded with an actionable suggestion) */}
      {success && suggestion && (
        <div className='flex items-start gap-8px text-12px'>
          <div className='flex items-center gap-6px flex-1 min-w-0'>
            <div
              className='flex items-center justify-center w-16px h-16px rounded-4px shrink-0'
              style={{ backgroundColor: iconConfig.bgColor }}
            >
              <span className='text-10px font-medium' style={{ color: iconConfig.color }}>
                {protocol === 'openai' ? 'O' : protocol === 'gemini' ? 'G' : protocol === 'anthropic' ? 'A' : '?'}
              </span>
            </div>
            <span className='text-t-secondary truncate'>{getSuggestionMessage(suggestion, t)}</span>
          </div>

          {showSwitchButton && onSwitchPlatform && (
            <button
              type='button'
              className='shrink-0 px-8px py-2px rounded-4px text-11px font-medium transition-colors'
              style={{ backgroundColor: iconConfig.bgColor, color: iconConfig.color }}
              onClick={() => onSwitchPlatform(suggestion.suggestedPlatform!)}
            >
              {t('settings.switchPlatform')}
            </button>
          )}
        </div>
      )}

      {/* Multi-key test result */}
      {success && multiKeyResult && multiKeyResult.total > 1 && (
        <div className='flex items-center gap-6px text-11px text-t-tertiary pl-22px'>
          <span>
            {multiKeyResult.invalid === 0
              ? t('settings.multiKeyAllValid', { total: String(multiKeyResult.total) })
              : multiKeyResult.valid === 0
                ? t('settings.multiKeyAllInvalid', { total: String(multiKeyResult.total) })
                : t('settings.multiKeyPartialValid', {
                    valid: String(multiKeyResult.valid),
                    invalid: String(multiKeyResult.invalid),
                  })}
          </span>
        </div>
      )}

      {/* Detection failed (show the error and/or the actionable suggestion) */}
      {!success && (result.error || suggestion) && (
        <div className='flex items-center gap-6px text-12px text-warning'>
          <div className='flex items-center justify-center w-16px h-16px rounded-4px bg-warning/10 shrink-0'>
            <span className='text-10px font-medium'>!</span>
          </div>
          <span className='truncate'>{suggestion ? getSuggestionMessage(suggestion, t) : result.error}</span>
        </div>
      )}
    </div>
  );
};

/**
 * 供应商 Logo 组件
 * Provider Logo Component
 */
const ProviderLogo: React.FC<{ logo: string | null; name: string; size?: number }> = ({ logo, name, size = 20 }) => {
  if (logo) {
    return <img src={logo} alt={name} className='object-contain shrink-0' style={{ width: size, height: size }} />;
  }
  return <LinkCloud theme='outline' size={size} className='text-t-secondary flex shrink-0' />;
};

/**
 * 平台下拉选项渲染（第一层）
 * Platform dropdown option renderer (first level)
 *
 * @param platform - 平台配置 / Platform config
 * @param t - 翻译函数 / Translation function
 */
const renderPlatformOption = (platform: PlatformConfig, t?: (key: string) => string) => {
  // 如果有 i18nKey 且提供了翻译函数，使用翻译后的名称；否则使用原始名称
  // If i18nKey exists and t function is provided, use translated name; otherwise use original name
  const display_name = platform.i18nKey && t ? t(platform.i18nKey) : platform.name;
  return (
    <div className='flex items-center gap-8px'>
      <ProviderLogo logo={platform.logo} name={display_name} size={18} />
      <span>{display_name}</span>
    </div>
  );
};

const AddPlatformModal = ModalHOC<{
  onSubmit: (platform: IProvider) => void | Promise<void>;
  deepLinkData?: DeepLinkAddProviderDetail;
}>(({ modalProps, onSubmit, modalCtrl, deepLinkData }) => {
  const [message, messageContext] = useArcoMessage();
  const { t } = useTranslation();
  const [form] = Form.useForm();
  const [api_keyEditorVisible, setApiKeyEditorVisible] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  // 用于追踪上次检测时的输入值，避免重复检测
  // Track last detection input to avoid redundant detection
  const [lastDetectionInput, setLastDetectionInput] = useState<{ base_url: string; api_key: string } | null>(null);

  const platformValue = Form.useWatch('platform', form);
  const base_url = Form.useWatch('base_url', form);
  const api_key = Form.useWatch('api_key', form);
  const modelValue = Form.useWatch('model', form);
  const bedrockAuthMethod = Form.useWatch('bedrockAuthMethod', form);
  const _bedrockRegion = Form.useWatch('bedrockRegion', form);

  // 获取当前选中的平台配置 / Get current selected platform config
  const selectedPlatform = useMemo(() => getPlatformByValue(platformValue), [platformValue]);

  const platform = selectedPlatform?.platform ?? 'gemini';
  // 判断是否为"自定义"选项（没有预设 base_url） / Check if "Custom" option (no preset base_url)
  const isCustom = isCustomOption(platformValue);
  const isBedrock = platform === 'bedrock';
  const isGemini = isGeminiPlatform(platform);
  const isNewApi = isNewApiPlatform(platform);

  // new-api 每模型协议选择状态 / new-api per-model protocol selection state
  const [modelProtocol, setModelProtocol] = useState<string>('openai');
  const [tasks, setTasks] = useState<ModelTask[]>([]);
  const [visionInput, setVisionInput] = useState(false);
  const [isFullUrl, setIsFullUrl] = useState(false);
  // 用户已忽略的 base_url 自动修复建议 / base_url auto-fix suggestion the user dismissed
  const [dismissedFixUrl, setDismissedFixUrl] = useState<string | null>(null);
  // API 地址示例 tips 弹层显隐 / API endpoint examples tips popover visibility
  const [urlTipsVisible, setUrlTipsVisible] = useState(false);

  // 计算某个平台选项的默认供应商名称：自定义裸选项与 New API 聚合网关留空，让用户自行命名
  // （同一聚合平台常有多个供应商部署，需要可区分的名称）；其余预设平台沿用其展示名。
  // Resolve the default provider name for a platform option. The bare Custom option and the
  // New API aggregator gateway stay blank so the user names their own provider (one aggregator
  // platform often has many vendor deployments). Other preset platforms reuse their display name.
  const resolvePlatformName = (value: string): string => {
    const plat = MODEL_PLATFORMS.find((p) => p.value === value);
    if (!plat || isCustomOption(value) || isNewApiPlatform(plat.platform)) return '';
    return plat.i18nKey ? t(plat.i18nKey) : plat.name;
  };

  // Auto-detect protocol when model changes (for new-api platforms)
  useEffect(() => {
    if (isNewApi && modelValue) {
      setModelProtocol(detectNewApiProtocol(modelValue));
    }
  }, [modelValue, isNewApi]);

  const taskOptions = useMemo(
    () => MODEL_TASK_ORDER.map((v) => ({ label: t(`settings.modelTask.${v}`), value: v })),
    [t]
  );

  // 计算实际使用的 base_url（优先使用用户输入，否则使用平台预设）
  // Calculate actual base_url (prefer user input, fallback to platform preset)
  const actualBaseUrl = useMemo(() => {
    if (base_url) return base_url;
    return selectedPlatform?.base_url || '';
  }, [base_url, selectedPlatform?.base_url]);

  // For Bedrock, don't pass bedrock_config to avoid auto-refresh on input changes
  // We'll build it dynamically in onFocus
  const modelListState = useModeModeList(platform, actualBaseUrl, api_key, true, undefined);

  // 协议检测 Hook / Protocol detection hook
  // 启用检测的条件：
  // 1. 自定义平台 或 用户输入了自定义 base URL（非官方地址，如本地代理）
  // 2. 输入值与上次"采纳建议"时不同（避免切换平台后重复检测）
  // Enable detection when:
  // 1. Custom platform OR user entered a custom base URL (non-official, like local proxy)
  // 2. Input values differ from last "accepted suggestion" (avoid redundant detection after platform switch)
  const isNonOfficialBaseUrl = base_url && !isGoogleApisHost(base_url);
  const shouldEnableDetection = isCustom || isNonOfficialBaseUrl;
  // 只有在用户修改了输入值（相对于上次采纳建议时）才触发检测
  // Only trigger detection when input changed since last accepted suggestion
  const inputChangedSinceLastSwitch =
    !lastDetectionInput || lastDetectionInput.base_url !== actualBaseUrl || lastDetectionInput.api_key !== api_key;
  const protocolDetection = useProtocolDetection(
    shouldEnableDetection && inputChangedSinceLastSwitch ? actualBaseUrl : '',
    shouldEnableDetection && inputChangedSinceLastSwitch ? api_key : '',
    {
      debounceMs: 1000,
      autoDetect: true,
      timeout: 10000,
    }
  );

  // 是否显示检测结果：启用检测 且 (有结果或正在检测) 且 输入值与上次采纳时不同
  // Whether to show detection result: enabled AND (has result or detecting) AND input changed since last switch
  const shouldShowDetectionResult = shouldEnableDetection && inputChangedSinceLastSwitch;

  // 处理平台切换建议
  // Handle platform switch suggestion
  const handleSwitchPlatform = (suggestedPlatform: string) => {
    const targetPlatform = MODEL_PLATFORMS.find((p) => p.value === suggestedPlatform || p.name === suggestedPlatform);
    if (targetPlatform) {
      form.setFieldValue('platform', targetPlatform.value);
      form.setFieldValue('model', '');
      protocolDetection.reset();
      // 记录当前输入，防止切换后重复检测
      // Record current input to prevent redundant detection after switch
      setLastDetectionInput({ base_url: actualBaseUrl, api_key });
      message.success(t('settings.platformSwitched', { platform: targetPlatform.name }));
    }
  };

  // 弹窗打开时重置表单 / Reset form when modal opens
  useEffect(() => {
    if (modalProps.visible) {
      form.resetFields();
      form.setFieldValue('bedrockAuthMethod', 'accessKey');
      form.setFieldValue('bedrockRegion', 'us-east-1');
      protocolDetection.reset();
      setLastDetectionInput(null); // 重置检测记录 / Reset detection record
      setModelProtocol('openai'); // 重置协议选择 / Reset protocol selection
      setTasks([]);
      setVisionInput(false);
      setIsFullUrl(false);
      setDismissedFixUrl(null); // 重置 base_url 修复建议 / Reset base_url fix suggestion

      // Pre-fill from deep link data (nomifun:// protocol)
      if (deepLinkData?.base_url || deepLinkData?.api_key) {
        // Default to new-api platform for deep links (typical one-api/new-api usage)
        const dlPlatform = deepLinkData.platform || 'new-api';
        form.setFieldValue('platform', dlPlatform);
        // 深链若自带供应商名则优先使用，否则按平台规则预填（new-api/自定义留空待用户填写）
        // Prefer the deep link's provider name when present; otherwise prefill per platform rules
        // (new-api / custom stay blank for the user to fill).
        form.setFieldValue('name', deepLinkData.name ?? resolvePlatformName(dlPlatform));
        if (deepLinkData.base_url) form.setFieldValue('base_url', deepLinkData.base_url);
        if (deepLinkData.api_key) form.setFieldValue('api_key', deepLinkData.api_key);
      } else {
        form.setFieldValue('platform', 'gemini');
        form.setFieldValue('name', resolvePlatformName('gemini'));
      }
    }
  }, [modalProps.visible, deepLinkData]);

  useEffect(() => {
    if (platform?.includes('gemini')) {
      void modelListState.mutate();
    }
  }, [platform]);

  // base_url 自动修复：仅作为「建议」展示，由用户决定是否采用，绝不强制覆盖用户输入。
  // base_url auto-fix is surfaced as a *suggestion* only — the user decides whether
  // to apply it. We never silently overwrite what the user typed (so they remain free
  // to point at gpt / anthropic / their own gateway endpoints).
  const fixBaseUrl = modelListState.data?.fix_base_url;
  const showBaseUrlSuggestion = !!fixBaseUrl && fixBaseUrl !== base_url && fixBaseUrl !== dismissedFixUrl;

  const testApiKeyForProvider = async (key: string, baseUrl: string) => {
    try {
      const res = await ipcBridge.mode.detectProtocol.invoke({
        base_url: baseUrl,
        api_key: key,
        timeout: 10000,
      });
      return res?.success === true;
    } catch {
      return false;
    }
  };

  const handleSubmit = () => {
    form
      .validate()
      .then(async (values) => {
        // 优先使用用户填写的供应商名称，留空时回退到平台预设名称
        // Prefer the user-entered provider name; fall back to the platform preset when blank
        const presetName = selectedPlatform?.i18nKey
          ? t(selectedPlatform.i18nKey)
          : (selectedPlatform?.name ?? values.platform);
        const name = String(values.name ?? '').trim() || presetName;
        const contextLimit =
          typeof values.context_limit === 'number' && values.context_limit > 0 ? values.context_limit : undefined;
        const providerPlatform = selectedPlatform?.platform ?? 'custom';
        const providerBaseUrl = isBedrock ? '' : values.base_url || selectedPlatform?.base_url || '';
        let normalizedApiKey = '';

        // 订阅套餐网关（Coding/Agent Plan）没有 /models 目录，用 /models 探测会把
        // 合法 Key 误判为不可用 —— 跳过保存前探测，交由心跳检测校验。
        if (!isBedrock && !isFullUrl && !platformHasNoModelsEndpoint(providerPlatform)) {
          setIsSaving(true);
          const validation = await validateApiKeysForSave(values.api_key, (key) =>
            testApiKeyForProvider(key, providerBaseUrl)
          );
          form.setFieldValue('api_key', validation.normalized);
          if (!validation.valid) {
            message.error(
              t('settings.removeInvalidApiKeysBeforeSave', { count: validation.invalidIndexes.length })
            );
            return;
          }
          normalizedApiKey = validation.normalized;
        } else {
          normalizedApiKey = isBedrock ? '' : normalizeApiKeyList(values.api_key);
        }

        const provider: IProvider = {
          id: prefixedId('prov'),
          platform: providerPlatform,
          name,
          // 优先使用用户输入的 base_url，否则使用平台预设值
          // Prefer user input base_url, fallback to platform preset
          base_url: providerBaseUrl,
          api_key: normalizedApiKey,
          models: [values.model],
          is_full_url: isFullUrl,
          model_context_limits: contextLimit ? { [values.model]: contextLimit } : undefined,
        };

        // Add Bedrock configuration if platform is Bedrock
        if (isBedrock) {
          provider.bedrock_config = {
            auth_method: values.bedrockAuthMethod,
            region: values.bedrockRegion,
            ...(values.bedrockAuthMethod === 'accessKey'
              ? {
                  access_key_id: values.bedrockAccessKeyId,
                  secret_access_key: values.bedrockSecretAccessKey,
                }
              : {
                  profile: values.bedrockProfile,
                }),
          };
        }

        // new-api 平台：保存每模型协议配置 / new-api platform: save per-model protocol config
        if (isNewApi && values.model) {
          provider.model_protocols = { [values.model]: modelProtocol };
        }

        setIsSaving(true);
        await onSubmit(provider);
        if (values.model) {
          const selectedTraits = tasks.includes('chat') && visionInput ? (['vision_input'] as const) : [];
          try {
            await ipcBridge.modelProfile.upsert.invoke({
              ...buildModelProfileUpsertRequest(provider.id, values.model, tasks, [...selectedTraits]),
            });
            void mutateSWR(MODEL_PROFILES_SWR_KEY);
          } catch (error) {
            console.error('model profile upsert failed', error);
            message.warning(t('settings.saveModelConfigFailed', { defaultValue: '模型能力保存失败' }));
            return;
          }
        }
        modalCtrl.close();
      })
      .catch(() => {
        // validation failed
      })
      .finally(() => {
        setIsSaving(false);
      });
  };

  return (
    <NomiModal
      visible={modalProps.visible}
      onCancel={modalCtrl.close}
      header={{ title: t('settings.addModel'), showClose: true }}
      style={{ maxWidth: '92vw', borderRadius: 16 }}
      contentStyle={{
        background: 'var(--dialog-fill-0)',
        borderRadius: 16,
        padding: '20px 24px 16px',
        overflow: 'auto',
      }}
      onOk={handleSubmit}
      confirmLoading={modalProps.confirmLoading || isSaving}
      okText={t('common.confirm')}
      cancelText={t('common.cancel')}
    >
      {messageContext}
      <div className='pt-4px pb-12px'>
        <Form form={form} layout='vertical' className='[&_.arco-form-item]:mb-12px [&_.arco-form-item:last-child]:mb-0'>
          {/* 模型平台选择（第一层）/ Model Platform Selection (first level) */}
          <Form.Item
            initialValue='gemini'
            label={t('settings.modelPlatform')}
            field={'platform'}
            required
            rules={[{ required: true }]}
          >
            <Select
              showSearch
              filterOption={(inputValue, option) => {
                const optionValue = (option as React.ReactElement<{ value?: string }>)?.props?.value;
                const plat = MODEL_PLATFORMS.find((p) => p.value === optionValue);
                return plat?.name.toLowerCase().includes(inputValue.toLowerCase()) ?? false;
              }}
              onChange={(value) => {
                const plat = MODEL_PLATFORMS.find((p) => p.value === value);
                if (plat) {
                  form.setFieldValue('model', '');
                  setTasks([]);
                  setVisionInput(false);
                  // 预填模型供应商名称：预设平台用其展示名，自定义裸选项留空待用户填写。
                  // 这样用户在新增时即可命名供应商，便于在 Nomi 中归类（尤其聚合平台）。
                  // Prefill provider name: preset platforms use their display name,
                  // the bare Custom option stays blank for the user to fill. Lets
                  // users name the provider at add-time for clean grouping in the
                  // Nomi (especially for same-looking aggregator platforms).
                  form.setFieldValue('name', resolvePlatformName(value));
                  setDismissedFixUrl(null);
                }
              }}
              renderFormat={(option) => {
                const optionValue = (option as { value?: string })?.value;
                const plat = MODEL_PLATFORMS.find((p) => p.value === optionValue);
                if (!plat) return optionValue;
                return renderPlatformOption(plat, t);
              }}
            >
              {MODEL_PLATFORMS.map((plat) => (
                <Select.Option key={plat.value} value={plat.value}>
                  {renderPlatformOption(plat, t)}
                </Select.Option>
              ))}
            </Select>
          </Form.Item>

          {/* 模型供应商名称：“自定义”裸选项与 New API 聚合网关需用户填写（同一网关常有多个供应商部署，
              需要可区分的名称）；其余预设平台已由上方下拉框标明供应商，自动沿用其名称。
              Model provider name: the bare "Custom" option and the New API aggregator gateway need a
              user-entered name (one gateway often has many vendor deployments, so a distinct name helps).
              Other preset platforms are already identified by the dropdown above, so the field is hidden
              and the preset name is used. */}
          <Form.Item
            hidden={!isCustom && !isNewApi}
            label={
              <div className='flex items-center gap-6px'>
                <ProviderLogo logo={selectedPlatform?.logo ?? null} name={selectedPlatform?.name ?? ''} size={16} />
                <span>{t('settings.modelProvider')}</span>
              </div>
            }
            field={'name'}
            required={isCustom || isNewApi}
            rules={[{ required: isCustom || isNewApi }]}
            extra={<span className='text-11px text-t-tertiary'>{t('settings.modelProviderHint')}</span>}
          >
            <Input placeholder={t('settings.modelProvider')} />
          </Form.Item>

          {/* Base URL - 自定义选项、标准 Gemini 和 New API 显示 / Base URL - for Custom, standard Gemini and New API */}
          <Form.Item
            hidden={isBedrock || (!isCustom && !isNewApi && platformValue !== 'gemini')}
            label={
              <div className='inline-flex items-center gap-6px'>
                <span>{t('settings.apiEndpoint', 'API 请求地址')}</span>
                <Popover
                  trigger='click'
                  position='bl'
                  popupVisible={urlTipsVisible}
                  onVisibleChange={setUrlTipsVisible}
                  title={t('settings.apiUrlTipsTitle')}
                  content={
                    <div className='w-300px max-h-320px overflow-y-auto'>
                      <div className='text-11px text-t-tertiary mb-6px'>{t('settings.apiUrlTipsHint')}</div>
                      {API_URL_TIPS.map((tip) => (
                        <div
                          key={tip.base_url}
                          className='flex items-center gap-8px px-6px py-5px rd-6px cursor-pointer hover:bg-[var(--fill-1)]'
                          onClick={() => {
                            form.setFieldValue('base_url', tip.base_url);
                            setDismissedFixUrl(null);
                            setUrlTipsVisible(false);
                            void modelListState.mutate();
                          }}
                        >
                          <ProviderLogo logo={tip.logo} name={tip.name} size={16} />
                          <div className='flex-1 min-w-0'>
                            <div className='text-12px text-t-primary leading-4'>{tip.name}</div>
                            <div className='text-11px text-t-tertiary truncate'>{tip.base_url}</div>
                          </div>
                          <Copy
                            theme='outline'
                            size={14}
                            className='text-t-secondary hover:text-t-primary shrink-0'
                            onClick={(e) => {
                              e.stopPropagation();
                              void copyText(tip.base_url);
                              message.success(t('common.copySuccess'));
                            }}
                          />
                        </div>
                      ))}
                    </div>
                  }
                >
                  <span className='inline-flex cursor-pointer'>
                    <Info
                      theme='outline'
                      size={14}
                      className='text-t-secondary hover:text-[rgb(var(--primary-6))] flex'
                    />
                  </span>
                </Popover>
              </div>
            }
            field={'base_url'}
            required={isCustom || isNewApi}
            rules={[{ required: isCustom || isNewApi }]}
            extra={
              showBaseUrlSuggestion ? (
                <div className='flex items-center gap-8px text-12px text-t-secondary py-4px flex-wrap'>
                  <span className='min-w-0 break-all'>
                    {t('settings.baseUrlSuggestion', { base_url: fixBaseUrl! })}
                  </span>
                  <Button
                    size='mini'
                    type='text'
                    className='!px-6px !h-auto shrink-0'
                    onClick={() => {
                      form.setFieldValue('base_url', fixBaseUrl!);
                      setDismissedFixUrl(null);
                      void modelListState.mutate();
                    }}
                  >
                    {t('settings.baseUrlSuggestionApply')}
                  </Button>
                  <Button
                    size='mini'
                    type='text'
                    status='default'
                    className='!px-6px !h-auto shrink-0 !text-t-tertiary'
                    onClick={() => setDismissedFixUrl(fixBaseUrl!)}
                  >
                    {t('settings.baseUrlSuggestionKeep')}
                  </Button>
                </div>
              ) : undefined
            }
          >
            <Input
              placeholder={
                isFullUrl
                  ? 'https://your-api-endpoint.com/v1/chat/completions'
                  : isNewApi
                    ? 'https://your-newapi-instance.com'
                    : selectedPlatform?.base_url || ''
              }
              onBlur={() => {
                void modelListState.mutate();
              }}
            />
          </Form.Item>

          {/*
            Full URL toggle - only for custom and new-api platforms.
            Use a positive marginTop so the Switch row sits below the Input.
            A negative marginTop would overlap the Input's bottom edge and
            intercept clicks on its lower rim (see ELECTRON-1K4).
          */}
          {(isCustom || isNewApi) && !isBedrock && (
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginTop: 4, marginBottom: 12 }}>
              <Switch size='small' checked={isFullUrl} onChange={setIsFullUrl} />
              <span className='text-12px text-t-secondary'>{t('settings.fullUrlMode', '完整 URL')}</span>
              <span className='text-11px text-t-tertiary'>
                {isFullUrl
                  ? t('settings.fullUrlHint', '直接使用此地址，不拼接路径')
                  : t('settings.baseUrlHint', '系统会自动拼接请求路径')}
              </span>
            </div>
          )}

          {/* API Key */}
          <Form.Item
            hidden={isBedrock}
            label={t('settings.apiKey')}
            required={!isBedrock}
            rules={[{ required: !isBedrock }]}
            field={'api_key'}
            extra={
              <div className='space-y-2px'>
                <div className='text-11px text-t-secondary mt-2 leading-4'>{t('settings.multiApiKeyTip')}</div>
                {/* 协议检测状态 / Protocol detection status */}
                {shouldShowDetectionResult && (
                  <ProtocolDetectionStatus
                    isDetecting={protocolDetection.isDetecting}
                    result={protocolDetection.result}
                    currentPlatform={platformValue}
                    onSwitchPlatform={handleSwitchPlatform}
                  />
                )}
              </div>
            }
          >
            <Input
              onBlur={() => {
                void modelListState.mutate();
              }}
              suffix={
                <Edit
                  theme='outline'
                  size={16}
                  className='cursor-pointer text-t-secondary hover:text-t-primary flex'
                  onClick={() => setApiKeyEditorVisible(true)}
                />
              }
            />
          </Form.Item>

          {/* AWS Bedrock Authentication Method */}
          <Form.Item
            hidden={!isBedrock}
            label={t('settings.bedrock.authMethod')}
            field={'bedrockAuthMethod'}
            initialValue='accessKey'
            required={isBedrock}
            rules={[{ required: isBedrock }]}
          >
            <Select>
              <Select.Option value='accessKey'>{t('settings.bedrock.authMethodAccessKey')}</Select.Option>
              <Select.Option value='profile'>{t('settings.bedrock.authMethodProfile')}</Select.Option>
            </Select>
          </Form.Item>

          {/* AWS Region */}
          <Form.Item
            hidden={!isBedrock}
            label={t('settings.bedrock.region')}
            field={'bedrockRegion'}
            initialValue='us-east-1'
            required={isBedrock}
            rules={[{ required: isBedrock }]}
            extra={t('settings.bedrock.regionHint')}
          >
            <Select showSearch>
              <Select.Option value='us-east-1'>US East (N. Virginia)</Select.Option>
              <Select.Option value='us-west-2'>US West (Oregon)</Select.Option>
              <Select.Option value='eu-west-1'>Europe (Ireland)</Select.Option>
              <Select.Option value='eu-central-1'>Europe (Frankfurt)</Select.Option>
              <Select.Option value='ap-southeast-1'>Asia Pacific (Singapore)</Select.Option>
              <Select.Option value='ap-northeast-1'>Asia Pacific (Tokyo)</Select.Option>
              <Select.Option value='ap-southeast-2'>Asia Pacific (Sydney)</Select.Option>
              <Select.Option value='ca-central-1'>Canada (Central)</Select.Option>
            </Select>
          </Form.Item>

          {/* Access Key ID */}
          <Form.Item
            hidden={!isBedrock || bedrockAuthMethod !== 'accessKey'}
            label={t('settings.bedrock.accessKeyId')}
            field={'bedrockAccessKeyId'}
            required={isBedrock && bedrockAuthMethod === 'accessKey'}
            rules={[{ required: isBedrock && bedrockAuthMethod === 'accessKey' }]}
          >
            <Input.Password placeholder='AKIA...' visibilityToggle />
          </Form.Item>

          {/* Secret Access Key */}
          <Form.Item
            hidden={!isBedrock || bedrockAuthMethod !== 'accessKey'}
            label={t('settings.bedrock.secretAccessKey')}
            field={'bedrockSecretAccessKey'}
            required={isBedrock && bedrockAuthMethod === 'accessKey'}
            rules={[{ required: isBedrock && bedrockAuthMethod === 'accessKey' }]}
          >
            <Input.Password visibilityToggle />
          </Form.Item>

          {/* AWS Profile */}
          <Form.Item
            hidden={!isBedrock || bedrockAuthMethod !== 'profile'}
            label={t('settings.bedrock.profile')}
            field={'bedrockProfile'}
            required={isBedrock && bedrockAuthMethod === 'profile'}
            rules={[{ required: isBedrock && bedrockAuthMethod === 'profile' }]}
            extra={t('settings.bedrock.profileHint')}
          >
            <Input placeholder='default' />
          </Form.Item>

          {/* 模型选择 / Model Selection */}
          <Form.Item
            label={t('settings.modelName')}
            field={'model'}
            required
            rules={[{ required: true }]}
            validateStatus={!isFullUrl && modelListState.error ? 'error' : 'success'}
            help={
              !isFullUrl && modelListState.error instanceof Error
                ? modelListState.error.message
                : !isFullUrl && modelListState.error
                  ? String(modelListState.error)
                  : undefined
            }
          >
            <Select
              loading={!isFullUrl && modelListState.isLoading}
              showSearch
              allowCreate
              onChange={() => {
                setTasks([]);
                setVisionInput(false);
              }}
              suffixIcon={
                isFullUrl ? undefined : (
                  <Search
                    onClick={async (e) => {
                      e.stopPropagation();
                      if ((isCustom || isNewApi) && !base_url) {
                        message.warning(t('settings.pleaseEnterBaseUrl'));
                        return;
                      }
                      // For Bedrock, build bedrock_config from current form values and fetch models
                      if (isBedrock) {
                        const values = form.getFields();
                        if (!values.bedrockAuthMethod || !values.bedrockRegion) {
                          message.warning(t('settings.bedrock.fillRequiredFields'));
                          return;
                        }
                        if (
                          values.bedrockAuthMethod === 'accessKey' &&
                          (!values.bedrockAccessKeyId || !values.bedrockSecretAccessKey)
                        ) {
                          message.warning(t('settings.bedrock.fillRequiredFields'));
                          return;
                        }
                        if (values.bedrockAuthMethod === 'profile' && !values.bedrockProfile) {
                          message.warning(t('settings.bedrock.fillRequiredFields'));
                          return;
                        }
                        // Build bedrock_config and fetch models manually
                        const bedrock_config = {
                          auth_method: values.bedrockAuthMethod,
                          region: values.bedrockRegion,
                          ...(values.bedrockAuthMethod === 'accessKey'
                            ? {
                                access_key_id: values.bedrockAccessKeyId,
                                secret_access_key: values.bedrockSecretAccessKey,
                              }
                            : {
                                profile: values.bedrockProfile,
                              }),
                        };
                        try {
                          const res = await ipcBridge.mode.fetchModelList.invoke({
                            platform,
                            api_key: '',
                            bedrock_config,
                          });
                          const models =
                            res.models.map((v) => {
                              if (typeof v === 'string') {
                                return { label: v, value: v };
                              } else {
                                return { label: v.name, value: v.id };
                              }
                            }) || [];
                          // Update the model list state manually
                          void modelListState.mutate({ models }, false);
                        } catch (error: any) {
                          message.error(error.message || 'Failed to fetch models');
                        }
                        return;
                      }
                      // For Gemini, no api_key check needed
                      if (!isGemini && !api_key) {
                        message.warning(t('settings.pleaseEnterApiKey'));
                        return;
                      }
                      void modelListState.mutate();
                    }}
                    theme='outline'
                    size={16}
                    className='cursor-pointer text-t-secondary hover:text-t-primary'
                  />
                )
              }
              options={isFullUrl ? [] : modelListState.data?.models || []}
            />
          </Form.Item>

          {/* 模态能力 / Model modality */}
          <Form.Item
            label={t('settings.modelModality')}
            field={'model_modality'}
            extra={<span className='text-11px text-t-secondary'>{t('settings.modelModalityTip')}</span>}
          >
            <Select
              mode='multiple'
              value={tasks}
              onChange={(value: ModelTask[]) => {
                const next = value ?? [];
                setTasks(next);
                if (!next.includes('chat')) setVisionInput(false);
              }}
              options={taskOptions}
              placeholder={t('settings.modelModality')}
              triggerProps={{ getPopupContainer: () => document.body }}
            />
          </Form.Item>

          {tasks.includes('chat') && (
            <div className='-mt-6px mb-12px'>
              <Checkbox checked={visionInput} onChange={setVisionInput} className='!pl-0'>
                <span className='text-12px text-t-secondary'>{t('settings.modelVisionInput')}</span>
              </Checkbox>
            </div>
          )}

          {/* 上下文窗口 / Context window */}
          <Form.Item
            field='context_limit'
            label={t('settings.contextLimit', { defaultValue: '上下文窗口 (tokens)' })}
          >
            <ContextLimitSelect />
          </Form.Item>

          {/* New API 协议选择 / New API Protocol Selection */}
          {isNewApi && (
            <Form.Item
              label={t('settings.modelProtocol')}
              extra={<span className='text-11px text-t-secondary'>{t('settings.modelProtocolTip')}</span>}
            >
              <Select value={modelProtocol} onChange={setModelProtocol} options={NEW_API_PROTOCOL_OPTIONS} />
            </Form.Item>
          )}
        </Form>
      </div>

      {/* API Key 编辑器弹窗 / API Key Editor Modal */}
      <ApiKeyEditorModal
        visible={api_keyEditorVisible}
        api_keys={api_key || ''}
        onClose={() => setApiKeyEditorVisible(false)}
        onSave={(keys) => {
          form.setFieldValue('api_key', keys);
          void modelListState.mutate();
        }}
        onTestKey={async (key) => {
          // 套餐网关无 /models 目录，探测会误判为无效；交给心跳检测（真实对话）校验。
          if (platformHasNoModelsEndpoint(selectedPlatform?.platform)) return true;
          return testApiKeyForProvider(key, actualBaseUrl);
        }}
      />
    </NomiModal>
  );
});

export default AddPlatformModal;
