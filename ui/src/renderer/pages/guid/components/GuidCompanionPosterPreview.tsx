/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ICompanionWithStatus } from '@/common/adapter/ipcBridge';
import CompanionAvatar from '@/renderer/pages/companion/CompanionAvatar';
import { CHARACTERS } from '@/renderer/pages/companion/characters';
import type { CompanionActivity, CompanionMood, CustomFigureMeta } from '@/renderer/pages/companion/characters';
import { customFigureMetaOf } from '@/renderer/pages/companion/characters/customMeta';
import { useCompanions } from '@/renderer/pages/nomi/useNomi';
import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import styles from '../index.module.css';
import type { CompanionId } from '@/common/types/ids';

type PosterFigure = {
  id: string;
  name: string;
  character: string;
  companionId?: CompanionId;
  customFigure?: CustomFigureMeta | null;
  mood: CompanionMood;
  activity: CompanionActivity;
};

const POSTER_MOODS: CompanionMood[] = ['happy', 'content', 'excited', 'content'];
const KNOWN_MOODS = new Set<CompanionMood>(['happy', 'content', 'sleepy', 'worried', 'excited']);

const resolveMood = (value: string | undefined, fallback: CompanionMood): CompanionMood =>
  value && KNOWN_MOODS.has(value as CompanionMood) ? (value as CompanionMood) : fallback;

const companionToPosterFigure = (companion: ICompanionWithStatus, index: number): PosterFigure => ({
  id: companion.companion_id,
  name: companion.name,
  character: companion.character,
  companionId: companion.companion_id,
  customFigure: customFigureMetaOf(companion),
  mood: resolveMood(companion.status?.mood, POSTER_MOODS[index % POSTER_MOODS.length]),
  activity: companion.status?.last_learn?.status === 'running' ? 'thinking' : 'idle',
});

const GuidCompanionPosterPreview: React.FC = () => {
  const { t } = useTranslation();
  const { companions, loading } = useCompanions();

  const figures = useMemo<PosterFigure[]>(() => {
    const realFigures = companions.slice(0, 4).map(companionToPosterFigure);
    if (realFigures.length > 0) return realFigures;

    return CHARACTERS.slice(0, 3).map((character, index) => ({
      id: character.id,
      name: t(`nomi.characters.${character.nameKey}.name`),
      character: character.id,
      mood: POSTER_MOODS[index % POSTER_MOODS.length],
      activity: 'idle' as const,
    }));
  }, [companions, t]);

  return (
    <section className={styles.guidCompanionPoster} aria-label={t('conversation.companionPoster.title')}>
      <div className={styles.guidCompanionPosterCopy}>
        <span className={styles.guidCompanionPosterEyebrow}>{t('conversation.companionPoster.eyebrow')}</span>
        <h2 className={styles.guidCompanionPosterTitle}>{t('conversation.companionPoster.title')}</h2>
        <p className={styles.guidCompanionPosterDescription}>
          {companions.length > 0
            ? t('conversation.companionPoster.description')
            : t('conversation.companionPoster.emptyDescription')}
        </p>
      </div>

      <div className={styles.guidCompanionPosterStage} aria-busy={loading}>
        <div className={styles.guidCompanionPosterHorizon} />
        {figures.map((figure, index) => {
          const size = index === 0 ? 154 : index === 1 ? 132 : 118;
          return (
            <figure
              key={figure.id}
              className={`${styles.guidCompanionPosterFigure} ${
                index === 0 ? styles.guidCompanionPosterFigurePrimary : ''
              }`}
              style={
                {
                  '--poster-index': index,
                  '--poster-y': `${index % 2 === 0 ? 0 : 8}px`,
                } as React.CSSProperties
              }
            >
              <div className={styles.guidCompanionPosterAvatar}>
                <CompanionAvatar
                  character={figure.character}
                  companionId={figure.companionId}
                  customFigure={figure.customFigure}
                  mood={figure.mood}
                  activity={figure.activity}
                  size={size}
                />
              </div>
              <figcaption className={styles.guidCompanionPosterName}>{figure.name}</figcaption>
            </figure>
          );
        })}
      </div>
    </section>
  );
};

export default GuidCompanionPosterPreview;
