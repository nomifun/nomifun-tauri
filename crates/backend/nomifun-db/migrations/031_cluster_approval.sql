-- 031 agent集群：节点级审批模式。append-only,基线不动。
-- orch_runs.approval_mode: NULL/"auto"=全授权(节点遇抉择自行判断,现状零回归);
--   "manual"=审批模式(worker 可经 nomi_task_question 挂起提问,由用户进入节点作答)。
-- orch_run_tasks.pending_question: 节点挂起的决策问题原文(status=needs_review 时在场);
--   解决(采用产出/重跑)后清空。纯 ADD COLUMN(O(1));旧行读回 NULL —— 零回归。
ALTER TABLE orch_runs ADD COLUMN approval_mode TEXT;
ALTER TABLE orch_run_tasks ADD COLUMN pending_question TEXT;
