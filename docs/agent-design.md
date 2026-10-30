# Agent design

Nextty's agent primarily assists work in the user's shell, with occasional use of interactive programs such as GDB and pwndbg. Keep one small agent loop and the three tools `shell`, `input` and `read`.

## Responsibility boundary

Nextty provides reliable terminal facts and convenient access to them. The agent chooses what to inspect, interprets the evidence and decides what to do next.

Nextty owns:

- Correct PTY targeting, existing shell state, input conflict checks and reads that preserve the user's viewport and selection.
- Command boundaries and exit status when shell integration supplies them; live screen state and retained history otherwise.
- Bounded reads, literal search, paging, resumable positions, cancellation and explicit reporting of missing content.
- Faithful text extraction, physical row coordinates, wrapping and cursor visibility. Keep presentation cleanup conservative and independent of application names.

The agent owns:

- Choosing command output, history or the live screen, and selecting the relevant range or query.
- Deciding whether to continue reading, wait, interact or broaden the evidence.
- Application commands, debugger configuration, task relevance, diagnosis and verification of the user's objective.

Do not infer task relevance from keywords, recognize applications to select a hidden policy, summarize evidence before the agent sees it, or treat a quiet interval as application readiness. Report unknown state as unknown. Queued input, screen activity and an elapsed wait are not proof of completion.

## Observation model

Use two data shapes, without application-specific adapters:

- **Text:** a bounded part of one command's output or the main terminal's retained text, with a continuation cursor, `has_more` and `incomplete`. Include command execution state only for command reads.
- **Screen:** the live application's rows, original coordinates, size, wrapping and cursor visibility, with the current input/foreground state needed to operate it. Full session identity belongs in the initial context, not every read.

Prefer command output for shell tasks. Ordinary debugger commands can also be inspected as newly appended text. Use a screen snapshot for interfaces that overwrite their layout. An executable name or alternate-screen flag alone does not determine which observation the task needs.

## Reading and continuation

- Keep a usable `cursor` at the end of the available text. `has_more` means this snapshot still contains unread text; it does not mean the program is running. `incomplete` means retained evidence is missing.
- A cursor belongs to one pane, source and command. Its offset is a UTF-8 byte position in that source's rendered physical row, not a terminal column. Resize/reflow or clearing can invalidate it explicitly.
- Preserve appends to an unfinished final row, later hard newlines and soft wraps. Do not advance over unused blank screen rows.
- History is retained rendered terminal text, not an immutable recording of PTY bytes. Use screen observations for overwritten content; do not promise that a text cursor detects edits to previously read text.
- A literal query selects a bounded matching excerpt with neighboring context, rather than all text after the first match. Text search can return one contiguous excerpt per read; the cursor continues after the inspected excerpt. A query is range search, not a persistent streaming matcher across requests.
- Preserve literal matches across physical wrapping and archive boundaries within a read. Keep UTF-8 boundaries and report clipping or missing evidence honestly.
- The agent controls whether to retrieve more context. Defaults must be bounded and predictable; do not automatically return only lines classified as errors or important.

## Screen presentation

Extract the current screen from terminal cells, independently of the user's scroll position. Preserve substantive text, titles, addresses, instructions, pointer chains, change markers and layout. Omit only clearly defined empty or decorative rows; do not grow a catalog of application-specific cleanup rules.

A query focuses on matching rows and neighboring context while retaining the visible input row. An unfiltered screen read remains available. Keep snapshots stateless; add screen differences only if measured benefit justifies baseline, deletion, resize and rewind handling.

## Harness and implementation

Tools within one conversation execute in order. Consecutive queued full conversation snapshots may be replaced by the latest snapshot before presentation; preserve intervening view and lifecycle events. Network activity, terminal/history waits and presentation work must not block frontend input. Record known tool results before fallible UI notifications. Keep input authorization separate from the activity observation used for waits.

Waits are bounded observations. `read.wait` is an integer number of seconds, from 0 through 1800; omission or 0 reads immediately. Return a reason and current state so the agent can continue or respond to an interactive prompt; do not wake the model for every output line. Stopping the agent cancels its own requests and waits, not programs already running in the terminal. Preserve results already received when cancellation arrives, then stop before another action or model request.

