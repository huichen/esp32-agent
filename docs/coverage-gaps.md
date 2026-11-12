# Coverage gaps

`cargo llvm-cov -p esp32-agent-core` reports the lines the suite never executed. This file
lists the ones that remain in every file this pass touched, and why each is still there. A
line listed here is a claim that the tests have no opinion about it, which is why the list is
written down instead of being left implicit in a percentage.

Four kinds appear:

- **unreachable** — the line cannot run, whatever the input: the branch is dead by arithmetic,
  or a guard above it already enforces the same condition.
- **not reached** — reachable in principle, but reaching it needs a fixture or a path the
  tests added here do not build. The reason says what that would take. These are the honest
  holes; none of them is covered by a test that only looks like it covers them.
- **artifact** — a closing brace, or the panic arm of a test helper, that llvm-cov attributes
  to a region only one path enters. There is no statement of the product code on the line.
- **not in this pass** — files that were below the cut when the budget ran out.

Platform-specific code is not listed: `firmware/` is excluded from the workspace, so
llvm-cov never sees it.

## drivers/actuator.rs — 24 lines

| Lines | Kind | Why |
|---|---|---|
| 59–61, 120–122, 130–132, 206–208, 223–225, 301–303, 316–318 | not reached | The seven `bind` closures inside `tools()`. They are three lines each and do nothing but forward to `Devices::call`. `App::call_tool` reaches a device tool through the device registry, not through this closure — measured, not assumed: the call succeeds while these lines stay at zero. Covering them needs the model-facing tool host, which no test drives for these devices. |
| 387–389 | not reached | The fake `Hardware::board_info`, which nothing in this module calls. |

## drivers/expander.rs — 18 lines

| Lines | Kind | Why |
|---|---|---|
| 158–206 | not reached | The five `bind` closures, as above. The expander also cannot be instantiated on the virtual board at all (nothing at 0x20 echoes 0xFF, which is exactly what `probe` requires), so no app-level test can reach them either. |
| 273–275 | not reached | The fake `board_info`. |

## drivers/ranging.rs — 11 lines

| Lines | Kind | Why |
|---|---|---|
| 37–40 | unreachable | `probe`'s `Ok(_)` arm (a reading that is not finite, or not positive). `measure` returns `Ok` only after `width >= 100`, so the smallest value it can produce is 1.72 cm. The arm is defensive. |
| 111–113 | not reached | The `distance_cm` bind closure. Besides needing the tool host, instantiating the sensor costs ~14 s of wall clock on the virtual board (12,000 iterations of `gpio_read` + `delay_us` at Windows sleep granularity), which is why the app-level test that declared one was dropped rather than kept at that price. |
| 130 | unreachable | `if v.is_empty()` after the sample loop: the first failed ping returns the error, so `v` is never empty when the loop ends. |
| 190–192 | not reached | The fake `board_info`. |

## drivers/ina219.rs — 9 lines

| Lines | Kind | Why |
|---|---|---|
| 132–134 | not reached | The `power_read` bind closure: needs the tool host, and the virtual board has nothing at 0x40/0x41. |
| 183–185, 275–277 | not reached | The two fake `board_info` implementations. |

## drivers/ads1115.rs — 7 lines

| Lines | Kind | Why |
|---|---|---|
| 82 | unreachable | `probe`'s "reading outside the range" refusal. At the gain the probe always uses (4.096 V, 125 µV per LSB) the largest magnitude a 16-bit conversion can produce is 4.095875 V, so `v.abs() > 4.096` is never true and the check cannot reject anything. It is left as it is because removing it would be a behaviour change with no evidence behind it; the arithmetic above is the evidence that it is dead. |
| 157–159 | not reached | The `ads1115_read` bind closure: needs the tool host, and nothing sits at 0x48..0x4B on the virtual board. |
| 223–225 | not reached | The fake `board_info`. |

## drivers/onewire.rs — 7 lines

| Lines | Kind | Why |
|---|---|---|
| 170–172 | not reached | The `ds18b20_read` bind closure. A virtual DS18B20 would have to answer on a bit-banged bus, which the virtual board does not model. |
| 245–247 | not reached | The fake `board_info`. |
| 265 | artifact | A closing brace inside the fake. |

## drivers/ssd1306.rs — 19 lines

| Lines | Kind | Why |
|---|---|---|
| 195 | unreachable | `set_pixel`'s `if idx >= self.fb.len()` guard: the coordinate check immediately above already holds `x < width` and `y < height`, so the index is always inside the buffer. |
| 347–354 | not reached | The `oled_text` bind closure. It re-packs the arguments (`line` and `x` default to 0, `clear` to true) before forwarding, so it is not a pure pass-through; the packing it does is the same defaulting `Device::call` does, which is covered directly. |
| 400–406 | not reached | The `oled_bitmap` bind closure, for the same reason. |
| 524–526 | not reached | The fake `board_info`. |

## zlib.rs — 3 lines

| Lines | Kind | Why |
|---|---|---|
| 188 | unreachable | `dynamic_tables`' "bad code length symbol" arm. The code-length alphabet has 19 symbols and `Huffman::decode` returns a symbol from that table, so the `0..=18` arms cover every value it can return. |
| 212 | not reached | "bad distance symbol". Reachable only from a hand-built distance alphabet of more than 30 entries (`hdist` can declare up to 32); no test here builds one. |
| 228 | not reached | The 1 MiB output cap. Reaching it needs a stream that decodes to more than a megabyte (16 maximum-length stored blocks); the cap is a heap guard and no test spends that memory. |

The CRC is absent from this table because raw deflate carries no checksum. The check lives in
`zip::read`, and `zip.rs` is now fully covered, stored and deflated cases both.

## manifest.rs — 11 lines

| Lines | Kind | Why |
|---|---|---|
| 461, 470, 482, 503, 512, 525, 631, 783, 836 | artifact | Closing braces of `if let` blocks whose body returns, or that only one path enters. Every statement inside those blocks is covered; the brace belongs to a region that did not run. |
| 920 | unreachable | `limited`'s "a valid range" fallback: `limited` is called only when `min` or `max` is `Some`, so `(None, None)` cannot arrive. |
| 942 | unreachable | The same fallback in `safety_refused`, for the same reason. |

## tools/shell.rs — 2 lines

| Lines | Kind | Why |
|---|---|---|
| 353, 424 | artifact | The `panic!` arms of the two test helpers. They exist to fail the test if a fixture stops being refused; a passing test never evaluates them. |

## Files not in this pass

The work went in the order of the task list, worst file first. These three were still below
the line when the budget ran out, and have no new tests from this pass:

| File | Line coverage when this pass stopped |
|---|---|
| `tools/net.rs` | 76.45% |
| `tools/mod.rs` | 77.36% |
| `virtual_board.rs` | 83.38% |

All three are reachable from a host test, and they are the next three in that order. Nothing
in this document should be read as "there is nothing left to test"; it says only that the
files above have no tests for the lines in their tables.
