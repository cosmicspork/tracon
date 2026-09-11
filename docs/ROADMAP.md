# Roadmap

## Direction

- Personal agent workspace; one node is a complete installation. Mesh is optional.
- Enforce granted authority, not mandatory work items, phases, models, or review.
- Keep untrusted execution isolated and credentials outside agent-owned state.
- Prefer useful environments, clear evidence, and human intervention over more machinery.
- Record scoped decisions and supersede them explicitly; keep harnesses replaceable.

## Planned

### Grantable authority and optional workflow

Start a session in a workspace with permissions, without prescribing how work is organized.

- [ ] Offer scoped allow/ask/deny for merging, publishing, ticket transitions, and deployments.
- [ ] Bind grants to targets and revisions where appropriate; revalidate before acting.
- [ ] Execute consequential actions through the broker without exposing credentials.
- [ ] Make work items, plan/execute phases, explicit model selection, and review optional.
- [ ] Inherit sensible model defaults and record the actual model used.
- [ ] Keep structured workflows as presets; require review only when policy requires it.
- [ ] Record candidate, authority, evidence, and outcome even for automatically authorized actions.
- [ ] Expose policy and grant management in the interface, preserving signing-key and trust-root boundaries.
- [ ] Reconcile README, architecture, design, and shipped policy as behavior changes.

### Managed workspaces and separate publication

Work on independent repositories without host bind mounts.

- [ ] Remove bind mounts entirely; persist sessions in runtime-owned storage.
- [ ] Copy explicitly selected files/folders rather than granting ongoing host access.
- [ ] Import selected uncommitted work without activating host Git configuration, hooks, or credentials.
- [ ] Reject symlink escapes; never silently write back to the selected source.
- [ ] Resume tracon-owned workspaces and provide explicit export/download.
- [ ] Broker private source fetches without giving the agent forge credentials.
- [ ] Transfer an immutable candidate into a separate publisher-controlled repository with trusted configuration and targets.
- [ ] Never run credential-bearing host Git in the agent-owned clone.

### Prepared execution environments

Run real project workflows on a fresh node without manual environment repair.

- [ ] Separate dependency preparation, restricted execution, and candidate verification.
- [ ] Reuse project toolchain/devcontainer conventions where safe; reject privileged settings, host mounts, sockets, and unsafe setup configuration.
- [ ] Identify approved images, dependency inputs, and isolated caches without requiring a new repo format.
- [ ] Exercise a real private repository through preparation, agent work, checks, and authorized publication.

**Dependencies:** managed workspace and publication boundaries for the complete workflow.

### Reusable check evidence

Attach evidence to immutable candidates, not mutable worktrees or review submissions.

- [ ] Separate candidates, check runs, review revisions, and decisions in storage.
- [ ] Run checks against isolated candidate snapshots with operator-controlled required checks.
- [ ] Key reuse on source revision, check definition, execution image, and relevant configuration/dependency inputs.
- [ ] Retain successes, failures, input identities, logs, execution metadata, and explicit reruns.
- [ ] Reuse unchanged code evidence for MR-title/description or Jira-prose revisions.
- [ ] Show changed inputs and reused evidence; keep prose authorization separate from code checks.
- [ ] Migrate existing `head_sha`, `checks_json`, and check-result events into the evidence trail.

**Open question:** tree-hash reuse for checks proven independent of commit history; start with commit identity.

### Review context and demonstrations

Make evidence understandable without turning tracon into an IDE.