Use plain data and existing modules. Avoid compatibility aliases, tool registries, automatic application modes, new services or parallel writers to the same PTY. Validate changes on real shell/PTY paths and representative debugger screens. Measure data correctness and local overhead separately from real-model task quality.

## Conditional monitoring

Keep conditional monitoring in the existing conversation and PTY. The agent chooses a command that finishes when the requested condition holds, then uses a command read with an appropriate wait, for example `{"source":"command","command":123,"wait":300}`. The owner sleeps until command completion or the observation deadline while the frontend and other conversations continue working. One pending read needs no additional nextty watcher process, task registry or model polling loop.

The condition belongs to the command or to the agent's interpretation of its output. A build can report its own exit status; a log-watching command can exit on a selected event. The agent is responsible for bounding and cleaning up processes it explicitly starts. A read timeout only ends observation. Screen/history waits still return on terminal activity, and a query still selects an excerpt; it is not a persistent streaming predicate. Choose short waits when an interactive prompt may need attention. A successful condition command is evidence about that condition, not proof of the user's whole objective.

An outstanding read occupies that conversation's tool loop. Stopping the conversation cancels the wait promptly; an already running terminal command remains available for explicit interaction. Continued operation after a frontend detaches follows the existing owner lifetime; restart recovery is a separate product requirement.

## Context, goals and orchestration

Keep the model/tool loop distinct from an outer automation loop. The harness executes requested tools, appends their results and asks the model again; an ordinary final response ends the turn. Loop engineering adds a trigger, objective, verification and stopping policy around repeated turns. Graph engineering makes dependencies and verification-gated transitions explicit for a known workflow. Neither requires a graph execution framework for an interactive terminal assistant.

Current choices:

- **Retrieval:** keep command IDs, paths, literal queries and cursors as references and fetch evidence when needed. Shell search and terminal history are the first retrieval mechanisms. Add semantic RAG only for an identified corpus where text search demonstrably misses useful evidence; account for index freshness, provenance and retrieval cost.
- **Prompt cost:** describe shared read semantics once rather than repeating them in every schema branch. Keep tool definitions and earlier messages stable between requests. Execution requests opt into automatic prefix caching with `cache_control: {"type":"ephemeral"}`; gateways must support this Messages API field. One-off summary requests do not request cache writes. Cache hits reduce repeated processing, not context length, and must be measured from provider usage rather than assumed.
- **Context:** keep recent complete tool exchanges and the original active request while summarizing older work. Summaries are fallible historical context, not current terminal facts or a complete session recording. The byte budget bounds local serialization; it is not a model token-window measurement.
- **Monitoring:** wait on a concrete execution event without repeated model calls. Keep semantic conditions and decisions in the agent; keep deterministic execution state in nextty.
- **Subagents:** introduce them only for independently useful work with isolated context and measurable benefit. A future read-only analysis of a frozen log could qualify. All actions on a shared PTY must still pass through one ordered writer and current input authorization.
- **Goals:** add a persistent goal only when work must continue beyond an ordinary turn. Its contract must include user intent, observable completion evidence, progress, cancellation and finite resource limits. An evaluator's verdict is an assessment, not machine proof. Repeating a prompt after every final response does not supply verification.

Use the existing owner process for workspace coordination, one lazily started worker per conversation for model I/O and context, and existing history workers for searches. Add concurrency where independent work can proceed; avoid adding workers to a dependency chain. Measure model-request count and evidence correctness as well as local CPU and input latency.

## Evidence behind these choices

Reviewed on 2026-10-30. These sources inform the decisions above; their product features and benchmark scores are not requirements for nextty.

