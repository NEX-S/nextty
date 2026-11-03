#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty

RESET = '\033[0m'


def style(text, fg='cccccc', bg='191919', active=False):
    def rgb(value):
        return ';'.join(str(int(value[i:i + 2], 16)) for i in (0, 2, 4))
    underline = ';4;58;2;181;210;162' if active else ''
    return f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}{underline}m{text}{RESET}'


def render(active, hovered):
    columns, _ = shutil.get_terminal_size()
    width = min(columns, 80)
    labels = ['Research', 'Review', 'Notes']
    line = style('│', '555555')
    used = 1
    for index, label in enumerate(labels):
        selected = index == active
        foreground = 'eeeeee' if selected or index == hovered else '858585'
        background = '353535' if selected else '282828'
        line += style(label, foreground, background, selected)
        line += style('│', '555555')
        used += len(label) + 1
    line += style('+', '969696')
    used += 1
    line += style(' ' * max(0, width - used - 1)) + style('›')
    output = [
        '\033[H\033[2J',
        style('Agent tabline · 竖线分隔 / 零 padding', 'b5d2a2', '202020'),
        '',
        line,
        style('│', '444444', '202020') + style('已检查终端状态与命令退出码。', '969696', '202020'),
        style('│', '444444', '202020'),
        style('├' + '─' * (width - 1), '444444', '202020'),
        style('│', '444444', '202020') + style('输入消息…', '858585', '202020'),
        '',
        style('1 / 2 / 3 切换激活 tab · 鼠标移到标题查看 hover · q / Esc 退出', '858585', '202020'),
    ]
    sys.stdout.write('\r\n'.join(output))
    sys.stdout.flush()


if __name__ == '__main__':
    active, hovered = 0, None
    if not sys.stdin.isatty():
        render(active, hovered)
    else:
        settings = termios.tcgetattr(sys.stdin)
        pending = b''
        try:
            tty.setcbreak(sys.stdin.fileno())
            sys.stdout.write('\033[?1049h\033[?25l\033[?1003h\033[?1006h')
            previous = None
            while True:
                current = (shutil.get_terminal_size(), active, hovered)
                if current != previous:
                    render(active, hovered)
                    previous = current
                if not select.select([sys.stdin], [], [], 0.2)[0]:
                    continue
                pending += os.read(sys.stdin.fileno(), 1024)
                while pending:
                    if pending.startswith(b'\033[<'):
                        end = next((i for i, byte in enumerate(pending) if byte in (ord('M'), ord('m'))), None)
                        if end is None:
                            break
                        button, x, y = map(int, pending[3:end].split(b';'))
                        released = pending[end] == ord('m')
                        pending = pending[end + 1:]
                        hovered = None
                        if y == 4:
                            for index, (start, stop) in enumerate([(2, 10), (11, 17), (18, 23)]):
                                if start <= x < stop:
                                    hovered = index
                                    if button == 0 and not released:
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
