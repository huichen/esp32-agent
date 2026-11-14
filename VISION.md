# Vision

`esp32-agent` exists so that a microcontroller is an agent host rather than a thing an agent talks about.
It serves the person with a board on the bench: flash it, join it to WiFi, and its buses become tools a model can call, with no service in the middle.
It owns exactly one thing: the description of what this board can do, read by every caller - the on-board agent, the MCP endpoint and the console.

## One tool surface, three readers

The agent on the board and the agent that reaches it over HTTP see the same tools, from the same predicate, because a board that disagrees with itself cannot be trusted about what it did.
One predicate answers both questions - is this tool offered, and why not - and every view reads it: the model's tool array, `tools/list`, `/api/tools` and `/api/status`.
An absent device never shows the model a tool it would then call and fail on.
A tool name no model endpoint would accept is refused when the manifest is declared, not when a turn fails.
Two callers with one description is the product; a second, richer surface built for the on-board agent alone is a different project.

## The manifest is a language, and it is meant to grow

A GPIO switch, a PWM channel, an ADC reading or one I2C register is a manifest, applied live, in the same turn.
The manifest is a language rather than a form: expressions, conversion, thresholds and composition are the direction, and a more sophisticated one is the goal, not a tolerated cost.
Every case the manifest can express is a case that no longer costs a reflash, so moving behaviour out of a driver and into the manifest is a win.
A driver exists for what no declaration can express - timing, calibration, a protocol, a state machine over several transfers - and that boundary moves outward as the language grows.
The language stays bounded: an expression is evaluated under the turn's deadline and cannot hang the board, so capability arrives with a budget rather than instead of one.
A saved manifest on a board somewhere is a contract, so a field the firmware accepts stays accepted.
A new capability that can move something physical arrives with its `safety` story, and an unrestricted path is not the default.

## The board is the server, on a network its owner trusts

The console, the certificate, the portal PIN and the MCP token live on the board, so the bench works when the cloud does not.
The board issues its own TLS certificate and publishes the fingerprint, so a client can compare rather than trust.
Port 80 stays open by default, and closing it is a setting with a stated cost: a device that answers on no port is not more secure, only unreachable.
The console is a console for an internal network: a relay, a tunnel or a hosted panel that carries it off that network is not the convenience this project wants, because a board is then reachable by whoever reaches the relay.
Credentials stay on the board - WiFi reaches NVS through `provision`, `api_key` is write-only and reads report `api_key_set` - and none of them enter the repository.

## Every refusal is an answer

A refused call says what was refused and why, in a sentence, because a missing name teaches the caller nothing.
`reboot` is refused with a sentence rather than dropped from the list, and the parts of `sed` and `awk` this board does not implement are refused by name instead of approximated.
Every tool call runs under one configured deadline, and that budget reaches the device rather than staying in a client.
Limits are checked before a buffer is allocated or a bus is touched, never after the copy has already happened.
Every structure that grows - transcript, archive index, workspace, attachments - carries a stated ceiling that the code enforces.
No tool call and no driver probe can take the board down or lock it away from its owner, and nothing on the board restarts or resets it on a caller's behalf.

## Concurrency is the goal, not the compromise

A slow tool call must not queue an unrelated one, and a model turn in flight must not make `/api/status` wait.
The shared application lock is where this board started, not where it stays: the work ahead is separate device and workspace ownership with resource locks, so independent calls run in parallel.
Simplicity is not a trade this project makes for capability; the board is meant to do more at once, not less.
The model call already runs outside the lock, status, health and cancellation have their own plane, and the journal streams while a turn runs.
Queue admission is not storage completion: a reply that says `queued` does not claim `saved`, and a save is acknowledged only after publication.
Concurrency is reported so it can be reasoned about - which core a task runs on, how much stack headroom it holds - not so it can be admired.

## Evidence carries its source

Host tests and board measurements are different claims, and neither stands in for the other.
A change is verified when the command that verifies it is named: the test count, the gate sequence, the partition table the flasher was given.
A gate only a maintainer remembers is not a gate, so the five commands run in CI on every change, and a contribution that passed only on its author's laptop has not passed.
Contributions arrive from outside this repository, so the path from an issue to a merged driver is documented and templated, and a change is judged against this file rather than against who asked for it.
Behaviour with a build or a measurement behind it goes into `docs/`, everything else goes into `ROADMAP.md` as unverified, and a firmware change not built here says so in the commit and the handoff instead of passing as verified.
Historical bench numbers are dated, tied to the image that produced them, and never restated as a current limit.

## Scope

It is not a cloud service, a fleet manager or a hosted endpoint: it runs on one board, for one owner, on a network that owner trusts.
Nothing in the firmware coordinates boards: the board serves its own tools and its own console, and an outside agent may hold several boards at once, but that is the agent's business rather than a reason to add a board list, a fleet view or cross-board scheduling here.
It is not an OTA channel, and that is a risk decision rather than a missing feature: a firmware update is the one operation that can leave a board unreachable, so every flash happens over the cable with the partition table passed explicitly.
It is not a Linux machine with a shell bolted on: the shell and the script interpreter are subsets, and what they do not implement they refuse by name.
It is not a place for secrets: no credential, token or WiFi password enters the code, the examples, the tests or the image.
A name nothing calls is deleted - a REST route, an alias, a seeded skill, a changelog - because a surface kept for sentiment is a surface every later change has to defend.

A change aligns when it makes one description serve another caller, moves a case from a driver into the manifest, lets independent work run in parallel, answers something the board could not answer before, makes a refusal clearer or a limit enforceable, puts the gate in front of a contribution, or keeps the console on the network its owner trusts.
A change should be resisted when it adds a second source of truth about the tools, needs a reflash for something a manifest could describe, serializes independent work behind one lock, carries the console off the trusted network, makes a firmware update reachable without the cable, spends a budget it never bounded, claims a verification it never ran, or keeps a surface nothing calls.