- [ ] Present requirements, relevant surrounding code, check output, and runtime evidence beside the diff.
- [ ] Support phone review without hiding missing context or stale evidence.
- [ ] Keep authoritative execution records separate from curated demonstrations.
- [ ] Evaluate [Showboat](https://github.com/simonw/showboat) for documents combining commands, captured output, and images.

### QA deployment and browser verification

Demonstrate that the candidate works in the intended QA environment.

- [ ] Deploy an identified candidate to an explicitly authorized QA target.
- [ ] Run browser verification with scoped browsing and test-account authority.
- [ ] Link candidate → deployed build → QA target → browser run → evidence.
- [ ] Record environment identity and time; invalidate evidence when the deployment changes.
- [ ] Attach assertions, logs, screenshots, and demonstrations to the candidate.
- [ ] Keep deployment and browser permissions separate; neither grants production access or treats QA writes as harmless reads.

**Dependencies:** capable execution environments and candidate-bound evidence.

### Repository-derived prototypes

Use real application components and styles in interactive design artifacts.

- [ ] Build previews inside the execution environment and export versioned HTML/asset bundles with source revision and build metadata.
- [ ] Render through the same sandboxed artifact viewer, without host file serving or bind-mount exceptions.

**Dependencies:** a suitable project build environment; use the existing HTML bundle importer and sandboxed viewer.

### Ask the operator

Provide a real question/answer tool, not an Allow/Deny permission workaround.

- [ ] Accept free-text questions and optional choices; return structured answers to the originating call.
- [ ] Persist questions and answers through client disconnection; surface them in the queue and session.
- [ ] Allow asking for help without approval; silence is not consent and answers do not widen permissions.
- [ ] Define unanswered-question behavior separately from permission expiry.

### Notify the operator

Intentionally request an OS/PWA ping, not merely a transcript update.

- [ ] Include a title, message, and session/artifact/question link.
- [ ] Route through configured nodes/devices with rate limits and deduplication.
- [ ] Record delivery attempts without claiming the human saw a delivered notification.

### Report an issue

Lodge complaints about tracon, environments, tools, or harness integration.

- [ ] Capture expected/actual behavior, reproduction, versions, relevant errors, and attempted recovery.
- [ ] Distinguish observed evidence from the agent's diagnosis.
- [ ] Support opening an issue in tracon's repository through the broker with operator authorization.
- [ ] Make proposed issue text and attachments inspectable before publication.
- [ ] Exclude secrets and private project material; reporting permission does not authorize wholesale transcript/repository uploads.
- [ ] Keep reporting separate from declaring work blocked or pausing execution.

### Pause controls and runaway protection

Stop broken execution without requiring an invented token budget for every task.

- [ ] Provide explicit pause/stop controls that actually prevent new agent work while preserving workspace and evidence.
- [ ] Bound retries, recovery attempts, handshakes, and tool/process timeouts.
- [ ] Pause and explain repeated failures instead of automatically restarting the same loop.
- [ ] Treat repetition as a signal, not a universal measure of progress.
- [ ] Retain watchdogs for failures the agent cannot report; keep token/spending limits optional.
- [ ] Do not automatically pause on issue reports or treat notifications as questions.

### Local-first onboarding and README

Explain the job and make one-node use complete.

- [x] Lead the README with the personal workflow, authority, evidence, and interventions rather than topology.
- [ ] Present work items, phases, review, memory, remote access, and mesh as optional.
- [x] Explain the agent-built project as a demonstration of design judgment and decision-making, not hand-written coding.
- [x] Reduce setup burden and keep the hub out of required first-run steps.
- [x] Measure time to useful verified work, setup failures, human interventions/waiting, and tokens per accepted change.
- [ ] Document current limitations and reconcile superseded decisions when changes land.

### Other backlog

- [ ] Continue work on another node using explicit candidate/context transfer and a new session, without mandatory work items/phases or live-harness migration.
- [ ] Add optional hub-side rollups without making local use depend on them.
- [ ] Sign desktop releases and pin mutable build inputs for reproducibility; checksums alone are not independent publisher authentication.
- [ ] Paginate forge repository listings.

## Hardening

Findings originate from review of `2e32a59`; revalidate against the implementation revision. Planned policy flexibility must not weaken isolation.

### Reproduced paths

- [ ] Disable Git replacement/graft interpretation across capture, provenance, and publication; verify reviewed bytes match the pushed candidate.
- [ ] Authenticate enrollment's signing/encryption key binding before handing off channel keys.
- [ ] Eliminate executable agent-controlled Git metadata, including `config.worktree`. Git-side execution was reproduced; a full container escape was not exercised.
- [ ] Restrict credentialed model proxy methods/paths to granted capabilities, not arbitrary provider-account operations.
- [ ] Fix mutable-worktree checks and agent-controlled required-check overrides through candidate-bound verification.
- [ ] Reject inappropriate `Origin: null` operator requests; verify browser defenses as well as the reproduced middleware behavior.

### Recovery and boundary verification

- [ ] Recover interrupted publication honestly and idempotently, including crashes after external side effects.
- [ ] Verify private HTTPS pushes use brokered authentication rather than ambient host helpers.
- [ ] Verify desktop process identity before adoption/signaling, including stale handoff records and PID reuse.
- [ ] Exercise cancellation during checks, stalled harness startup, and resubmission racing approval; reject late completions that resurrect terminal sessions.
- [ ] Exercise delayed mesh keys, member removal/key revocation, restart recovery, and wire-version mismatches with visible refusals.
- [ ] Verify hub outages preserve local work and never silently widen authority.
- [ ] Verify unsent-text durability and spending/usage accounting.
- [ ] Exercise real Podman/Kubernetes project workflows; API fixtures and boundary probes alone are insufficient.
- [ ] Preserve a usable direct-harness recovery path and portable corpus exports; vectors remain rebuildable derived data.
- [ ] Pin harness versions and record per-session compatibility; reject unsupported protocol versions explicitly.

## Deferred

- **Third harness adapter (`opencode`):** when a real task requires it; bind HTTP to loopback and disable mDNS discovery before enabling.
- **Retrieval reranker:** when existing retrieval proves insufficient.
- **Client terminal:** only for a concrete task the existing interface cannot support.
- **`cr-sqlite`:** only if real multi-writer convergence requires it.
- **Stacked MR automation:** decide whether stacks are preferable to feature flags first.

## Out of scope

- Multi-user tenancy or a team product.
- A model/agent loop inside the node.
- Required tracon configuration files installed into project repositories.
- A full IDE, general file editor, or per-project editor configuration.
- Business-domain features such as invoicing and billing.
- Arbitrary host execution through the node API. Service/CLI installation and node restarts remain explicit desktop/CLI operations; file imports use a picker/upload flow.
