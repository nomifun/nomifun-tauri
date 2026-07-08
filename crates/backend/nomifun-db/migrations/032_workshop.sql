-- 032 创意工坊 (Creative Workshop) 域。append-only；基线不动。
-- 三表：画布轻索引 workshop_canvases（正文 canvas.json 落文件系统，本表只存元数据+node_count）、
-- 资产 workshop_assets（元数据入库，二进制落 data_dir）、生成任务 creation_tasks（状态机+参数快照）。
-- ID 规则：跨表实体用 TEXT 前缀 id（应用层 generate_prefixed_id：画布 wsc_ / 资产 wsa_ / 任务 wst_）。
-- 时间戳一律 INTEGER 毫秒（Unix ms）。
-- creation_tasks.provider_id 为 TEXT，与 providers.id 的真实类型（TEXT）一致（已核对）。

CREATE TABLE workshop_canvases (
    id                 TEXT PRIMARY KEY,
    title              TEXT NOT NULL,
    thumbnail_rel_path TEXT,
    node_count         INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);
CREATE INDEX idx_workshop_canvases_updated ON workshop_canvases(updated_at);

CREATE TABLE workshop_assets (
    id             TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,                 -- image | video | text
    title          TEXT NOT NULL,
    collection     TEXT,                          -- 集合（角色/场景…），可空
    tags           TEXT NOT NULL DEFAULT '[]',    -- JSON string[]
    rel_path       TEXT,                          -- 相对 data_dir；text 资产可空
    thumb_rel_path TEXT,
    mime           TEXT,
    width          INTEGER,
    height         INTEGER,
    bytes          INTEGER,
    text_content   TEXT,                          -- kind=text 正文
    in_library     INTEGER NOT NULL DEFAULT 1,    -- 1=出现在资产库；0=画布内部素材
    origin         TEXT,                          -- JSON:{prompt,model,provider_id,params,canvas_id,node_id,task_id} 可空
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX idx_workshop_assets_kind ON workshop_assets(kind);
CREATE INDEX idx_workshop_assets_library ON workshop_assets(in_library);

CREATE TABLE creation_tasks (
    id               TEXT PRIMARY KEY,
    canvas_id        TEXT,
    node_id          TEXT,
    provider_id      TEXT NOT NULL,               -- 类型与 providers.id 一致（TEXT）
    model            TEXT NOT NULL,
    capability       TEXT NOT NULL,               -- t2i|i2i|inpaint|t2v|i2v|v2v|tts|text
    params           TEXT NOT NULL,               -- JSON 参数快照
    status           TEXT NOT NULL,               -- queued|running|succeeded|failed|canceled
    error            TEXT,                         -- JSON {kind,message,http_status?} 可空
    result_asset_ids TEXT NOT NULL DEFAULT '[]',
    remote_task_id   TEXT,                         -- 异步协议远端任务 id（boot 恢复轮询用）
    attempt          INTEGER NOT NULL DEFAULT 0,
    submitted_at     INTEGER NOT NULL,
    started_at       INTEGER,
    finished_at      INTEGER
);
CREATE INDEX idx_creation_tasks_status ON creation_tasks(status);
CREATE INDEX idx_creation_tasks_canvas ON creation_tasks(canvas_id);
