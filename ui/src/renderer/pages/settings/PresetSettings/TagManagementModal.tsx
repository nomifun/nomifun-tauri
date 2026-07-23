/**
 * TagManagementModal — Double-column tag vocabulary CRUD (Audience / Skill
 * Scenario). Built-in seed tags render locked (greyed, no actions); user tags
 * support inline rename + delete (delete confirms and warns it is stripped
 * from all presets). Each column has a「+ New tag」input that calls onCreate.
 *
 * Theme variables only; `<div onClick>` for clickables (no <button>).
 */
import type {
  PresetTag,
  PresetTagDimension,
  CreatePresetTagRequest,
} from '@/common/types/agent/presetTypes';
import type { PresetTagId } from '@/common/types/ids';
import type { ArcoMessageInstance } from '@/renderer/utils/ui/useArcoMessage';
import { Input, Modal } from '@arco-design/web-react';
import { Check, Close, Delete, Lock, Plus } from '@icon-park/react';
import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';

type TagManagementModalProps = {
  visible: boolean;
  onClose: () => void;
  audienceTags: PresetTag[];
  scenarioTags: PresetTag[];
  localeKey: string;
  onCreate: (req: CreatePresetTagRequest) => Promise<unknown>;
  onRename: (presetTagId: PresetTagId, label: string) => Promise<void>;
  onDelete: (presetTagId: PresetTagId) => Promise<void>;
  message: ArcoMessageInstance;
};

const errorText = (error: unknown): string => {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  return '';
};

/** A single tag row — locked (built-in) or editable (user). */
const TagRow: React.FC<{
  tag: PresetTag;
  localeKey: string;
  busy: boolean;
  onRename: (presetTagId: PresetTagId, label: string) => void;
  onDelete: (tag: PresetTag) => void;
}> = ({ tag, localeKey, busy, onRename, onDelete }) => {
  const { t } = useTranslation();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const label = tag.label_i18n?.[localeKey] || tag.label;

  if (tag.builtin) {
    return (
      <div
        className='flex items-center gap-8px rounded-10px px-10px py-7px bg-[var(--color-fill-1)] opacity-65'
        data-testid={`tag-row-${tag.key}`}
      >
        <Lock theme='outline' size={13} className='flex-shrink-0 text-[var(--color-text-3)]' />
        <span className='flex-1 min-w-0 truncate text-13px text-[var(--color-text-2)]'>{label}</span>
        <span className='flex-shrink-0 text-10px text-[var(--color-text-3)]'>
          {t('settings.presetTagBuiltinLocked', { defaultValue: 'Built-in tag' })}
        </span>
      </div>
    );
  }

  const commit = () => {
    const next = draft.trim();
    if (next && next !== label) {
      onRename(tag.preset_tag_id, next);
    }
    setEditing(false);
  };

  return (
    <div
      className='group flex items-center gap-8px rounded-10px px-10px py-6px bg-[var(--color-bg-2)] border border-solid border-[var(--color-border-2)] hover:border-[var(--color-border-3)] transition-colors'
      data-testid={`tag-row-${tag.key}`}
    >
      {editing ? (
        <>
          <Input
            size='small'
            autoFocus
            value={draft}
            onChange={setDraft}
            onPressEnter={commit}
            disabled={busy}
            className='flex-1 !rounded-6px'
          />
          <div
            role='button'
            tabIndex={0}
            onClick={commit}
            onKeyDown={(e) => {
              if (e.key === 'Enter') commit();
            }}
            className='flex-shrink-0 flex items-center justify-center w-22px h-22px rounded-6px cursor-pointer text-[rgb(var(--primary-6))] hover:bg-[var(--color-primary-light-1)] transition-colors'
          >
            <Check theme='outline' size={14} strokeWidth={3} />
          </div>
          <div
            role='button'
            tabIndex={0}
            onClick={() => setEditing(false)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') setEditing(false);
            }}
            className='flex-shrink-0 flex items-center justify-center w-22px h-22px rounded-6px cursor-pointer text-[var(--color-text-3)] hover:bg-[var(--color-fill-2)] transition-colors'
          >
            <Close theme='outline' size={14} strokeWidth={3} />
          </div>
        </>
      ) : (
        <>
          <span
            role='button'
            tabIndex={0}
            onClick={() => {
              setDraft(label);
              setEditing(true);
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                setDraft(label);
                setEditing(true);
              }
            }}
            className='flex-1 min-w-0 truncate text-13px text-[var(--color-text-1)] cursor-text'
            title={t('settings.presetTagRenameHint', { defaultValue: 'Click to rename' })}
          >
            {label}
          </span>
          <div
            role='button'
            tabIndex={0}
            data-testid={`tag-delete-${tag.key}`}
            onClick={() => onDelete(tag)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') onDelete(tag);
            }}
            className='flex-shrink-0 flex items-center justify-center w-22px h-22px rounded-6px cursor-pointer text-[var(--color-text-3)] opacity-0 group-hover:opacity-100 hover:text-[rgb(var(--danger-6))] hover:bg-[rgba(var(--danger-6),0.08)] transition-all'
          >
            <Delete theme='outline' size={14} strokeWidth={3} />
          </div>
        </>
      )}
    </div>
  );
};

