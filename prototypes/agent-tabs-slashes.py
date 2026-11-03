#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty

RESET = '\033[0m'


def paint(text, fg='cccccc', bg='191919', active=False):
    def rgb(value):
        return ';'.join(str(int(value[i:i + 2], 16)) for i in (0, 2, 4))
    extra = ';4;58;2;181;210;162' if active else ''
    return f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}{extra}m{text}{RESET}'


def render(active):
    columns, rows = shutil.get_terminal_size()
    width = min(82, columns - 4)
    labels = ['Research', 'Review', 'Notes']
    schemes = [
        ('A · 共享斜线', '/ Research / Review / Notes /'),
        ('B · 紧凑斜线', '/Research/Review/Notes/'),
        ('C · 独立斜线', '/ Research /  / Review /  / Notes /'),
        ('D · 成对斜边', '/ Research \\  / Review \\  / Notes \\'),
        ('E · 反向斜边', '\\ Research /  \\ Review /  \\ Notes /'),
        ('F · 双斜线', '// Research // Review // Notes //'),
    ]
    lines = ['\033[H\033[2J' + paint('Agent tabline · 斜线分隔方案', 'b5d2a2', '202020'),
             paint('1 / 2 / 3 切换激活会话；Review 非激活时模拟 hover', '858585', '202020')]
    for number, (title, _) in enumerate(schemes):
        lines.extend(['', paint(title, 'b5d2a2', '202020')])
        pieces = []
        used = 0

        def add(text, fg='cccccc', bg='191919', selected=False):
            nonlocal used
            pieces.append(paint(text, fg, bg, selected))
            used += len(text)

        add(' ')
        shared = number in (0, 1, 5)
        delimiter = '//' if number == 5 else '/'
        if shared:
            add(delimiter, '60685b')
        for index, label in enumerate(labels):
            selected = index == active
            foreground = 'eeeeee' if selected or index == 1 else '858585'
            background = '30352c' if selected else '191919'
            separator = 'b5d2a2' if selected else '606060'
            if shared:
                text = label if number == 1 else ' ' + label + ' '
                add(text, foreground, background, selected)
                add(delimiter, '60685b')
            else:
                left, right = ('/', '/') if number == 2 else (('/', '\\') if number == 3 else ('\\', '/'))
                add(left, separator, background)
                add(' ' + label + ' ', foreground, background, selected)
                add(right, separator, background)
                add('  ')
        add('  +', '858585')
        add(' ' * max(1, width - used - 1))
        add('›')
        lines.append(''.join(pieces))
        if rows >= 36:
            lines.append(paint('│  已检查命令状态，等待下一步输入。', '858585', '202020'))
    lines.extend(['', paint('q / Esc 退出 · 选 A–F，也可以组合分隔符、间距和背景', '858585', '202020')])
    sys.stdout.write('\r\n'.join(lines) + '\033[J')
    sys.stdout.flush()


if __name__ == '__main__':
    active = 0
    if not sys.stdin.isatty():
        render(active)
    else:
        settings = termios.tcgetattr(sys.stdin)
        try:
            tty.setcbreak(sys.stdin.fileno())
            sys.stdout.write('\033[?1049h\033[?25l')
            previous = None
            while True:
                current = (shutil.get_terminal_size(), active)
                if current != previous:
                    render(active)
                    previous = current
                if select.select([sys.stdin], [], [], 0.2)[0]:
                    key = os.read(sys.stdin.fileno(), 1)
                    if key in (b'q', b'\033', b'\x03'):
                        break
                    if key in (b'1', b'2', b'3'):
                        active = int(key) - 1
        finally:
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, settings)
            sys.stdout.write('\033[0m\033[?25h\033[?1049l')
            sys.stdout.flush()
