#!/usr/bin/env python3
"""Minimal Wayland test client for the pcu vwayland live harness.

Connects to the compositor from the environment (WAYLAND_DISPLAY /
XDG_RUNTIME_DIR, as set by vwayland's `launch`), opens one xdg_toplevel,
and renders a 1280x720 ARGB frame showing:

- every wl_keyboard key event received, as a 3x5-digit cell of the evdev
  keycode (white = no shift, yellow = shift held at press time);
- a crosshair at the last wl_pointer position (red flash on button press).

Writes "READY" to the path given as argv[1] after the first configured
frame is committed, so the harness knows input will land.

When argv[2] names an event log path, every key press, pointer motion,
pointer enter, and button press is appended there as a text line:

    key <evdev> <shift 0|1>
    motion <x> <y>
    enter <x> <y>
    button <evdev>

so the harness can assert exactly what the client received. (The first
pointer_move into the surface arrives as wl_pointer.enter, not motion.)
"""

import mmap
import os
import sys
import tempfile

from pywayland.client import Display
from pywayland.protocol.wayland import WlCompositor, WlShm, WlSeat
from pywayland.protocol.xdg_shell import XdgWmBase

WIDTH, HEIGHT = 1280, 720

# 3x5 bitmap digits
FONT = {
    "0": ["###", "# #", "# #", "# #", "###"],
    "1": [" # ", "## ", " # ", " # ", "###"],
    "2": ["###", "  #", "###", "#  ", "###"],
    "3": ["###", "  #", "###", "  #", "###"],
    "4": ["# #", "# #", "###", "  #", "  #"],
    "5": ["###", "#  ", "###", "  #", "###"],
    "6": ["###", "#  ", "###", "# #", "###"],
    "7": ["###", "  #", "  #", " # ", " # "],
    "8": ["###", "# #", "###", "# #", "###"],
    "9": ["###", "# #", "###", "  #", "###"],
    "-": ["   ", "   ", "###", "   ", "   "],
}

BG = (30, 30, 34)
CELL_BG = (16, 16, 20)
FG_PLAIN = (235, 235, 235)
FG_SHIFT = (240, 200, 40)
CROSS = (80, 200, 120)
FLASH = (230, 60, 60)