/** A dimension column: header + tag rows + create input. */
const TagColumn: React.FC<{
  title: string;
  dimension: PresetTagDimension;
  tags: PresetTag[];
  localeKey: string;
  busy: boolean;
  onCreate: (label: string) => void;
  onRename: (presetTagId: PresetTagId, label: string) => void;
  onDelete: (tag: PresetTag) => void;
}> = ({ title, dimension, tags, localeKey, busy, onCreate, onRename, onDelete }) => {
  const { t } = useTranslation();
  const [newLabel, setNewLabel] = useState('');

  const submit = () => {
    const label = newLabel.trim();
    if (!label) return;
    onCreate(label);
    setNewLabel('');
  };

  return (
    <div className='flex flex-col gap-10px min-w-0'>
      <div className='flex items-center gap-7px'>
        <span className='inline-block w-3px h-13px rounded-[2px] bg-[var(--color-primary-light-3)]' aria-hidden='true' />
        <span className='text-13px font-medium text-[var(--color-text-1)]'>{title}</span>
        <span className='text-11px text-[var(--color-text-3)]'>({tags.length})</span>
      </div>

      <div className='flex flex-col gap-6px' data-testid={`tag-column-${dimension}`}>
        {tags.length === 0 ? (
          <div className='rounded-10px border border-dashed border-[var(--color-border-2)] px-10px py-12px text-center text-12px text-[var(--color-text-3)]'>
            {t('settings.presetTagColumnEmpty', { defaultValue: 'No tags in this group yet.' })}
          </div>
        ) : (
          tags.map((tag) => (
            <TagRow
              key={tag.preset_tag_id}
              tag={tag}
              localeKey={localeKey}
              busy={busy}
              onRename={onRename}
              onDelete={onDelete}
            />
          ))
        )}
      </div>

      <div className='flex items-center gap-8px mt-2px'>
        <Input
          size='small'
          value={newLabel}
          onChange={setNewLabel}
          onPressEnter={submit}
          disabled={busy}
          data-testid={`tag-add-input-${dimension}`}
          placeholder={t('settings.presetTagAddPlaceholder', { defaultValue: 'New tag…' })}
          className='flex-1 !rounded-8px'
        />
        <div
          role='button'
          tabIndex={0}
          data-testid={`tag-add-btn-${dimension}`}
          onClick={submit}
          onKeyDown={(e) => {
            if (e.key === 'Enter') submit();
          }}
          className={[
            'flex-shrink-0 inline-flex items-center gap-4px rounded-8px px-10px h-30px text-12px font-medium cursor-pointer',
            'border border-solid transition-all duration-150',
            newLabel.trim() && !busy
              ? 'bg-[var(--color-primary-light-1)] text-[rgb(var(--primary-6))] border-[var(--color-primary-light-3)] hover:bg-[var(--color-primary-light-2)]'
              : 'bg-[var(--color-fill-2)] text-[var(--color-text-3)] border-[var(--color-border-2)] cursor-not-allowed',
          ].join(' ')}
        >
          <Plus theme='outline' size={13} strokeWidth={3} />
          {t('common.add', { defaultValue: 'Add' })}
        </div>
      </div>
    </div>
  );
};

