# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`ai-team` is a Rust CLI (edition 2024, tokio) for a reusable "AI software company": the Owner (human) gives projects, and an orchestrator runs Planner → Architect → Builder ⇄ Reviewer. Employees are defined as files in `company/employees/`. `firma.md` is the phased plan (Romanian); phases 1–4 and 5a are implemented, and phase 6 is in progress (stabilization, budgets, KPIs done). Phase 5a covers subagents, skill packs, MCP and the veto. Phase 5b (a Python Worker Gateway) is deliberately on hold until a specialist needs more than MCP tool calls: long runs, state kept between tasks, progress streaming or another machine. Until then, connect Python specialists as MCP servers (see `firma.md`, phase 5). The README and `firma.md` are in Romanian. Code, prompts, job descriptions and identifiers are in English.

## Commands

- **Windows toolchain gotcha:** the active toolchain is `stable-x86_64-pc-windows-gnu`. It needs `dlltool.exe` from MSYS2 (`C:\msys64\ucrt64\bin`) on `PATH`, otherwise `getrandom`/`windows-sys` fail to compile. If a shell lacks it: `$env:Path = "C:\msys64\ucrt64\bin;$env:Path"`.
- Build / lint / format: `cargo build`, `cargo clippy --all-targets`, `cargo fmt`
- Test: `cargo test`. Single test: `cargo test --test registry rejects_invalid_contracts`
- CLI (global `--company <dir>`, default `company`):
  - `doctor`: validate the registry and probe each Ollama server: reachable, model installed, `num_ctx` within the model's maximum context. It exits non-zero on problems. `--offline` skips the network checks.
  - `team`: show the org chart.
  - `run --project projects/x.json --task "..." [--json]`
  - `hire propose|check|approve <ID>`
  - `employees set-status <ID> <active|suspended|disabled>`
  - Phase 3 commands (global `--data <dir>`, default `data/`, gitignored):
    - `project add <file.json>`, `project list`
    - `project plan <id> [--note]` → `project approve <id>`
    - `project run <id> [--max-tasks N]`, `project status <id>`
    - `task show <id>`
    - Owner decisions: `task resume <id> [--note] [--iterations N]`, `task accept <id>`, `task cancel <id>`
    - `project configure <id> [--test-command "..."] [--test-timeout N] [--remote <url>] [--budget-tokens N] [--budget-usd X]` (`ProjectSettings`; 0 removes a budget limit)
    - `kpi [--project ID]`: employee KPIs (`src/kpi.rs`, firma.md §18) from stored task states and Owner decisions.
    - `console [--project ID]` is the interactive Owner console (`owner_console` in `main.rs`).
      - You type a multi-line request (an empty line sends it), then file paths (quotes from Windows drag-and-drop are stripped) and a test command for a new project.
      - It calls `owner_request` and shows progress live (`ProjectManager::with_progress`: each new history line printed from the checkpoint).
      - It then asks resume/accept/cancel for stopped tasks. Commands: `/nou [name]` (names the next new project), `/proiect <id>`, `/status`, `/ajutor`, `/iesire`.
      - `?<question>` (or a request ending in `?`, after confirmation) calls `ProjectManager::ask` instead of creating a task.
    - `ask <project> "<question>"`: `ProjectManager::ask` answers from the project files and task list with the active Architect's model (`Orchestrator::consultant`). It is read-only: no task, no commit; usage goes to planning usage. Added because questions typed as requests became tasks that rewrote approved code.
    - `request "<prompt>" [--file F]... [--project ID | --name NAME] [--test-command C] [--plan [--yes]]` is the Owner's one-shot entry.
      - It creates a project, or adds to one with `--project`, and commits the files to `inputs/` on main as `OWNER` (`ProjectManager::add_inputs`).
      - The prompt becomes a task with `add_request`: an `"Owner requests"` milestone, no plan approval needed, and a done project reopens.
      - Then it runs the project.
      - File previews (the first 40 lines of text files) go into the project objective, so every agent sees them.
