#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty


def paint(text, fg='858585', bold=False):
    rgb = ';'.join(str(int(fg[i:i + 2], 16)) for i in (0, 2, 4))
    return f'\033[38;2;{rgb};48;2;25;25;25' + (';1' if bold else '') + 'm' + text + '\033[0m'


def render(active):
    columns, _ = shutil.get_terminal_size()
    width = min(84, columns)
    lines = ['\033[H\033[2J' + paint('Agent tabline · 竖线分隔符', 'cccccc'),
             paint('状态示意：research 完成 / test 执行中 / review 需要处理'), '']
    for title, glyph in [('A · 细实线 U+2502', '│'),
                         ('B · 粗实线 U+2503', '┃'),
                         ('C · 数学竖线 U+2223', '∣'),
                         ('D · 双线 U+2551', '║'),
                         ('E · 虚线 U+250A', '┊'),
                         ('F · 断线 U+00A6', '¦')]:
        lines.append(paint(title, 'cccccc'))
        parts, used = [], 0
        for index, (label, color) in enumerate([('research', 'b5d2a2'), ('test', 'fe9b30'), ('review', 'ef5350')]):
            parts.append(paint('•', color))
            parts.append(paint(' ' + label, 'ffffff' if index == active else '858585', index == active))
            parts.append(paint(' × ', '969696' if index == active else '606060'))
            parts.append(paint(glyph + (' ' if index < 2 else ''), '606060'))
            used += len(label) + 7 - int(index == 2)
        parts.append(paint('+', '969696'))
        parts.append(paint(' ' * max(1, width - used - 3)))
        parts.append(paint('› ', 'cccccc'))
        lines.extend([''.join(parts), ''])
    lines.extend([paint('• 完成', 'b5d2a2') + paint('    • 执行中', 'fe9b30') + paint('    • 失败 / 等待用户输入', 'ef5350'),
                  '', paint('1 / 2 / 3 切换激活 tab · q / Esc 退出')])
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
