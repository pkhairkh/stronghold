# Stronghold Agent Architecture — State-of-the-Art Multi-Agent Systems

> Synthesizes findings from recent arxiv papers on multi-agent LLM systems
> (2024-2026) with practical engineering for the Stronghold orchestration platform.

---

## 1. Agent Role Catalog

### Core Roles (already implemented)

| Role | Purpose | Reference |
|---|---|---|
| Planner | Decomposes tasks, creates DAG | ReAct (Yao et al., 2023) |
| Coder | Implements changes | CodeAct (Wang et al., 2024) |
| Reviewer | Reviews diffs | Self-Refine (Madaan et al., 2023) |
| Tester | Runs tests | AutoTest (Schäfer et al., 2024) |
| Integrator | Merges PRs, runs CI | — |

### Advanced Roles (new)

| Role | Purpose | Reference |
|---|---|---|
| **Watchdog** | Monitors agent dedication, detects workarounds, issues ultimata | MetaGPT watchdogs (Hong et al., 2023) |
| **Architect** | Makes system design decisions before implementation | ChatDev architect role (Qian et al., 2024) |
| **Facilitator** | Mediates disagreements between Coder and Reviewer | Multi-agent debate (Du et al., 2023) |
| **Security Auditor** | Runs security scans, checks for vulnerabilities | GPTLens (Sun et al., 2024) |
| **Documentation Agent** | Writes/updates docs, generates changelogs | DocAgent (Arora et al., 2024) |
| **Performance Agent** | Runs benchmarks, detects regressions | PerfAgent (Yan et al., 2024) |
| **DevOps Agent** | Handles deployment, infrastructure changes | InfraAgent (Kim et al., 2024) |
| **Oracle** | Answers questions from other agents about the codebase | RAG-based retrieval (Lewis et al., 2020) |

### Watchdog Agent (detailed)

The Watchdog is the most critical new role. It doesn't write code — it watches other agents and enforces focus.

**Responsibilities:**

1. **Commitment Tracking:** Every 60 seconds, the Watchdog queries the agent's recent PTY output and checks:
   - Is the agent working on the assigned task? (keyword matching against task spec)
   - Has the agent made measurable progress in the last 5 minutes? (file changes, test runs, git commits)
   - Is the agent exploring relevant files or going down rabbit holes?

2. **Dedication Score:** Computed as:
   ```
   dedication = (relevant_activity / total_activity) * progress_rate * task_alignment
   ```
   - `relevant_activity`: commands related to the task spec keywords
   - `total_activity`: all commands executed
   - `progress_rate`: file changes + test runs + commits per minute
   - `task_alignment`: cosine similarity between recent output and task instruction embedding

3. **Ultimatum Protocol:** When `dedication < 0.3` for 3 consecutive checks (3 minutes):
   - **Level 1 (Warning):** Inject `{"type":"ultimatum","level":1,"message":"You appear to be off-task. Your current work does not align with: <task_instruction>. Please refocus."}` via control channel
   - **Level 2 (Directive):** If dedication stays low after Level 1 (2 more minutes):
     `{"type":"ultimatum","level":2,"message":"You must refocus on: <task_instruction>. Stop current work. Acknowledge by running: echo ACK_TASK_FOCUS"}`
   - **Level 3 (Escalation):** If no acknowledgment after Level 2 (2 more minutes):
     - Post `escalation` on the message bus to the Planner
     - Notify the human via phone push
     - The human can: reprompt, extend TTL, or revoke the session

4. **Workaround Detection:** The Watchdog scans for patterns that indicate the agent is taking shortcuts instead of implementing proper solutions:
   - `// TODO` or `// FIXME` in committed code (excluding pre-existing ones)
   - `unwrap()` or `expect()` in production code (when the project convention forbids it)
   - `#[allow(dead_code)]` or `#[allow(clippy::...)]` added to suppress warnings
   - Tests marked `#[ignore]` without justification
   - `println!` or `dbg!` in committed code
   - Empty function bodies (`fn foo() {}`)
   - Commented-out code blocks
   - `unimplemented!()` or `todo!()` macros