- `main` calls `refuse_inside_workspace`: with a relative `--data`, the CLI refuses to run inside `<data>/workspaces/<project>`, because it would create a stray `data/` that `save_uncommitted` then commits. The console suggests `python -m unittest discover -s tests -v` as a new project's test command (`-` = none), and `owner_request` warns when a new project has no test command.
- Audit locations: CLI runs write to `.ai-team/runs/<run_id>.jsonl` and HR decisions to `.ai-team/hr.jsonl`. Don't leave demo hires or status changes in the real `company/`; try them on a copy with `--company`.

## Architecture

- **Registry (`src/registry/`).** One folder per employee: `contract.yaml` (serde, `deny_unknown_fields`) plus `job_description.md`, which is used verbatim as the agent's system prompt.
  - `Registry::load` validates structure and returns all violations at once.
  - Validation rules (`validate` in `registry/mod.rs`): exactly one `system` orchestrator reporting to `OWNER`, with no model and no permissions; at most one active planner; agents report to the orchestrator; subagents report to an agent that has `delegate_subtasks`, have permissions ⊆ the manager's, and cannot delegate; a function requires its capability; `write_implementation` and `review_work` cannot be held together; the folder name must equal `employee_id`.
  - Add new rules there, and add a case to the table test in `tests/registry.rs`.
- **Capabilities.** The `Capability` enum lists only actions the control plane enforces today. Workspace/tool capabilities arrive in phase 4. Don't add a capability nothing enforces; `firma.md` §17 forbids it.
- **Hiring (`registry/hire.rs`).**
  - `propose_hire` writes a skeleton with `TODO`s to `company/proposals/<ID>/`.
  - `check_proposal` validates the registry as if the employee were hired, and rejects leftover `TODO`s.
  - `approve_hire` moves the folder and records `HIRE_APPROVED`.
  - `set_status` edits only the `status:` line, preserving comments, and reverts if the registry becomes invalid.
- **Orchestrator (`src/orchestrator.rs`).** Staffing is either `Fixed(Team)` (`with_agents`, used in tests and for composite leads) or `Company { registry, planner }` (`from_registry`). Per run:
  1. The team comes from `project.assigned_team` (Owner wins) or else from `Planner::propose`.
  2. `staffing::validate_assignment` checks it and records `PLAN_REJECTED` on failure.
     - A rejected Planner proposal, or an unparseable one, is never executed. `plan_team` feeds the rejection reason back to the Planner, up to `MAX_PLAN_ATTEMPTS` (3), then fails the run.
     - An invalid `assigned_team` fails the run immediately.
     - Small models do mangle IDs (a real run produced `EMP-BUILD-0-01`), so keep this loop.
  3. If builder and reviewer share a model, `same_model_warning` records `POLICY_WARNING`; this does not block the run.
  4. `build_team` creates agents named by `employee_id`, which is also the audit actor.
  5. The workflow loop runs (spec revision via `target: architect`, `HumanReviewRequired` at the iteration limit). Any error leads to `Stage::Failed` + `RUN_FAILED` + `Err`.
- **`Agent` trait (`src/agents/mod.rs`).** `execute(ctx, &WorkOrder) -> BoxFuture<Result<AgentOutput>>`. It is object-safe through the hand-boxed `crate::BoxFuture`. A lead may delegate to subagents via `ctx.child(name)` (nested audit spans). `WorkOrder` has the same shape for every role; `specification: Some` tells the Architect to revise.
- **Model replies are parsed fail-closed** via `agents/prompt.rs::parse_json_reply`: strict JSON, or else the outermost `{...}`.
  - A malformed review is re-asked (`reviewer::FORMAT_RETRIES` = 2). It is never interpreted, and is an error after the retries.
  - A malformed plan counts as a rejected attempt.
  - `Planner::propose` returns `Result<Result<Plan, String>>`: the outer error is transport (fatal), the inner one is an unusable reply (retryable with feedback).
  - `Orchestrator::with_planner` injects a planner, e.g. a scripted one in tests.
