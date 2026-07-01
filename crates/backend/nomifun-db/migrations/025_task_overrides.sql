-- 025 节点启动前配置台(选模型 + 预置要求)。append-only,基线不动。
-- 给 orch_run_tasks 加三个可空列,让用户在任务【启动前】为单个节点覆盖模型 / 预置一段要求:
--   override_provider_id / override_model = per-task 模型覆盖(任意可用 provider×model,
--       不受 run 创建时冻结的 fleet 池限制)。引擎 dispatch 时若二者都非空,则用它覆写该
--       节点解析出的成员的 provider/model(保留成员的角色/persona)。为空 = 跟随自动路由。
--   preset_prompt = 用户预置要求。引擎组装该节点 worker brief 时,作为独立一段追加(与
--       规划器写的任务描述 spec 分离,不覆盖它)。为空 = 无附加要求。
-- 三列全可空,纯 ADD COLUMN(O(1),不重建表);旧行读回 NULL —— 既有 run/plan 零回归。
ALTER TABLE orch_run_tasks ADD COLUMN override_provider_id TEXT;
ALTER TABLE orch_run_tasks ADD COLUMN override_model TEXT;
ALTER TABLE orch_run_tasks ADD COLUMN preset_prompt TEXT;
