-- 029 编排节点失败策略。append-only,基线不动。
-- 给 orch_run_tasks 加可空列 on_fail:节点永久失败时的处置策略。
--   NULL / "fail_run"        = 默认:任一必需节点永久失败即整 run 判 failed(现状,零回归)。
--   "skip_and_continue"      = 跳过该节点的传递性下游(标 skipped),其余独立分支照常跑完;
--                              run 全部 settled 且无 fail_run 硬失败时,判 completed_with_failures。
-- 纯 ADD COLUMN(O(1));旧行读回 NULL = fail_run —— 既有 run/plan 零回归。
ALTER TABLE orch_run_tasks ADD COLUMN on_fail TEXT;