5. **Simplification Avoidance:** The Watchdog checks if the agent is implementing the full specification or a simplified version:
   - Compare the task spec's requirements against the actual implementation
   - Flag missing error handling, missing edge cases, missing tests
   - Detect if the agent reduced scope without explicit approval

---

## 2. Team Strategies

### Strategy A: Hierarchical Delegation (default)

```
Human → Planner → Coder(s) → Reviewer → Integrator
                ↑ Watchdog monitors all ↓
```

The Planner is the team lead. It decomposes the task, assigns sub-tasks to Coders, coordinates with the Reviewer, and hands off to the Integrator. The Watchdog runs in parallel, monitoring all agents.

**When to use:** Complex multi-file tasks, refactors, new features.

### Strategy B: Debate-Based Consensus

```
Coder-A → solution-1 ↘
Coder-B → solution-2 → Reviewer → Facilitator → winning solution
Coder-C → solution-3 ↗
```

Multiple Coders independently implement the same task. The Reviewer compares solutions. The Facilitator mediates if Coders disagree on approach. The best solution (by test pass rate, code quality, and Reviewer judgment) wins.

**When to use:** Hard bugs with multiple possible approaches, algorithm design, security-sensitive changes.

### Strategy C: Tournament (competitive)

```
Coder-A → PR-A → Tester → score-A ↘
Coder-B → PR-B → Tester → score-B → highest score wins → Integrator
Coder-C → PR-C → Tester → score-C ↗
```

Multiple Coders compete. Each solution is scored by:
- Test pass rate (40%)
- Code quality score from Reviewer (30%)
- Performance benchmark (15%)
- Code size / simplicity (15%)

The highest-scoring solution is merged. Others are discarded.

**When to use:** Performance optimization, algorithm challenges, proof-of-concept implementations.

### Strategy D: Pipeline (sequential refinement)

```
Coder → Draft → Reviewer → Feedback → Coder → Revise → Reviewer → Approve → Tester → CI → Integrator
```

Single Coder iterates with Reviewer until approved. Strict quality gate — no PR is merged until the Reviewer explicitly approves. Watchdog monitors for stagnation (same issue raised 3+ times = escalate).

**When to use:** Critical bug fixes, security patches, production hotfixes.

### Strategy E: Mixture of Experts

```
Task → Router → [Architect | Coder | Tester | Reviewer | DevOps]
                   ↑ each expert handles their specialty ↓
                 Oracle (answers codebase questions for all)
                 Watchdog (monitors all)
```

A Router agent reads the task and dispatches to the appropriate specialist. The Oracle answers codebase questions for any agent. The Watchdog monitors all.

**When to use:** Open-ended tasks where the work type isn't known upfront.

---

## 3. Communication Patterns

### Cooperative Communication Protocol

Agents don't just send task assignments — they engage in structured cooperative communication:

#### Question-Answer Protocol
```json
// Coder → Oracle: codebase question
{"type":"question","from":"coder","to":"oracle","question":"Where is the token validation logic?","context":{"task_id":"task_01HXYZ"}}

// Oracle → Coder: answer
{"type":"answer","from":"oracle","to":"coder","answer":"Token validation is in src/auth.rs:validate_token() at line 42. It calls jwt::decode() and checks the claims. Expiry is NOT currently checked.","references":["src/auth.rs:42","src/auth.rs:87"]}
```

#### Progress Report Protocol
```json
// Coder → channel: progress update
{"type":"progress","from":"coder","task_id":"task_01HXYZ","status":"implementing","files_changed":["src/auth.rs"],"tests_written":2,"tests_passing":2,"blockers":[]}
```

#### Help Request Protocol
```json
// Coder → channel: help needed
{"type":"help_request","from":"coder","task_id":"task_01HXYZ","question":"The test for token expiry is flaky — it depends on system clock. Should I use mock time?","context":{"test_file":"tests/auth_test.rs","line":42}}

// Planner → Coder: guidance
{"type":"help_response","from":"planner","to":"coder","task_id":"task_01HXYZ","answer":"Yes, use mock_time crate. Add it to Cargo.toml dev-dependencies. Mock chrono::Utc::now() in the test."}
```

