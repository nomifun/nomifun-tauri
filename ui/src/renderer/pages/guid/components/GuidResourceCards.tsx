/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { openExternalUrl } from '@/renderer/utils/platform';
import { BookOne, Comment, PlayOne } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import styles from '../index.module.css';

const DOCS_URL = 'https://www.nomifun.com/docs';
const VIDEO_URL_CN = 'https://www.bilibili.com/video/BV1kwKZ6UE5X/';
const VIDEO_URL_GLOBAL = 'https://youtu.be/AsEToBDFR9s';
const FEEDBACK_URL = 'https://www.nomifun.com/contact';

const ResourceLinkCard: React.FC<{
  icon: React.ReactNode;
  title: string;
  description: string;
  action: string;
  url: string;
}> = ({ icon, title, description, action, url }) => (
  <button type='button' className={styles.guidResourceCard} onClick={() => void openExternalUrl(url)}>
    <span className={styles.guidResourceCardHeader}>
      <span className={styles.guidResourceIcon}>{icon}</span>
      <span className={styles.guidResourceTitle}>{title}</span>
    </span>
    <span className={styles.guidResourceDescription}>{description}</span>
    <span className={styles.guidResourceAction}>{action}</span>
  </button>
);

const GuidResourceCards: React.FC = () => {
  const { t, i18n } = useTranslation();
  const language = i18n.resolvedLanguage || i18n.language;
  const videoUrl = language.toLowerCase().startsWith('zh') ? VIDEO_URL_CN : VIDEO_URL_GLOBAL;

  return (
    <div className={styles.guidResourceCards} data-testid='guid-resource-cards'>
      <ResourceLinkCard
        icon={<BookOne theme='outline' size='18' fill='currentColor' />}
        title={t('conversation.emptyCards.docsTitle')}
        description={t('conversation.emptyCards.docsDescription')}
        action={t('conversation.emptyCards.docsAction')}
        url={DOCS_URL}
      />
      <ResourceLinkCard
        icon={<PlayOne theme='outline' size='18' fill='currentColor' />}
        title={t('conversation.emptyCards.videoTitle')}
        description={t('conversation.emptyCards.videoDescription')}
        action={t('conversation.emptyCards.videoAction')}
        url={videoUrl}
      />
      <ResourceLinkCard
        icon={<Comment theme='outline' size='18' fill='currentColor' />}
        title={t('conversation.emptyCards.feedbackTitle')}
        description={t('conversation.emptyCards.feedbackDescription')}
        action={t('conversation.emptyCards.feedbackAction')}
        url={FEEDBACK_URL}
      />
    </div>
  );
};

export default GuidResourceCards;
