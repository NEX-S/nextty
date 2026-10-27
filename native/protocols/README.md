# Fixed Wayland protocol definitions

The XML definitions and generated client stubs are committed so normal builds do not require wayland-scanner. Each XML and generated file retains its upstream copyright/license. `SHA256SUMS` pins the XML snapshots. Each XML includes its permissive license text.

- xdg-shell, viewporter, fractional-scale, primary-selection, text-input and presentation-time: wayland-protocols definitions distributed by the installed Qt 6 protocol collection (`qt6-qtbase-gui-6.11.2-3.fc44.x86_64`); upstream https://gitlab.freedesktop.org/wayland/wayland-protocols (copied 2026-10-23).
- cursor-shape and tablet-v2: official wayland-protocols 1.45 release, https://gitlab.freedesktop.org/wayland/wayland-protocols/-/tree/1.45 (copied 2026-10-23). Tablet-v2 is generated interface metadata required by cursor-shape's tablet request signature; the application binds only the pointer cursor-shape interface.

Regenerate a protocol with `wayland-scanner client-header NAME.xml NAME-client.h` and `wayland-scanner private-code NAME.xml NAME-protocol.c`. Ordinary builds compile only the committed generated C sources.
