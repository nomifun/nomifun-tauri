-- Orchestrator P5: remove the legacy `team` subsystem.
--
-- The multi-agent orchestrator (P0–P3b) fully replaces the inert legacy team
-- engine, which has been deleted at the crate/wiring level. This migration
-- drops its now-orphaned tables so no historical schema debt remains.
--
-- Migrations run with `PRAGMA foreign_keys = OFF` (see `run_migrations` in
-- database.rs), so DROP order cannot trigger ON DELETE CASCADE. The order below
-- is still child-before-parent for clarity, and every DROP is `IF EXISTS` so the
-- migration is a no-op on databases that never created the team tables.
DROP TABLE IF EXISTS team_task_deps;
DROP TABLE IF EXISTS team_tasks;
DROP TABLE IF EXISTS mailbox;
DROP TABLE IF EXISTS team_agents;
DROP TABLE IF EXISTS teams;