- **`LlmProvider` (`src/llm.rs`).** The only implementation is `HttpChatProvider`, with three dialects chosen by `provider`. There is deliberately no mock provider, so a contract can only name a real backend.
  - `openai`: `/chat/completions`, bearer auth.
  - `ollama`: native `/api/chat` with `options.num_ctx` / `num_predict`. Ollama's `/v1` endpoint ignores `num_ctx`, so don't switch Ollama to the openai dialect.
  - `claude`: `/v1/messages`, with `x-api-key` and `anthropic-version: 2023-06-01`. It never sends `temperature` (current models 400 on sampling params). `max_tokens` defaults to 16000, plus optional `output_config.effort`. `stop_reason` `refusal` or `max_tokens` is a fatal error.
  - All dialects retry only timeouts, connection errors, 429 and 5xx, and strip a leading `<think>` block.
  - A truncated answer is a fatal, non-retried error in every dialect: Ollama `done_reason: length`, OpenAI `finish_reason: length`, Claude `stop_reason: max_tokens`. A real run showed a 4B Reviewer approving a cut-off implementation, so don't relax this.
  - `ModelConfig::validate` rejects options the chosen provider would ignore.
  - Agents get their backends from a `ProviderFactory`. `Orchestrator::from_registry` uses `http_providers()`; tests call `from_registry_with_providers(registry, test_providers())`.
  - The shipped `company/` points at Ollama on `http://10.10.0.14:11434`: Planner, Architect and Builder use `qwen3-coder:30b`, the Reviewer uses `qwen2.5:14b`, all with `num_ctx` 32768. `run` needs that server. Tests never touch the network.
  - An iteration is counted only after the Builder delivers (`orchestrator.rs`), so a failed or truncated Builder call does not use up a task's budget.
  - The Builder and Architect job descriptions carry a "Size and focus" section (about 2,500 / 2,000 words, no boilerplate). Without it, `qwen3-coder` wrote full Java classes and revisions overflowed even 8192 tokens.
  - `num_predict` is 8192 for Architect/Builder, 4096 for Planner and 1200 for Reviewer. 4096 was too small: on a real project task, the Builder's second iteration (full rewrite plus fixes) was cut off.
  - Keep `num_ctx` at 32768 or lower: at 65536 `qwen3-coder:30b` spills out of the GPU and drops from about 147 to about 7 tok/s.
  - Thinking models (`qwen3:4b`, `qwen3.6`) currently return invalid JSON, because thinking consumes `num_predict`.
- `ModelConfig` (`src/config.rs`) is the `model:` block of a contract. Projects are `projects/*.json`.

- **Projects (`src/project/`, phase 3).**
  - **Plan.** `plan.rs` validates the Planner's task plan (`Planner::propose_tasks`): max 25 tasks, unique keys, at least one criterion each, dependencies only on the same or earlier milestones, no cycles. It also returns the execution order (Kahn's algorithm, ties broken by milestone and then by listing order). `ProjectManager::plan` retries with feedback (3 attempts) and stores only valid plans, as `PROPOSED`. `approve` creates the tasks `<project>-T01..`.
  - **Store.** `store.rs` uses SQLite (`rusqlite` with bundled SQLite; it needs MSYS2 `gcc` on the GNU toolchain) and a content-addressed `artifacts/<sha256>.md` store. `save_task_state` replaces artifact contents with `artifact:<hash>`, and `load_task_state` restores them.
  - **Run.** `manager.rs::run` runs one task at a time: interrupted tasks first (`TaskStatus::is_interrupted`), otherwise the earliest `PLANNED` task whose dependencies are all `DONE`. `refresh_blocked` maintains `BLOCKED` in one pass because tasks are stored in dependency order.
  - **Resume and checkpoints.** Each task runs through `Orchestrator::start` / `resume`, with a checkpoint that persists every step (`status_for(stage)`). `resume` keeps the team, specification and last implementation, under a new run id and audit file. A failed run still returns its state (`RunResult`).
  - **Task text and limits.** The task text is composed by `compose_task`: the task, project criteria as context, dependency implementations (truncated to 6000 characters) and Owner `resume` notes. The task's own acceptance criteria replace the project's in the reviewer prompt, and `iteration_budget` replaces `max_iterations`.
  - **Owner decisions.** `resume` (adds iterations, back to `PLANNED`), `accept` (manual approval, needs an implementation, reported as Owner-approved) and `cancel`. A milestone report is written once all its tasks are `DONE` or `CANCELLED`.

