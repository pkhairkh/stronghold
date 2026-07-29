# Stronghold Agent Resources

Resources for running multi-agent agentic coding flows through Stronghold.

## Directory Structure

```
agent/
├── stronghold-agent.sh          # Bash SDK (source this at session start)
├── README.md                    # SDK function reference
├── prompts/                     # System prompts for each agent role
│   ├── planner.md               # Planner: analyzes tasks, creates plans
│   ├── coder.md                 # Coder: implements changes, creates PRs
│   ├── reviewer.md              # Reviewer: reviews diffs, approves/rejects
│   ├── tester.md                # Tester: runs tests, reports results
│   └── integrator.md            # Integrator: merges PRs, runs CI
├── protocols/
│   └── multi-agent-coding.md    # Full multi-agent coding flow specification
└── templates/                   # Workflow DAG templates
    ├── standard-cicd.json       # Plan → Implement → Test → Review → Merge
    ├── bug-fix-fast.json        # Fix+Test → Review+Merge (fast path)
    └── multi-component-refactor.json  # Parallel refactoring + integration
```

## Quick Start

```bash
source stronghold-agent.sh
WORKFLOW=$(cat templates/standard-cicd.json)
stronghold_workflow_create "fix-issue-42" "$WORKFLOW"
stronghold_workflow_run $WORKFLOW_ID
stronghold_workflow_status $RUN_ID
```

## Agent Roles

| Role | Writes Code | Creates PRs | Merges | Tests |
|---|---|---|---|---|
| Planner | No | No | No | No |
| Coder | Yes | Yes | No | Local only |
| Reviewer | No | Comments only | No | No |
| Tester | No | No | No | Full suite |
| Integrator | No | No | Yes | Post-merge CI |

## Communication

Agents communicate via the message bus. Channel: `workflow-run-<run_id>`.

Message types: `task_assigned`, `review_requested`, `changes_requested`, `review_approved`, `test_results`, `integration_complete`, `escalation`.
