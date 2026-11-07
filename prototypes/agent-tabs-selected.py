#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty

BG = '202020'
BAR = '111111'
INACTIVE = '1b1b1b'
ACTIVE = '363636'
LABELS = [('research', 'b5d2a2'), ('tests', 'fe9b30'), ('review', 'ef7373')]


def rgb(color):
    return ';'.join(str(int(color[i:i + 2], 16)) for i in (0, 2, 4))


def paint(text, fg='cccccc', bg=BG, underline=False):
    style = f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}m'
    if underline:
        style += '\033[4;58;2;204;204;204m'
    return style + text + '\033[0m'


def render(active):
    cols, rows = shutil.get_terminal_size()
    width = max(1, min(76, cols - 4))
    lines = ['\033[H\033[2J', paint('  Agent · dot title x', 'dddddd'), '']
    parts, used = [], 0
    for index, (label, dot) in enumerate(LABELS):
        selected = index == active
        bg = ACTIVE if selected else INACTIVE
        fg = 'dddddd' if selected else '606060'
        parts.append(paint(' ', fg, bg, selected))
        parts.append(paint('•', dot, bg, selected))
        parts.append(paint(f' {label} × ', fg, bg, selected))
        used += len(label) + 6
    parts.append(paint(' + ', '858585', BAR))
    used += 3
    parts.append(paint(' ' * max(0, width - used), bg=BAR))
    lines.append('  ' + ''.join(parts))
    lines.extend([
        paint('  ' + ' ' * width),
        paint('    You', '909090'),
        paint('    检查测试结果，并总结需要修改的地方。'),
        '',
        paint('    Agent', 'b5d2a2'),
        paint('    已检查终端输出，正在核对失败的测试。'),
        paint('    下一步会定位问题并验证修改。', '909090'),
        '',
        paint('  ' + '─' * width, '414141'),
        paint('    输入消息…', '707070'),
        '',
        paint('  1 / 2 / 3 切换激活 tab · h / l 前后切换 · q / Esc 退出', '858585'),
        paint('  状态圆点：完成 / 执行中 / 待处理', '858585'),
    ])
    sys.stdout.write('\r\n'.join(lines[:rows - 1]) + '\033[J')
    sys.stdout.flush()


def main():
    settings = termios.tcgetattr(sys.stdin)
    active = 0
    previous = None
    try:
        tty.setcbreak(sys.stdin.fileno())
        sys.stdout.write('\033[?1049h\033[?25l' + f'\033[48;2;{rgb(BG)}m')
        while True:
            state = (shutil.get_terminal_size(), active)
            if state != previous:
                render(active)
                previous = state
            if select.select([sys.stdin], [], [], 0.15)[0]:
                key = os.read(sys.stdin.fileno(), 1)
                if key in (b'q', b'\x1b', b'\x03'):
                    break
                if key in (b'1', b'2', b'3'):
                    active = int(key) - 1
                elif key == b'h':
                    active = (active - 1) % 3
                elif key == b'l':
                    active = (active + 1) % 3
    finally:
        termios.tcsetattr(sys.stdin, termios.TCSADRAIN, settings)
        sys.stdout.write('\033[0m\033[?25h\033[?1049l')
        sys.stdout.flush()


if __name__ == '__main__':
    main()