- **Execution (phase 4).**
  - **Git workspace.** `src/workspace/` keeps one git repository per project at `<data>/workspaces/<project>/`. `Workspace::open` creates it empty, with an initial commit holding the platform `.gitignore` and `core.autocrlf=false`.
    - Each task works on `task/<id>`, created from `main`. A single working tree is enough because tasks are sequential.
    - `start_task` and `checkout_main` first commit any leftover changes ("Save uncommitted work").
    - For an existing branch, `start_task` merges `main` into it (`sync_with_main`). Conflicts are committed with their markers on the task branch, and the conflicted files are returned. `TaskWorkbench::with_conflicts` puts them at the top of the Builder briefing, so resolving them becomes Builder work, checked by tests and review. This came from a real run, where a stale branch conflicted on `__init__.py` at the final merge.
    - Every Builder iteration is committed in `TaskWorkbench::verify` with the employee as author (`-c user.name=...`), whether it passed or not.
    - `merge_task` runs `git merge --no-ff` as `EMP-ORCH-001`, and `merge --abort`s on a conflict so `main` stays untouched.
    - Pushes go only to the Owner's `remote_url` (`project configure --remote`), with no force and `GIT_TERMINAL_PROMPT=0`: the task branch after every task, `main` after every merge. A failed push is reported in `RunSummary.push_errors` and task history and never changes the outcome. All git calls shell out to `git`.
  - **Answer format and limits.** The Builder answers with `### FILE: path` plus a fenced block, or with `### DELETE: path`; `extract_files` returns an `AnswerFiles`. `.gitignore` is platform-managed. `validate_path` rejects `..`, absolute paths, drive letters, `.git`, build directories (`SKIP_DIRS`) and Windows reserved names. Limits: 60 files and 256 KB per file.
  - **Verification warnings.** `Verification.warnings` do not block the work; they reach the Reviewer through `report()` as `WARNING: ... (approve only if the task asked for this)`. Two sources:
    - `Workspace::test_regressions`: test files on `main` (`is_test_path`) that are gone, or that have fewer cases (`count_test_cases`). A real follow-up task rewrote approved tests from 6 to 2 and nobody noticed.
    - `differs_from_main` being false: the task changed nothing (in a real run the work was already done).
  - **Test runner.** `runner.rs` runs the Owner's `test_command` through `cmd /C` or `sh -c`, with `env_clear()` plus `ALLOWED_ENV`, so no secrets reach the tests. On timeout it kills the whole tree (`taskkill /T`) *before* dropping the shell; dropping first orphaned children (bug found in phase 4).
  - **Workflow gates.** `Workbench` (`workbench.rs`) plugs into `Orchestrator::start`/`resume`. Per iteration the order is: briefing in the Builder prompt, Builder answers, `verify` writes the files and runs the tests, the Reviewer runs only if `Verification::passed()` and sees the platform report, otherwise the Builder gets the report as feedback. The project manager merges the task branch on `DONE` and on Owner `accept`; a merge conflict means `FAILED`.
  - **Capabilities.** `write_workspace` and `run_tests` require `write_implementation`. The orchestrator checks the Builder holds what `Workbench::required_capabilities` returns.
  - **Metering.** `LlmProvider::generate` returns a `Completion { text, usage }`; `complete` is a convenience wrapper. `from_registry_with_providers` wraps the factory with `llm::metered` into a `UsageLedger`. `Orchestrator::mark` drains the ledger into `TeamState.usage`, and planning usage is stored per project. Optional `cost_per_mtok_*` in `ModelConfig` set prices.
  - **Budgets and KPIs (phase 6).**
    - `ProjectConfig.budget` (`Budget { max_tokens, max_cost_usd }`) is checked in `ProjectManager::run` before each task against `project_usage`. A started task finishes; the next one is not started and `RunSummary.budget_stop` says why. Enforcement is deliberately per task, not mid-task: stopping a run halfway would leave a task FAILED for a reason that is not the team's.
    - `kpi::collect` builds `TaskFacts` per started task (team, iterations, spec revisions, usage, Owner accept/resume/cancel, verification warnings) and aggregates them overall, per Builder and per Reviewer. An Owner accept never counts as a first-pass Reviewer approval. Unit-test new KPI rules on `KpiReport::add`.
  - **Database.** The schema is at version 2 and `Store::open` migrates version 1 in place.

