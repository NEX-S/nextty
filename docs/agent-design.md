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

Tools within one conversation execute in order. Network activity, terminal/history waits and presentation work must not block frontend input. Record known tool results before fallible UI notifications. Keep input authorization separate from the activity observation used for waits.

Waits are bounded observations. Return a reason and current state so the agent can continue or respond to an interactive prompt; do not wake the model for every output line. Stopping the agent cancels its own requests and waits, not programs already running in the terminal.

Use plain data and existing modules. Avoid compatibility aliases, tool registries, automatic application modes, new services or parallel writers to the same PTY. Validate changes on real shell/PTY paths and representative debugger screens. Measure data correctness and local overhead separately from real-model task quality.
