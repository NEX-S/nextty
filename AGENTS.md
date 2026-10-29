# Agent and terminal observation changes

For changes to the agent, harness, tools or terminal observation, read [docs/agent-design.md](docs/agent-design.md) first.

Keep the responsibility boundary explicit: nextty supplies reliable terminal facts and bounded access; the agent chooses, interprets and acts. Prefer shell/text workflows and keep interactive screen support generic.

Update the design document when intentionally changing this contract. Do not duplicate its rules in application-specific adapters or extra configuration modes.
