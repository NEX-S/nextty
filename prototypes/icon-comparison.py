#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty
import unicodedata


def width(text):
    return sum(2 if unicodedata.east_asian_width(c) in 'WF' else 1 for c in text)


def paint(text, fg='cccccc', bg='202020'):
    def rgb(value):
        return ';'.join(str(int(value[i:i + 2], 16)) for i in (0, 2, 4))
    return f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}m{text}\033[0m'


def panel(optimized, columns):
    down, right, up = ('▾', '▸', '▴') if optimized else ('', '', '')
    cpu, mem, disk, swap = ('CPU', 'MEM', 'DSK', 'SWP') if optimized else ('', '', '', '')
    branch = 'git:main' if optimized else '󱐋 main'
    title = '优化实现 · 普通 Unicode + 文字' if optimized else '目前实现 · Nerd Font 图标'
    rows = [
        (title, 'b5d2a2', '202020'),
        ('', 'cccccc', '202020'),
        ('FILES', '858585', '202020'),
        (f'{down} next-terminal', 'dddddd', '202020'),
        (f'  {down} src', 'cccccc', '202020'),
        ('      agent/', 'dddddd', '363636'),
        ('      main.rs', 'cccccc', '202020'),
        ('      ui.rs', 'cccccc', '202020'),
        (f'  {right} native', 'cccccc', '202020'),
        (f'  {right} docs', 'cccccc', '202020'),
        ('', 'cccccc', '202020'),
        (f'MONITOR                         {down}', '858585', '202020'),
        (f'{cpu} 12%   {mem} 38%   {disk} 61%   {swap} 0%', 'cccccc', '191919'),
        ('', 'cccccc', '202020'),
        ('CPU      ▁▂▁▃▂▁▄▂▁▂▃▂▁▂    12%', 'b5d2a2', '202020'),
        ('Memory   ━━━━━━━━━░░░░░░░    38%', 'cccccc', '202020'),
        ('', 'cccccc', '202020'),
        (f'Monitor collapsed               {up}', '858585', '202020'),
        ('', 'cccccc', '202020'),
        (f'~/GitHub/next-terminal {branch}', '858585', '191919'),
        ('', 'cccccc', '202020'),
        ('• research ×    • tests ×    +', 'cccccc', '111111'),
        ('Agent tab 图标保持原样', '858585', '202020'),
    ]
    result = []
    for text, fg, bg in rows:
        clipped = ''
        for ch in text:
            if width(clipped + ch) > columns - 2:
                break
            clipped += ch
        result.append(paint(' ' + clipped + ' ' * (columns - width(clipped) - 1), fg, bg))
    return result


def render(mode):
    cols, rows = shutil.get_terminal_size()
    lines = [paint('  NEXTTY / ICON COMPARISON', 'dddddd'),
             paint('  当前图标与建议替代 · 相同内容、数值和配色', '858585'), '']
    if mode == 0 and cols >= 106:
        size = (cols - 7) // 2
        left, right = panel(False, size), panel(True, size)
        lines.extend('  ' + a + paint(' │ ', '414141') + b for a, b in zip(left, right))
    else:
        selected = mode == 2
        lines.extend('  ' + line for line in panel(selected, min(70, cols - 4)))
    lines.extend(['', paint('  0 对照 · 1 当前实现 · 2 优化实现 · q / Esc 退出', '858585')])
    if cols < 106 and mode == 0:
        lines.append(paint('  窗口较窄：按 1 / 2 切换，放宽窗口可并排对照', '858585'))
    sys.stdout.write('\033[H\033[2J' + '\r\n'.join(lines[:rows - 1]) + '\033[J')
    sys.stdout.flush()


def main():
    saved = termios.tcgetattr(sys.stdin)
    mode, previous = 0, None
    try:
        tty.setcbreak(sys.stdin.fileno())
        sys.stdout.write('\033[?1049h\033[?25l')
        while True:
            current = (shutil.get_terminal_size(), mode)
            if current != previous:
                render(mode)
                previous = current
            if select.select([sys.stdin], [], [], 0.15)[0]:
                key = os.read(sys.stdin.fileno(), 1)
                if key in (b'q', b'\x1b', b'\x03'):
                    break
                if key in (b'0', b'1', b'2'):
                    mode = int(key)
    finally:
        termios.tcsetattr(sys.stdin, termios.TCSADRAIN, saved)
        sys.stdout.write('\033[0m\033[?25h\033[?1049l')
        sys.stdout.flush()


if __name__ == '__main__':
    main()
