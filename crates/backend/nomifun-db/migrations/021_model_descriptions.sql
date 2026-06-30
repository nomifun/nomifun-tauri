-- Migration 021: 为 providers 增加 model_descriptions(用户撰写的逐模型描述)。
--
-- 编排器按描述自动择优模型(P3),需要在每个 provider 上存一份
-- model_id -> 描述文本 的 JSON 映射。与既有 model_protocols / model_enabled /
-- model_health 同型(TEXT JSON 对象),但仿 models / capabilities 列采用
-- NOT NULL DEFAULT '{}' —— 新列总有确定的空映射默认值,免去读侧 NULL 分支。
ALTER TABLE providers ADD COLUMN model_descriptions TEXT NOT NULL DEFAULT '{}';
