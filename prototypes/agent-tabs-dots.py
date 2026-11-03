#!/usr/bin/env python3
import os
import re
import select
import shutil
import sys
import termios
import tty

RESET = '\033[0m'
LABELS = ['research', 'test', '...']


def paint(text, fg='858585', bold=False):
    rgb = ';'.join(str(int(fg[i:i + 2], 16)) for i in (0, 2, 4))
    return f'\033[38;2;{rgb};48;2;25;25;25' + (';1' if bold else '') + 'm' + text + RESET


def render(active, pointer):
    columns, _ = shutil.get_terminal_size()
    width = min(86, columns)
    pieces, hits = [], []
    x = 1
    for index, label in enumerate(LABELS):
        selected = index == active
        stop = x + len(label) + 5
        hovered = pointer is not None and pointer[1] == 4 and x <= pointer[0] < stop
        pieces.append(paint('•', 'ffffff' if selected else '969696'))
        pieces.append(paint(' ' + label + ' ', 'ffffff' if selected else ('cccccc' if hovered else '858585'), selected))
        close_x = x + len(label) + 3
        close_hovered = pointer is not None and pointer == (close_x, 4)
        pieces.append(paint('×', 'ef5350' if close_hovered else ('969696' if selected else '606060')))
        pieces.append(paint(' / ', '505050'))
        hits.append((x, stop, index))
        x += len(label) + 7
    pieces.append(paint('+', '969696'))
    pieces.append(paint(' ' * max(1, width - x - 1)))
    pieces.append(paint('›', 'cccccc'))
    lines = [
        '\033[H\033[2J' + paint('Agent tabline · dot / 标题 / 关闭 / 斜线', 'cccccc'),
        paint('激活标题：亮白加粗 · 统一背景 · 无下划线'),
        '',
        ''.join(pieces),
        paint('│'),
        paint('│  已检查终端状态与命令退出码。', '969696'),
        paint('│'),
        paint('├' + '─' * (width - 1), '444444'),
        paint('│  输入消息…'),
        '',
        paint('1 / 2 / 3 或点击标题切换 · hover × 变红 · q / Esc 退出'),
    ]
    sys.stdout.write('\r\n'.join(lines) + '\033[J')
    sys.stdout.flush()
    return hits


if __name__ == '__main__':
    active, pointer = 0, None
    if not sys.stdin.isatty():
        render(active, pointer)
    else:
        settings = termios.tcgetattr(sys.stdin)
        pending = b''
        try:
            tty.setcbreak(sys.stdin.fileno())
            sys.stdout.write('\033[?1049h\033[?25l\033[?1003h\033[?1006h')
            previous = None
            while True:
                current = (shutil.get_terminal_size(), active, pointer)
                if current != previous:
                    hits = render(active, pointer)
                    previous = current
                if not select.select([sys.stdin], [], [], 0.2)[0]:
                    continue
                pending += os.read(sys.stdin.fileno(), 1024)
                while pending:
                    if pending.startswith(b'\033[<'):
                        match = re.match(rb'\x1b\[<(\d+);(\d+);(\d+)([Mm])', pending)
                        if match is None:
                            break
                        button, x, y = map(int, match.group(1, 2, 3))
                        pending = pending[match.end():]
                        pointer = (x, y)
                        if button == 0 and match[4] == b'M' and y == 4:
                            for start, stop, index in hits:
                                if start <= x < stop:
                                    active = index
                        continue
                    if pending in (b'\033', b'\033['):
                        if select.select([sys.stdin], [], [], 0.04)[0]:
                            pending += os.read(sys.stdin.fileno(), 1024)
                            continue
                    key, pending = pending[:1], pending[1:]
                    if key in (b'q', b'\033', b'\x03'):
                        raise SystemExit
                    if key in (b'1', b'2', b'3'):
                        active = int(key) - 1
        finally:
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, settings)
            sys.stdout.write('\033[?1003l\033[?1006l\033[0m\033[?25h\033[?1049l')
            sys.stdout.flush()
