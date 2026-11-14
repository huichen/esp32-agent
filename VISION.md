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

## Adding a device is configuration until it has to be code

A GPIO switch, a PWM channel, an ADC reading or one I2C register is a manifest, applied live, in the same turn.
A driver exists only for what configuration cannot express: timing, calibration, a protocol, a state machine that runs over several transfers.
The manifest layer never knows a chip and the driver layer never knows MCP exists; the effort goes into the boundary between them.
A saved manifest on a board somewhere is a contract, so a field the firmware accepts stays accepted.
A new capability that can move something physical arrives with its `safety` story, and an unrestricted path is not the default.

## The board is the server

The console, the certificate, the portal PIN and the MCP token live on the board, so the bench works when the cloud does not.
The board issues its own TLS certificate and publishes the fingerprint, so a client can compare rather than trust.
Port 80 stays open by default, and closing it is a setting with a stated cost: a device that answers on no port is not more secure, only unreachable.
Credentials stay on the board - WiFi reaches NVS through `provision`, `api_key` is write-only and reads report `api_key_set` - and none of them enter the repository.

## Every refusal is an answer

A refused call says what was refused and why, in a sentence, because a missing name teaches the caller nothing.
`reboot` is refused with a sentence rather than dropped from the list, and the parts of `sed` and `awk` this board does not implement are refused by name instead of approximated.
Every tool call runs under one configured deadline, and that budget reaches the device rather than staying in a client.
Limits are checked before a buffer is allocated or a bus is touched, never after the copy has already happened.
Every structure that grows - transcript, archive index, workspace, attachments - carries a stated ceiling that the code enforces.
No tool call can take the board down or lock it away from its owner, and nothing on the board restarts or resets it on a caller's behalf.
A driver's `probe` reports `None` or `Err` instead of panicking.
Where the board cannot do a thing, the page that describes the tool says so in the same paragraph.

## The board answers while it works

A long tool call or a model turn in flight does not make `/api/status` wait.
The model call runs outside the application lock, status, health and cancellation have their own plane, and the journal streams while the turn runs.
Queue admission is not storage completion: a reply that says `queued` does not claim `saved`, and a save is acknowledged only after publication.
Concurrency is reported so it can be reasoned about - which core a task runs on, how much stack headroom it holds - not so it can be admired.

## Evidence carries its source

Host tests and board measurements are different claims, and neither stands in for the other.
A change is verified when the command that verifies it is named: the test count, the gate sequence, the partition table the flasher was given.
Behaviour with a build or a measurement behind it goes into `docs/`; everything else goes into `ROADMAP.md` as unverified.
A firmware change that was not built here says so in the commit and the handoff instead of passing as verified.
Historical bench numbers are dated, tied to the image that produced them, and never restated as a current limit.
The repository ships without CI: the gate is five commands a contributor runs, and the README, its seven translations and `ROADMAP.md` state the same test count.

## Scope

It is not a cloud service, a fleet manager or a hosted endpoint: one board, one owner, one LAN.
It is not a general agent framework, and the goal control plane was removed rather than grown.
It is not an OTA channel: remote firmware updates are out of scope, and a flash happens over the cable with the partition table passed explicitly.
It is not a Linux machine with a shell bolted on: the shell and the script interpreter are subsets, and what they do not implement they refuse by name.
It is not every driver: a peripheral earns a driver only when configuration cannot express it, and unverified paths stay in `ROADMAP.md` instead of the feature list.
It is not a place for secrets: no credential, token or WiFi password enters the code, the examples, the tests or the image.
A name nothing calls is deleted - a REST route, an alias, a seeded skill, a changelog - because a surface kept for sentiment is a surface every later change has to defend.

A change aligns when it makes one description serve another caller, moves a device from code to configuration, answers something the board could not answer before, makes a refusal clearer or a limit enforceable, or lets the board keep answering while it works.
A change should be resisted when it adds a second source of truth about the tools, needs a reflash for hardware a manifest could describe, puts the cloud in the control path, spends a budget it never bounded, claims a verification it never ran, or keeps a surface nothing calls.