- **Phase 5a: subagents, skills, MCP.**
  - **Skill packs.** They live in `company/skills/<id>/SKILL.md` (optional YAML front matter) and are referenced by `skill_packs` in a contract. `Registry::system_prompt` appends them to the job description; always use it, never `job_description` directly, when building agents.
  - **MCP servers.** The catalog is `company/mcp.yaml` (`McpCatalog`); a contract references servers through `mcp_servers`. `src/mcp.rs` is a stdio JSON-RPC client: handshake, `tools/list` with pagination, `tools/call`. Servers get `env_clear` plus `allowed_env` plus `env`/`env_from`. Server-initiated requests are declined. `Toolbox` (one per employee, built by `Registry::toolbox_for`) starts servers lazily and exposes tools as `<server>__<tool>`. A failing tool call becomes an error outcome for the model, not a task failure.
  - **Tool calling.** `LlmProvider::chat(system, messages, tools)` is the core of `HttpChatProvider` for all three dialects; `generate` wraps it. `agents::tooling::answer` runs at most 8 tool rounds per answer and audits every `TOOL_CALL`. Every agent and specialist answers through it.
  - **Follow-up tasks.** The Architect also sees the project's current files (`Workbench::snapshot`), and `compose_task` adds a CONTEXT note once the project has a `DONE` task: build on approved work, and the OBJECTIVE is only the Owner's first request. Without this, a later task was specified from the objective and the Builder rewrote working code.
  - **Delegation.** `agents/lead.rs::BuilderLead` is used by `staffing::build_team` when the Builder has `delegate_subtasks` and active subagents with `write_implementation`.
    - Each iteration it asks for a split (`DELEGATION_HEADING`), which `validate_delegation` checks: team members only, at most 4 subtasks, disjoint file ownership.
    - A rejected split is retried once, then the lead works solo.
    - Each `Specialist` gets only its subtask and files. Files outside a subtask are dropped and recorded in `SUBTASK_DONE`.
  - **Reviewer advisors.** Reviewer subagents with `review_work` advise as `Advisor`. A `veto_review` advisor's `CHANGES_REQUIRED` overrides the Reviewer (`VETO_APPLIED`). A failing or invalid veto advisor counts as a rejection (fail closed).
  - **Validation rules.** The registry rejects unknown skill packs or servers, a subagent with servers its manager lacks, `veto_review` outside a reviewer's subagent, and duplicate entries. `Role::Specialist` meters subagent calls.

## Tests

`tests/common/mod.rs` provides:
- `sample_company(dir)`: a valid five-employee registry written to a tempdir; its contracts point at an unused URL;
- `TestLlm` / `test_providers()`: an offline role-aware fake. The Reviewer rejects its first call and then approves. The Planner picks the first `- EMP-... | function |` roster line of its prompt per function, so keep that roster format in `planner.rs`;
- `edit_contract` and `write_employee`, for breaking one rule at a time;
- `ScriptedProvider`, which returns queued answers and records prompts; an empty queue makes the agent fail.

`tests/subagents.rs` covers phase 5a:
- MCP and tool calling, against the shipped `pydoc` server (needs `python`) and an Ollama-shaped HTTP fake;
- delegation with dropped out-of-scope files;
- solo fallback after an invalid split;
- the veto.

`TestLlm` answers delegation requests (one `work/<id>.txt` per team member), plays Specialist (writes its owned files, or approves when acting as an advisor) and reports fake usage. `tests/execution.rs` covers phase 4: the gates, path safety, per-iteration commits and their authors, git merge conflicts, deletions, pushes (to a local bare repo), capabilities and metering. The git tests need `git` on `PATH`. Its test commands are cross-platform shell one-liners. `TestLlm`'s Builder writes `notes/result.md`, and it reports fake usage. `tests/projects.rs` covers phase 3 end to end with `TestLlm`, whose Planner also answers task-plan prompts with `test_task_plan()`: T1 → T2 → T3 across 2 milestones. Interrupted runs are simulated by saving a mid-run `TeamState` with `save_task_state`. `shipped_registry_is_valid_and_ready` keeps `company/employees` valid. Prefer `ScriptedProvider` over `TestLlm` when asserting on prompt contents.