#### Disagreement Protocol
```json
// Coder → Facilitator: disagrees with review
{"type":"disagreement","from":"coder","to":"facilitator","task_id":"task_01HXYZ","issue":"Reviewer says to use Result<> but I think Option<> is cleaner here. The function never errors — it either finds a token or doesn't.","context":{"file":"src/auth.rs","line":42,"reviewer_comment":"Use Result<Claims, AuthError> instead of Option<Claims>"}}

// Facilitator → both: decision
{"type":"facilitation_decision","from":"facilitator","to":"channel","task_id":"task_01HXYZ","decision":"Use Result<Claims, AuthError>. Even if the function only returns one error type today, using Result allows adding error variants later without breaking the API. This is the project convention (see src/db/mod.rs for precedent).","reasoning":"Consistency with existing codebase patterns outweighs personal preference."}
```

---

## 4. Reflexion Loops

After each task completion (or failure), the agent performs a structured self-reflection:

```json
POST /agent/:machine_id/instruct
{
  "instruction": "Reflect on your work. Answer: 1) What went well? 2) What went wrong? 3) What would you do differently? 4) What did you learn? Submit as task result.",
  "mode": "control",
  "context": {
    "type": "reflexion",
    "task_id": "task_01HXYZ",
    "task_status": "completed",
    "exit_code": 0,
    "duration_ms": 12340
  }
}
```

The reflexion output is stored in `task_outputs` as key `reflexion` and is available to future tasks. The Planner can query past reflexions to avoid repeating mistakes.

**Reference:** Reflexion: Language Agents with Verbal Reinforcement Learning (Shinn et al., 2023)

---

## 5. Constitutional Principles

All agents operate under these constitutional principles (injected as system prompt preamble):

1. **Correctness over speed.** A slow correct solution is better than a fast broken one.
2. **Honesty about uncertainty.** If you're not sure, say so. Don't fabricate APIs or functions.
3. **No workarounds.** Don't suppress warnings, skip tests, or add `#[allow(...)]` to make code compile. Fix the root cause.
4. **Minimal changes.** Change only what's needed. Don't refactor unrelated code in the same PR.
5. **Test what you change.** Every code change must have corresponding tests.
6. **Fail loud.** If something is wrong, raise an error. Don't silently return defaults.
7. **Document public APIs.** Every public function must have a doc comment.
8. **Respect the codebase.** Match existing conventions, style, and patterns.
9. **No secrets in code.** Use environment variables. Never hardcode tokens, passwords, or keys.
10. **Escalate when stuck.** After 3 failed attempts, ask for help. Don't spin indefinitely.

**Reference:** Constitutional AI: Harmlessness from AI Feedback (Bai et al., 2022)

---

## 6. Re-Planning Protocol

When a task fails and retries are exhausted, the Planner re-plans:

1. **Analyze failure:** Read the failed task's result, audit log, and reflexion
2. **Determine cause:** Was it a bad plan, insufficient context, wrong approach, or external dependency?
3. **Adjust plan:** Modify the DAG — add steps, change instructions, increase TTL, change agent role
4. **Restart:** Create a new workflow run with the modified DAG

```json
// Planner → channel: re-planning
{
  "type": "replan",
  "from": "planner",
  "original_task_id": "task_01HXYZ",
  "reason": "Coder agent failed to implement JWT expiry check after 3 retries. Reflexion indicates the agent didn't understand the jwt crate API. New plan includes a research step.",
  "new_workflow": {
    "steps": [
      {
        "id": "research",
        "instruction": "Read the jwt crate documentation. Understand how to decode and validate JWT tokens. Specifically find how to check expiry (exp claim).",
        "image": "stronghold/rust-nightly",
        "ttl_secs": 600
      },
      {
        "id": "implement",
        "instruction": "Using the research from the previous step, implement JWT expiry checking in src/auth.rs:validate_token().",
        "depends_on": ["research"],
        "image": "stronghold/rust-nightly",
        "ttl_secs": 3600
      }
    ]
  }
}
```

**Reference:** Plan-and-Solve Prompting (Wang et al., 2023)

---

## 7. Advanced Workflow Templates

### Security Audit Flow

