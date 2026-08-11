# Hardware support (backends)

## Canon PIXMA

- Different product lines:
    - PIXMA series
    - i-SENSYS
    - MAXIFY
- Lookup for Canon solutions for network attached scanners with send-to-computer function
- Plan implementation of Canon backend(s)

## Epson

Different product lines:
- EcoTank
- Expression (Home)
- SureColor
- WorkForce (Pro)

## Xerox

- VersaLine product Line

# Graphical frontends

See `docs/design/` for design idea, based on adwaita Gnome lib.

## Windows

Do-able ?
D-Bus for Windows ?

## Gnome

Planned: `docs/scanbus-gnome-gui.md`, workstream 10. A GTK4/libadwaita
client in a `scanbus-gui` crate, taking `scanbus-client` so it breaks at compile time with the
CLI and the daemon's conformance tests. The window covers discovery and configuration
(`docs/design`); job notifications come from a windowless background mode of the same binary,
using the Desktop Notification Spec —
https://specifications.freedesktop.org/notification-spec/ — because a button press happens
with nobody at the computer.