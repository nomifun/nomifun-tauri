-- 023 编排任务 kind 地基(ultracode 模式增强 Phase 1a)。append-only。
-- 给 orch_run_tasks 加两列,让主 agent 规划时为每个任务标注「模式 kind」:
--   kind           = 任务模式;默认 'agent'(= 完全现状,单 agent 执行一个任务,零回归)。
--                    1a 取值: 'agent' | 'synthesis'(综合/合并上游产出为最终结果)。
--   pattern_config = 该 kind 的可选配置 JSON(nullable),如 fan-out 兄弟任务共享的
--                    分组标签 {"group":"<label>"}。未用时为 NULL。
-- 旧行 kind 取默认 'agent',pattern_config 为 NULL —— 既有 run/plan 零回归。
ALTER TABLE orch_run_tasks ADD COLUMN kind TEXT NOT NULL DEFAULT 'agent';
ALTER TABLE orch_run_tasks ADD COLUMN pattern_config TEXT;