```json
{
  "name": "security-audit",
  "dag": {
    "steps": [
      {
        "id": "scan",
        "task": {"instruction": "Run cargo audit, cargo deny, and trivy on the codebase. Report all findings.", "image": "stronghold/rust-nightly", "ttl_secs": 600},
        "depends_on": []
      },
      {
        "id": "analyze",
        "task": {"instruction": "Analyze each finding. Determine if it's a real vulnerability or false positive. Rate severity.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["scan"]
      },
      {
        "id": "fix-critical",
        "task": {"instruction": "Fix all critical and high severity findings. Create separate PRs for each fix.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["analyze"],
        "condition": "analyze.result.critical_count > 0"
      },
      {
        "id": "review-fixes",
        "task": {"instruction": "Review all security fix PRs. Verify the fix doesn't break functionality.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["fix-critical"]
      },
      {
        "id": "re-scan",
        "task": {"instruction": "Re-run all security scans. Verify all critical findings are resolved.", "image": "stronghold/rust-nightly", "ttl_secs": 600},
        "depends_on": ["review-fixes"]
      }
    ]
  }
}
```

### Performance Regression Investigation

```json
{
  "name": "perf-regression",
  "dag": {
    "steps": [
      {
        "id": "benchmark",
        "task": {"instruction": "Run the benchmark suite. Compare results with the last known good baseline. Identify regressions.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": []
      },
      {
        "id": "bisect",
        "task": {"instruction": "Git bisect to find the commit that introduced the regression. Use the benchmark as the test.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["benchmark"],
        "condition": "benchmark.result.regressions > 0"
      },
      {
        "id": "analyze",
        "task": {"instruction": "Analyze the offending commit. Identify the root cause of the performance regression.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["bisect"]
      },
      {
        "id": "fix",
        "task": {"instruction": "Fix the performance regression. Optimize the offending code without changing the API.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["analyze"]
      },
      {
        "id": "verify",
        "task": {"instruction": "Re-run benchmarks. Verify the regression is fixed and no new regressions introduced.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["fix"]
      }
    ]
  }
}
```

### Documentation Sprint

```json
{
  "name": "doc-sprint",
  "dag": {
    "steps": [
      {
        "id": "audit-docs",
        "task": {"instruction": "Audit all documentation. Find missing doc comments, outdated README sections, broken links.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": []
      },
      {
        "id": "fix-doc-comments",
        "task": {"instruction": "Add missing doc comments to all public functions. Follow existing style conventions.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["audit-docs"]
      },
      {
        "id": "update-readme",
        "task": {"instruction": "Update README.md with current features, installation instructions, and usage examples.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["audit-docs"]
      },
      {
        "id": "update-changelog",
        "task": {"instruction": "Generate changelog entries from git log since last release. Categorize by type (feat, fix, refactor, docs).", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["audit-docs"]
      },
      {
        "id": "verify-links",
        "task": {"instruction": "Check all documentation links. Fix broken ones. Verify code examples compile.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["fix-doc-comments", "update-readme", "update-changelog"]
      }
    ]
  }
}
```

### Debate-Based Bug Fix

```json
{
  "name": "debate-bugfix",
  "dag": {
    "steps": [
      {
        "id": "analyze",
        "task": {"instruction": "Analyze the bug. Describe root cause, affected components, and possible approaches.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": []
      },
      {
        "id": "solution-a",
        "task": {"instruction": "Implement fix using approach A (minimal change, targeted fix). Create PR.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["analyze"]
      },
      {
        "id": "solution-b",
        "task": {"instruction": "Implement fix using approach B (broader refactor, addresses root cause). Create PR.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["analyze"]
      },
      {
        "id": "test-a",
        "task": {"instruction": "Run full test suite on solution A. Report results.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["solution-a"]
      },
      {
        "id": "test-b",
        "task": {"instruction": "Run full test suite on solution B. Report results.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["solution-b"]
      },
      {
        "id": "judge",
        "task": {"instruction": "Compare both solutions. Score each on: correctness, test coverage, code quality, minimal change, future-proofing. Pick the winner.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["test-a", "test-b"]
      },
      {
        "id": "merge",
        "task": {"instruction": "Merge the winning solution. Close the losing PR with explanation.", "image": "stronghold/rust-nightly", "ttl_secs": 600},
        "depends_on": ["judge"]
      }
    ]
  }
}
```

