# pinout

description: discover and record what is wired to this board, so the next session knows

A board arrives undocumented: the owner knows what they meant to wire, not what the probe
can see. Turn that into a memory with the detail and a manifest for the parts no probe
can reach.

## Probe

1. `inventory`: the SoC, the board profile, the pins the profile claims, the capabilities
   it believes it has. Everything else narrows this.
2. `devices_list`: what the drivers already adopted, with ids and tool names.
3. `i2c_scan {"bus":0}`, then bus 1: every address that acknowledges.
4. `i2c_identify {"addr":118}` for each address the list does not claim. It answers
   candidate drivers and a register to read; confirm with `i2c_read` before believing it.
5. `adc_read` a pin only when something is known to drive it. A floating pin reads noise,
   and noise recorded as a measurement is worse than no entry.

Ask the owner about what cannot be probed: a relay, a mosfet, a speaker, a divider. Do
not guess a pin and write the guess down as fact.

## Record it

Write the detail to `/memory/pinout.md` with `write`: one line per connection with pin,
what it is, its polarity, and the evidence (`i2c_scan found 0x76`, `gpio_read 12 floated
high`). Then add one pointer line to `/MEMORY.md`, because the index is all that reaches
the prompt:

```text
- [pinout](/memory/pinout.md) - GPIO12 relay (active low), I2C 8/9, OLED 0x3C
```

## Declare what cannot be probed

A part no probe can see exists for the agent only once the manifest says so. Validate,
dry-run, apply, then call the new tool once.

```text
manifest_apply {"dry_run":true,"manifest":{"devices":[{"id":"pump","kind":"relay","driver":"relay","pins":{"out":12},"options":{"active_low":true}}]}}
```

Re-run `devices_list`: the declared device must answer with a tool name. Note in the
memory which parts are declared rather than detected, so a later session does not mistake
the manifest for a measurement.
