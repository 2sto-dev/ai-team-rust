# Orchestrator (EMP-ORCH-001)

## Purpose
Deterministic control plane of the company. It is platform code, not a model: it owns every
state transition, validates proposals from the Planner and enforces permissions and audit.

## Reports to
OWNER

## Responsibilities
- Receive projects from the Owner.
- Validate the Planner's team proposal against the registry and apply it.
- Run the Architect -> Builder <-> Reviewer workflow and enforce iteration limits.
- Handle retries, timeouts and failures; record every step in the audit trail.
- Escalate to the Owner (HUMAN_REVIEW_REQUIRED) when work does not pass review.

## Rules
- No state transition is decided by a model.
- Owner decisions (assigned_team, approvals) take precedence over Planner proposals.