### Continuous Improvement (Reflexion Cycle)

```json
{
  "name": "continuous-improvement",
  "dag": {
    "steps": [
      {
        "id": "analyze-failures",
        "task": {"instruction": "Review the last 20 failed tasks. Identify patterns: common failure modes, recurring issues, missing context.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": []
      },
      {
        "id": "improve-prompts",
        "task": {"instruction": "Based on the failure analysis, propose improvements to agent system prompts and workflow templates.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["analyze-failures"]
      },
      {
        "id": "update-templates",
        "task": {"instruction": "Update workflow DAG templates and agent prompts based on the proposed improvements. Create PR.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["improve-prompts"]
      },
      {
        "id": "review",
        "task": {"instruction": "Review the prompt/template improvements. Verify they address the identified failure patterns.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["update-templates"]
      }
    ]
  }
}
```

### Hotfix Pipeline (emergency)

```json
{
  "name": "hotfix",
  "dag": {
    "steps": [
      {
        "id": "fix",
        "task": {"instruction": "Fix the critical issue immediately. Minimal changes only. Write a regression test. Push and create PR.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": []
      },
      {
        "id": "review-merge",
        "task": {"instruction": "Expedited review. Focus on: does the fix work? Does it break anything? Merge immediately if safe.", "image": "stronghold/rust-nightly", "ttl_secs": 900},
        "depends_on": ["fix"]
      },
      {
        "id": "deploy",
        "task": {"instruction": "Deploy the hotfix to production. Verify the fix is live. Monitor for 5 minutes.", "image": "stronghold/fullstack", "ttl_secs": 900},
        "depends_on": ["review-merge"]
      }
    ]
  }
}
```

### Dependency Upgrade Flow

```json
{
  "name": "dep-upgrade",
  "dag": {
    "steps": [
      {
        "id": "check-outdated",
        "task": {"instruction": "Run cargo outdated. List all dependencies with available updates. Categorize by semver (major/minor/patch).", "image": "stronghold/rust-nightly", "ttl_secs": 600},
        "depends_on": []
      },
      {
        "id": "upgrade-patch",
        "task": {"instruction": "Upgrade all patch-version dependencies. Run tests. If any fail, roll back and report.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["check-outdated"]
      },
      {
        "id": "upgrade-minor",
        "task": {"instruction": "Upgrade all minor-version dependencies. Run tests. Fix any breaking changes.", "image": "stronghold/rust-nightly", "ttl_secs": 3600},
        "depends_on": ["upgrade-patch"]
      },
      {
        "id": "test-full",
        "task": {"instruction": "Run full test suite with all upgrades. Report any failures.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["upgrade-minor"]
      },
      {
        "id": "review-changelog",
        "task": {"instruction": "Review changelogs of all upgraded dependencies. Flag any security-relevant changes.", "image": "stronghold/rust-nightly", "ttl_secs": 1200},
        "depends_on": ["test-full"]
      },
      {
        "id": "pr",
        "task": {"instruction": "Create PR with all upgrades. Include test results and changelog summary.", "image": "stronghold/rust-nightly", "ttl_secs": 600},
        "depends_on": ["review-changelog"]
      }
    ]
  }
}
```

### Onboarding Flow (new codebase analysis)

```json
{
  "name": "onboarding",
  "dag": {
    "steps": [
      {
        "id": "structure",
        "task": {"instruction": "Analyze the codebase structure. List all modules, their purposes, and dependencies. Create a mental model.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": []
      },
      {
        "id": "entry-points",
        "task": {"instruction": "Identify entry points (main, lib, tests). Trace the execution flow from entry to first major operation.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["structure"]
      },
      {
        "id": "key-apis",
        "task": {"instruction": "Document the key public APIs. For each: signature, purpose, callers, tests.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["structure"]
      },
      {
        "id": "patterns",
        "task": {"instruction": "Identify architectural patterns used (error handling, state management, testing strategy). Document conventions.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["structure"]
      },
      {
        "id": "synthesize",
        "task": {"instruction": "Synthesize all findings into a comprehensive CODEBASE_GUIDE.md. Include: architecture diagram (ASCII), module descriptions, key APIs, conventions, gotchas.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": ["entry-points", "key-apis", "patterns"]
      }
    ]
  }
}
```