state = {
    "keys": [],          # list of (evdev_code, shift_held)
    "pointer": (WIDTH // 2, HEIGHT // 2),
    "flash": 0,
    "ready_path": sys.argv[1] if len(sys.argv) > 1 else "/tmp/pcu-client-ready",
    "event_log": sys.argv[2] if len(sys.argv) > 2 else None,
    "signaled": False,
}


def log_event(line):
    if state["event_log"]:
        with open(state["event_log"], "a") as f:
            f.write(line + "\n")


def draw_digit(buf, x0, y0, ch, color, scale=4):
    glyph = FONT[ch]
    for r, row in enumerate(glyph):
        for c, px in enumerate(row):
            if px == "#":
                for dy in range(scale):
                    for dx in range(scale):
                        x, y = x0 + c * scale + dx, y0 + r * scale + dy
                        if 0 <= x < WIDTH and 0 <= y < HEIGHT:
                            o = (y * WIDTH + x) * 4
                            buf[o:o + 4] = bytes((color[2], color[1], color[0], 255))


def render(buf):
    buf[:] = bytes((BG[2], BG[1], BG[0], 255)) * (WIDTH * HEIGHT)
    # key log grid: up to 40 cells per row
    for i, (code, shift) in enumerate(state["keys"][-120:]):
        col, row = i % 40, i // 40
        x0, y0 = 20 + col * 30, 20 + row * 32
        fg = FG_SHIFT if shift else FG_PLAIN
        s = str(code)
        for yy in range(y0 - 4, y0 + 24):
            for xx in range(x0 - 4, x0 + 4 + len(s) * 16):
                if 0 <= xx < WIDTH and 0 <= yy < HEIGHT:
                    o = (yy * WIDTH + xx) * 4
                    buf[o:o + 4] = bytes((CELL_BG[2], CELL_BG[1], CELL_BG[0], 255))
        for j, ch in enumerate(s):
            draw_digit(buf, x0 + j * 16, y0, ch, fg)
    # crosshair
    px, py = state["pointer"]
    color = FLASH if state["flash"] > 0 else CROSS
    for dx in range(-14, 15):
        for x, y in ((px + dx, py), (px, py + dx)):
            if 0 <= x < WIDTH and 0 <= y < HEIGHT:
                o = (y * WIDTH + x) * 4
                buf[o:o + 4] = bytes((color[2], color[1], color[0], 255))
    if state["flash"] > 0:
        state["flash"] -= 1


def main():
    display = Display()
    display.connect()
    registry = display.get_registry()
    compositor = shm = xdg_wm_base = seat = None

    def on_global(r, id_, interface, version):
        nonlocal compositor, shm, xdg_wm_base, seat
        if interface == "wl_compositor":
            compositor = r.bind(id_, WlCompositor, version)
        elif interface == "wl_shm":
            shm = r.bind(id_, WlShm, version)
        elif interface == "xdg_wm_base":
            xdg_wm_base = r.bind(id_, XdgWmBase, version)
        elif interface == "wl_seat":
            seat = r.bind(id_, WlSeat, version)

    registry.dispatcher["global"] = on_global
    display.roundtrip()
    assert compositor and shm and xdg_wm_base and seat, "missing globals"

    xdg_wm_base.dispatcher["ping"] = lambda base, serial: base.pong(serial)

    surface = compositor.create_surface()
    xdg_surface = xdg_wm_base.get_xdg_surface(surface)
    toplevel = xdg_surface.get_toplevel()
    toplevel.set_title("pcu-test")
    toplevel.set_app_id("pcu-test-client")

    stride = WIDTH * 4
    size = stride * HEIGHT
    fd, _path = tempfile.mkstemp()
    os.ftruncate(fd, size)
    buf = mmap.mmap(fd, size, access=mmap.ACCESS_WRITE)
    pool = shm.create_pool(fd, size)
    buffer = pool.create_buffer(0, WIDTH, HEIGHT, stride, WlShm.format.argb8888.value)

    def on_configure(surf, serial):
        surf.ack_configure(serial)
        render(buf)
        surface.attach(buffer, 0, 0)
        surface.damage(0, 0, WIDTH, HEIGHT)
        surface.commit()
        if not state["signaled"]:
            state["signaled"] = True
            with open(state["ready_path"], "w") as f:
                f.write("READY\n")

    xdg_surface.dispatcher["configure"] = on_configure
    surface.commit()
    display.roundtrip()

    keyboard = seat.get_keyboard()
    mods = {"depressed": 0}

    def on_key(kb, serial, time_, key, key_state):
        # py-vwayland 0.1.1 passes evdev codes through unchanged (no +8
        # XKB offset), so the received value is the evdev code directly.
        if key_state == 1:  # pressed
            shift = bool(mods["depressed"] & 1)
            state["keys"].append((key, shift))
            log_event(f"key {key} {1 if shift else 0}")
            render(buf)
            surface.attach(buffer, 0, 0)
            surface.damage(0, 0, WIDTH, HEIGHT)
            surface.commit()

    def on_modifiers(kb, serial, dep, lat, lock, group):
        mods["depressed"] = dep

    keyboard.dispatcher["key"] = on_key
    keyboard.dispatcher["modifiers"] = on_modifiers

    pointer = seat.get_pointer()

    def on_enter(p, serial, surface_, sx, sy):
        # The first pointer_move into the surface arrives as enter, not
        # motion (Smithay). Treat it as a position update the same way.
        state["pointer"] = (int(sx), int(sy))
        log_event(f"enter {int(sx)} {int(sy)}")
        render(buf)
        surface.attach(buffer, 0, 0)
        surface.damage(0, 0, WIDTH, HEIGHT)
        surface.commit()

    def on_motion(p, time_, sx, sy):
        state["pointer"] = (int(sx), int(sy))
        log_event(f"motion {int(sx)} {int(sy)}")
        render(buf)
        surface.attach(buffer, 0, 0)
        surface.damage(0, 0, WIDTH, HEIGHT)
        surface.commit()

    def on_button(p, serial, time_, button, button_state):
        if button_state == 1:
            log_event(f"button {button}")
            state["flash"] = 6
            render(buf)
            surface.attach(buffer, 0, 0)
            surface.damage(0, 0, WIDTH, HEIGHT)
            surface.commit()

    pointer.dispatcher["enter"] = on_enter
    pointer.dispatcher["motion"] = on_motion
    pointer.dispatcher["button"] = on_button

    while True:
        display.dispatch(block=True)


if __name__ == "__main__":
    main()
