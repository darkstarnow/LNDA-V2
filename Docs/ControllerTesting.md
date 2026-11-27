# Controller testing

Diagnostics builds support a software gamepad when no XInput or DualSense pad is connected. This can test input delivery without installing a virtual controller driver. It does not establish rendering correctness or gameplay compatibility.

Build with `cargo build --release --locked --package leonida --features diagnostics --bins`.

Before launching, set `LEONIDA_MOVEMENT_FILE` to an absolute path outside the source tree and `LEONIDA_LOG=info`. Write one command into that file while Leonida runs:

```text
1 Enter 500
```

The fields are an increasing sequence number, one control, and a hold duration in milliseconds (1 through 10000). The file is checked every 200 ms. Use a new sequence number for every action; duplicate and older numbers do not restart a hold. A later valid command replaces the previous hold. Inputs release at the end of the hold. Short holds may expire before the title reads them; start with 500 ms and check the read-back log.

| Control | Command |
| --- | --- |
| A / B | Enter / Back |
| Menu / View | Esc / Tab |
| D-pad | Up / Down / Left / Right |
| X / Y | X / Y |
| Left / right shoulder | Q / E |
| Left / right stick click | F / H |
| Left / right trigger | Z / C |
| Left stick forward / back / left / right | W / S / A / D |
| Right stick up / down / left / right | I / K / J / L |

`A` retains its keyboard meaning of left-stick movement. Use `Enter` for the controller A button. Triggers and sticks use full deflection. One command controls one button, trigger or stick axis. Other keyboard controls remain available, and an existing value on the same control takes precedence. A real controller disables the file-driven stand-in.

`stand-in input started` confirms the command was accepted. `GetGamepadState returned scripted input` confirms the title read a gamepad state containing it; the log includes sequence, button bits, triggers and stick axes. Neither message proves that the title performed an action. Check the visible result separately. Keep runtime logs, saves, caches and screenshots outside the repository.

The existing `LEONIDA_KEYS` timed startup script and `LEONIDA_ENTER_ON_FILE` one-shot startup signal remain available.