const TagManagementModal: React.FC<TagManagementModalProps> = ({
  visible,
  onClose,
  audienceTags,
  scenarioTags,
  localeKey,
  onCreate,
  onRename,
  onDelete,
  message,
}) => {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);

  const handleCreate = async (dimension: PresetTagDimension, label: string) => {
    setBusy(true);
    try {
      await onCreate({ dimension, label });
    } catch (error) {
      console.error('Failed to create tag:', error);
      message.error(
        errorText(error) || t('settings.presetTagCreateFailed', { defaultValue: 'Failed to create tag' })
      );
    } finally {
      setBusy(false);
    }
  };

  const handleRename = async (presetTagId: PresetTagId, label: string) => {
    setBusy(true);
    try {
      await onRename(presetTagId, label);
    } catch (error) {
      console.error('Failed to rename tag:', error);
      message.error(
        errorText(error) || t('settings.presetTagRenameFailed', { defaultValue: 'Failed to rename tag' })
      );
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = (tag: PresetTag) => {
    const label = tag.label_i18n?.[localeKey] || tag.label;
    Modal.confirm({
      title: t('settings.presetTagDeleteTitle', { defaultValue: 'Delete tag' }),
      content: t('settings.presetTagDeleteConfirm', {
        defaultValue: 'Delete "{{label}}"? It will be removed from all presets.',
        label,
      }),
      okText: t('common.delete', { defaultValue: 'Delete' }),
      cancelText: t('common.cancel', { defaultValue: 'Cancel' }),
      okButtonProps: { status: 'danger' },
      onOk: async () => {
        setBusy(true);
        try {
          await onDelete(tag.preset_tag_id);
        } catch (error) {
          console.error('Failed to delete tag:', error);
          message.error(
            errorText(error) || t('settings.presetTagDeleteFailed', { defaultValue: 'Failed to delete tag' })
          );
        } finally {
          setBusy(false);
        }
      },
    });
  };

  return (
    <Modal
      visible={visible}
      onCancel={onClose}
      footer={null}
      title={t('settings.presetTagModalTitle', { defaultValue: 'Manage Tags' })}
      style={{ width: 680, maxWidth: '92vw', borderRadius: 16 }}
      maskClosable={!busy}
      data-testid='tag-management-modal'
    >
      <p className='mt-0 mb-16px text-12px leading-18px text-[var(--color-text-3)]'>
        {t('settings.presetTagModalDesc', {
          defaultValue:
            'Organize presets by audience and skill scenario. Built-in tags are locked; your own tags can be renamed or deleted.',
        })}
      </p>
      <div className='grid gap-20px' style={{ gridTemplateColumns: 'repeat(auto-fit, minmax(min(240px, 100%), 1fr))' }}>
        <TagColumn
          title={t('settings.presetTagAudience', { defaultValue: 'Audience' })}
          dimension='audience'
          tags={audienceTags}
          localeKey={localeKey}
          busy={busy}
          onCreate={(label) => void handleCreate('audience', label)}
          onRename={(key, label) => void handleRename(key, label)}
          onDelete={handleDelete}
        />
        <TagColumn
          title={t('settings.presetTagScenario', { defaultValue: 'Skill Scenario' })}
          dimension='scenario'
          tags={scenarioTags}
          localeKey={localeKey}
          busy={busy}
          onCreate={(label) => void handleCreate('scenario', label)}
          onRename={(key, label) => void handleRename(key, label)}
          onDelete={handleDelete}
        />
      </div>
    </Modal>
  );
};

export default TagManagementModal;