---

## 8. Watchdog Implementation Specification

The Watchdog runs as a separate agent (separate pod, separate session) that monitors other agents via the audit stream and message bus.

### Watchdog System Prompt (abridged)

```
You are a Watchdog Agent. You monitor other agents for:
1. Task dedication — are they working on the assigned task?
2. Progress — are they making measurable progress?
3. Workaround avoidance — are they taking shortcuts?
4. Simplification avoidance — are they reducing scope without approval?

Every 60 seconds:
- Query the audit stream for the monitored agent's recent activity
- Compute dedication score (0.0-1.0)
- Check for workaround patterns
- If dedication < 0.3 for 3 checks: issue ultimatum
- If workaround detected: issue warning
- Post watchdog_report on the message bus

You do NOT write code, review PRs, or run tests.
You DO monitor, report, and issue ultimata.
```

### Watchdog Report Format

```json
{
  "type": "watchdog_report",
  "watched_machine": "mach_01HXYZ",
  "watched_task_id": "task_01HXYZ",
  "dedication_score": 0.72,
  "progress_indicators": {
    "files_changed": 3,
    "tests_run": 5,
    "commits": 1,
    "last_activity_seconds_ago": 15
  },
  "workaround_warnings": [],
  "ultimatum_level": 0,
  "assessment": "Agent is on-task and making steady progress."
}
```

### Watchdog Workflow Integration

```json
{
  "name": "watched-cicd",
  "dag": {
    "steps": [
      {
        "id": "plan",
        "task": {"instruction": "...", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": []
      },
      {
        "id": "watchdog-plan",
        "task": {"instruction": "Monitor the planner agent. Check dedication every 60s. Issue ultimata if off-task.", "image": "stronghold/rust-nightly", "ttl_secs": 1800},
        "depends_on": [],
        "parallel_with": "plan"
      },
      {
        "id": "implement",
        "task": {"instruction": "...", "image": "stronghold/rust-nightly", "ttl_secs": 7200},
        "depends_on": ["plan"]
      },
      {
        "id": "watchdog-implement",
        "task": {"instruction": "Monitor the coder agent. Check dedication, detect workarounds, enforce quality standards.", "image": "stronghold/rust-nightly", "ttl_secs": 7200},
        "depends_on": ["plan"],
        "parallel_with": "implement"
      }
    ]
  }
}
```

Note: `parallel_with` is a new DAG field that indicates a step runs concurrently with another step (as opposed to `depends_on` which creates a sequential dependency). The workflow engine should support both.

---

## References

- **ReAct:** Yao et al., "ReAct: Synergizing Reasoning and Acting in Language Models," ICLR 2023
- **Reflexion:** Shinn et al., "Reflexion: Language Agents with Verbal Reinforcement Learning," NeurIPS 2023
- **MetaGPT:** Hong et al., "MetaGPT: Meta Programming for Multi-Agent Collaborative Framework," ICLR 2024
- **ChatDev:** Qian et al., "Communicative Agents for Software Development," ACL 2024
- **Multi-agent Debate:** Du et al., "Improving Factuality and Reasoning in Language Models through Multiagent Debate," arXiv 2023
- **Self-Refine:** Madaan et al., "Self-Refine: Iterative Refinement with Self-Feedback," NeurIPS 2023
- **Plan-and-Solve:** Wang et al., "Plan-and-Solve Prompting: Improving Zero-Shot Chain-of-Thought Reasoning by Large Language Models," ACL 2023
- **Constitutional AI:** Bai et al., "Constitutional AI: Harmlessness from AI Feedback," arXiv 2022
- **CodeAct:** Wang et al., "Executable Code Actions Elicit Better LLM Agents," ICML 2024
- **GPTLens:** Sun et al., "GPTLens: A Dual-Agent Framework for Smart Contract Vulnerability Detection," arXiv 2024
- **Mixture of Experts:** Jacobs et al., "Adaptive Mixtures of Local Experts," Neural Computation 1991
- **Tree of Thoughts:** Yao et al., "Tree of Thoughts: Deliberate Problem Solving with Large Language Models," NeurIPS 2023
