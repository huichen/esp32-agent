# recover-board

description: diagnose a board that looks broken before touching its configuration

A board that "stopped working" is usually a device that is not there, a device declared
but not live, or a manifest that no longer describes the wiring. Read before you write.

## Look

- `board_status`: profile, uptime, free heap, WiFi, and whether a restart is pending. A
  short uptime means it is rebooting in a loop.
- `logs_tail {"n":40}`: boot lines, driver errors, auth failures. The first error is the
  cause; the ones after it are consequences.
- `devices_probe`, then `devices_list`: re-scan the buses and see what was adopted.

## Tell the two failures apart

- **No device.** The scan is empty or the address never acknowledges: wiring, power or a
  pin change, not a manifest problem. Report what the scan actually saw.
- **Declared but not live.** The manifest names it, `devices_list` does not, and the log
  records an instantiation failure. The declaration and the hardware disagree.

## Fix the manifest, not the hardware

1. `manifest_validate {"manifest":{...}}`: field paths for anything wrong.
2. `manifest_apply {"dry_run":true,"manifest":{...}}`: the verdict, nothing changed.
3. `manifest_apply {"mode":"merge","manifest":{...}}`: apply the corrected part only;
   `replace` discards the rest.

A tool that exists and fails belongs to its driver: read the error, do not delete it.

## Reboot last

`reboot` restarts the WiFi stack, the agent and the devices. It is not a fix: it erases
the log you were reading. Use it only after a manifest change that needs the registry
rebuilt.

`factory_reset {"confirm":true}` erases the configuration and restarts with defaults; the
workspace files survive. Say plainly what was reset: the whole configuration, or the one
tool you removed.

Write what you found to `/memory/<topic>.md` with `write`, and one pointer line in
`/MEMORY.md`, or the next session debugs the same board from zero.
