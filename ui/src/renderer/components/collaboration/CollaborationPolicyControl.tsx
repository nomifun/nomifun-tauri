import { Button, Popover, Switch } from '@arco-design/web-react';
import { EveryUser } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import type { TDecisionPolicy, TDelegationPolicy } from '@/common/types/agentExecution/agentExecutionTypes';
import styles from './CollaborationPolicyControl.module.css';

export type CollaborationPolicyValue = {
  delegationPolicy: TDelegationPolicy;
  decisionPolicy: TDecisionPolicy;
};

type CollaborationPolicyControlProps = CollaborationPolicyValue & {
  runtimeType?: string;
  onChange: (next: CollaborationPolicyValue) => void | Promise<void>;
  compact?: boolean;
  className?: string;
};

const DELEGATION_OPTIONS: TDelegationPolicy[] = ['disabled', 'automatic', 'prefer_parallel'];

const CollaborationPolicyControl: React.FC<CollaborationPolicyControlProps> = ({
  runtimeType,
  delegationPolicy,
  decisionPolicy,
  onChange,
  compact = false,
  className,
}) => {
  const { t } = useTranslation();
  const titleId = React.useId();
  const descriptionId = React.useId();
  if (runtimeType !== 'nomi') return null;

  const title = t('collaboration.policy.title', { defaultValue: '协作策略' });
  const description = t('collaboration.policy.description', {
    defaultValue: '决定当前对话是否拆分任务，以及是否优先并行推进。',
  });
  const askUserLabel = t('collaboration.policy.askUser', {
    defaultValue: '关键决策时询问我',
  });

  const content = (
    <section className={styles.panel} aria-labelledby={titleId} aria-describedby={descriptionId}>
      <div className={styles.header}>
        <span className={styles.headerIcon} aria-hidden='true'>
          <EveryUser theme='outline' size='17' strokeWidth={3} />
        </span>
        <div className={styles.headerCopy}>
          <div id={titleId} className={styles.title}>
            {title}
          </div>
          <div id={descriptionId} className={styles.description}>
            {description}
          </div>
        </div>
      </div>

      <div className={styles.segmentedControl} role='radiogroup' aria-labelledby={titleId}>
        <div className={styles.segmentedTrack}>
          {DELEGATION_OPTIONS.map((option) => {
            const active = option === delegationPolicy;
            return (
              <button
                key={option}
                type='button'
                className={`${styles.segmentedOption} ${active ? styles.segmentedOptionActive : ''}`}
                role='radio'
                aria-checked={active}
                onClick={() =>
                  void onChange({
                    delegationPolicy: option,
                    decisionPolicy,
                  })
                }
              >
                {t(`collaboration.policy.delegation.${option}`, {
                  defaultValue: option === 'disabled' ? '关闭' : option === 'prefer_parallel' ? '优先并行' : '自动',
                })}
              </button>
            );
          })}
        </div>
      </div>

      <div className={styles.decisionCard} data-disabled={delegationPolicy === 'disabled'}>
        <div className={styles.decisionCopy}>
          <div className={styles.decisionTitle}>{askUserLabel}</div>
          <div className={styles.decisionDescription}>
            {t('collaboration.policy.askUserDescription', {
              defaultValue: '协作者遇到无法安全判断的选择时暂停并询问。',
            })}
          </div>
        </div>
        <Switch
          size='small'
          className={styles.decisionSwitch}
          checked={decisionPolicy === 'ask_user'}
          disabled={delegationPolicy === 'disabled'}
          aria-label={askUserLabel}
          onChange={(checked) =>
            void onChange({
              delegationPolicy,
              decisionPolicy: checked ? 'ask_user' : 'automatic',
            })
          }
        />
      </div>
    </section>
  );

  const active = delegationPolicy !== 'disabled';
  return (
    <Popover className={styles.popover} content={content} trigger='click' position='top' unmountOnExit>
      <Button
        type={compact ? 'text' : 'secondary'}
        shape={compact ? 'circle' : 'round'}
        size='small'
        className={[styles.trigger, className].filter(Boolean).join(' ')}
        aria-label={t('collaboration.policy.open', {
          defaultValue: '协作策略',
        })}
        aria-pressed={active}
        data-testid='collaboration-policy-control'
      >
        <span className='inline-flex items-center gap-5px'>
          <EveryUser theme='outline' size='15' fill='currentColor' strokeWidth={3} />
          {!compact && (
            <span>
              {t('collaboration.policy.button', {
                defaultValue: active ? '协作已启用' : '协作已关闭',
              })}
            </span>
          )}
          {compact && active && <span className={styles.triggerStatus} aria-hidden='true' />}
        </span>
      </Button>
    </Popover>
  );
};

export default CollaborationPolicyControl;
