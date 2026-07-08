-- Authoritative per-model capability profiles, keyed by (provider_id, model).
-- Supersedes the name-only heuristic as the runtime source of truth for a
-- model's tasks/traits and service-config params. Rows are seeded (source =
-- 'inferred') by a boot-time reconciler and may be overridden by the user
-- (source = 'user') or a managed catalog (source = 'catalog').
--
-- Columns:
--   tasks  : JSON array of ModelTask   (e.g. ["image_generation","image_edit"])
--   traits : JSON array of ModelTrait  (e.g. ["vision_input"])
--   params : JSON object of service config (image size/steps, tts voice,
--            asr language, endpoint/request-shape overrides, timeout, …)
--   source : 'inferred' | 'user' | 'catalog'
CREATE TABLE IF NOT EXISTS model_profiles (
    provider_id TEXT    NOT NULL,
    model       TEXT    NOT NULL,
    tasks       TEXT    NOT NULL DEFAULT '[]',
    traits      TEXT    NOT NULL DEFAULT '[]',
    params      TEXT    NOT NULL DEFAULT '{}',
    source      TEXT    NOT NULL DEFAULT 'inferred',
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider_id, model),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
);
