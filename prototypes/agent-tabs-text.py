#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty

RESET = '\033[0m'


def paint(text, fg='969696', bold=False, bg='191919'):
    def rgb(value):
        return ';'.join(str(int(value[i:i + 2], 16)) for i in (0, 2, 4))
    return f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}' + (';1' if bold else '') + 'm' + text + RESET


def render(active):
    columns, rows = shutil.get_terminal_size()
    width = min(80, columns - 4)
    labels = ['research', 'test', 'xxx']
    schemes = [
        ('A · 柔绿 / 标准间距', 'b5d2a2', ' / ', '505050', True),
        ('B · 亮白 / 弱分隔符', 'eeeeee', ' / ', '404040', True),
        ('C · 浅蓝 / 紧凑间距', '83b9d6', '/', '606060', True),
        ('D · 灰紫 / 宽间距', 'b9a5d5', '  /  ', '505050', True),
        ('E · 柔绿 / 保留大小写', 'b5d2a2', ' / ', '606060', False),
        ('F · 暖白 / 宽间距', 'e1d8c5', '  /  ', '484848', True),
    ]
    lines = ['\033[H\033[2J' + paint('Agent tabline · 纯文字与斜线', 'b5d2a2', bg='202020'),
             paint('同一背景 · 无下划线 · 激活项仅变色加粗 · test 模拟 hover', '858585', bg='202020')]
    for title, accent, separator, divider, uppercase in schemes:
        lines.extend(['', paint(title, 'cccccc', bg='202020')])
        pieces = []
        used = 0
        for index, label in enumerate(labels):
            selected = index == active
            text = label.upper() if selected and uppercase else label
            pieces.append(paint(text, accent if selected else ('cccccc' if index == 1 else '858585'), selected))
            pieces.append(paint(separator, divider))
            used += len(text) + len(separator)
        pieces.append(paint('+', '969696'))
        used += 1
        pieces.append(paint(' ' * max(1, width - used - 1)))
        pieces.append(paint('›', 'cccccc'))
        lines.append(''.join(pieces))
        if rows >= 36:
            lines.append(paint('│  已检查终端状态与命令退出码。', '858585', bg='202020'))
    lines.extend(['', paint('1 / 2 / 3 切换激活会话 · q / Esc 退出', '858585', bg='202020')])
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