- OpenAI, [Unrolling the Codex agent loop](https://openai.com/index/unrolling-the-codex-agent-loop/) (2026-01-23): the small model/tool cycle, prompt structure and context management. [Harness engineering](https://openai.com/index/harness-engineering/) (2026-02-11) concerns engineering an observable, verifiable development environment at a much larger product scale.
- Anthropic, [Effective context engineering](https://www.anthropic.com/research/effective-context-engineering-for-ai-agents) (2025-09-29): lightweight references and just-in-time retrieval. [Scaling Managed Agents](https://www.anthropic.com/research/managed-agents) (2026-04-08): separate session records, working context and execution; remove scaffolding that newer models no longer need.
- Claude Code's [Monitor reference](https://code.claude.com/docs/en/tools-reference#monitor-tool) and [scheduled tasks](https://code.claude.com/docs/en/scheduled-tasks): event-producing scripts can avoid repeated prompt polling. Its separate command execution and task lifecycle differ from nextty's shared PTY.
- [pi source](https://github.com/earendil-works/pi/tree/f09d01b85e0384647fcab6779d35ab9dabe04be3) and [Hermes source](https://github.com/NousResearch/hermes-agent/tree/088f657ecf77674c7b8e2ededf4e3576d82e042c) (2026-10-29 snapshots): useful examples of small loops, explicit cancellation and on-demand history. Their subprocess ownership, concurrency defaults and broader assistant features must be evaluated separately; current source can differ from older descriptions.
- Addy Osmani, [Loop Engineering](https://addyosmani.com/blog/loop-engineering/) (2026-06-07), and [Loop Engineering: Building Blocks, Adoption, and Impact](https://arxiv.org/html/2608.21884v3) (2026-10-22 revision): the outer trigger/verify/continue layer. The latter reports that systematic evidence for broad benefit claims is still missing.
- [Towards Agentic Cloud Engineering](https://arxiv.org/html/2609.00050v2) (2026-10-16 revision) uses graph-controlled progression for cloud deployment with predefined verification predicates. That controlled setting does not establish a need for graphs in general shell assistance.
- [Towards a Science of Scaling Agent Systems](https://arxiv.org/html/2512.08296v3) (2026-04-08 revision) and [Demystifying Harness Effects](https://arxiv.org/html/2609.32459v1) (2026-09-26) show task- and model-dependent orchestration effects. [An Empirical Study of Harness Design](https://arxiv.org/html/2609.20804v1) (2026-09-17) similarly finds that adding recall machinery does not consistently improve task completion. These experiments motivate local evaluation rather than universal agent-count or component rules.

Additional review on 2026-10-30:

- OpenAI's [Codex platform overview](https://developers.openai.com/blog/codex-as-a-platform) and [iterative repair example](https://developers.openai.com/cookbook/examples/codex/build_iterative_repair_loops_with_codex) separate reusable harness execution from application-owned context and validation. For nextty, verify the requested terminal outcome; do not add an automatic continuation after every final response.
- Anthropic's [tool definitions](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools) and [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching) support detailed, compact tool contracts and a single automatic cache breakpoint. Retain tool definitions when summarizing histories containing tool-use blocks, with tool choice disabled; a summary request is not an execution turn.
- [The Complexity Trap](https://arxiv.org/html/2508.21433v3) (2025-10-27) finds simple old-observation masking competitive with model summaries in its coding-task experiments. [Demystifying Harness Effects](https://arxiv.org/html/2609.32459v1) also reports compression reducing tokens while lowering task scores. These results make masking a candidate for paired task evaluation, not a reason to silently discard terminal evidence or replace the current summary policy without measuring correctness.
- [AutoCompact](https://arxiv.org/html/2610.02163v1) (2026-10-01) trains compaction decisions and continuation behavior together. Its reported benefit is not evidence that adding an untrained compaction tool to an arbitrary model will reproduce the result.

Evaluate context changes using the same model, tasks, budgets and initial terminal state. Include shell completion, long-output diagnosis, an interactive prompt, overwritten debugger screens and a task spanning compaction. Measure task completion and unsupported success claims alongside total provider input/output tokens, cache reads/writes, model requests and elapsed time. The local SSE benchmark establishes execution correctness, request bytes and local overhead; it cannot establish real-model task quality, token savings or cache hits.

Prefer official descriptions, fixed source revisions and original experiments. Treat practitioner reports as experience and third-party leak analyses as unverified implementation claims; neither substitutes for evidence from the current code and real terminal behavior.
