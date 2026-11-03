#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty
import unicodedata

RESET = '\033[0m'
BG = '202020'
LINE = '191919'
GREEN = 'b5d2a2'
FG = 'dddddd'
MUTED = '858585'


def cell_width(text):
    return sum(2 if unicodedata.east_asian_width(c) in 'WF' else 1 for c in text)


def paint(text, fg=FG, bg=LINE, underline=False, bold=False):
    def rgb(color):
        return ';'.join(str(int(color[i:i + 2], 16)) for i in (0, 2, 4))
    extra = ';4;58;2;181;210;162' if underline else ''
    if bold:
        extra += ';1'
    return f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}{extra}m{text}{RESET}'


def render(active):
    columns, rows = shutil.get_terminal_size()
    width = min(86, columns - 4)
    compact = rows < 38
    labels = ['Research', 'Review', 'Notes']
    descriptions = [
        ('A · 纯文字', '只用字重和颜色区分，没有背景块'),
        ('B · 底部短线', '每个标题下只留一条独立短线'),
        ('C · 终端书签', '圆角轮廓包住激活项，其他项退后'),
        ('D · 编号会话', '序号作为视觉锚点，适合多会话'),
        ('E · 反色标签', '浅绿实底，激活状态一眼可见'),
        ('F · 文件夹页签', '连续底线与激活页签相接'),
    ]
    output = ['\033[H\033[2J' + paint('Agent tabline · 六套新方案', GREEN, BG),
              paint('按 1 / 2 / 3 切换会话；第二个非激活标题模拟 hover', MUTED, BG)]
    for number, (title, description) in enumerate(descriptions):
        output.append('')
        output.append(paint(title, GREEN, BG) + paint('  ' + description, MUTED, BG))
        segments = []
        used = 0
        positions = []

        def add(text, fg=FG, bg=LINE, underline=False, bold=False):
            nonlocal used
            segments.append(paint(text, fg, bg, underline, bold))
            used += cell_width(text)

        add(' ')
        if number == 5:
            top = ' '
            for i, label in enumerate(labels):
                top += paint('╭' + '─' * (len(label) + 2) + '╮', GREEN if i == active else LINE)
                top += paint(' ')
            output.append(top)
        for index, label in enumerate(labels):
            selected = index == active
            foreground = FG if selected or index == 1 else MUTED
            start = used
            if number == 0:
                add(label, GREEN if selected else foreground, bold=selected)
                add('   ')
            elif number == 1:
                add(label, foreground, underline=selected, bold=selected)
                add('   ')
            elif number == 2:
                if selected:
                    add('❨', GREEN)
                    add(label, FG, '293027')
                    add('❩', GREEN)
                else:
                    add(' ' + label + ' ', foreground)
                add(' ')
            elif number == 3:
                add(str(index + 1), GREEN if selected else '555555', bold=selected)
                add(' ' + label, foreground, underline=selected)
                add('   ')
            elif number == 4:
                add(' ' + label + ' ', '202820' if selected else foreground,
                    GREEN if selected else LINE, bold=selected)
                add(' ')
            else:
                add('│' if selected else ' ', GREEN if selected else LINE)
                add(' ' + label + ' ', FG if selected else foreground, '293027' if selected else LINE)
                add('│' if selected else ' ', GREEN if selected else LINE)
                add(' ')
            positions.append((start, used))
        add('+', MUTED)
        add(' ' * max(1, width - used - 1))
        add('›', FG)
        output.append(''.join(segments))
        if number == 5:
            bottom = ''
            for column in range(width):
                selected_range = positions[active]
                # The selected folder opens into the output area below it.
                char = ' ' if selected_range[0] < column < selected_range[1] - 2 else '─'
                bottom += char
            output.append(paint(bottom, '485143'))
        elif not compact:
            output.append(paint('─' * width, '303030', BG))
        if not compact:
            output.append(paint('已检查命令状态，等待下一步输入。', MUTED, BG))
    output.extend(['', paint('q / Esc 退出 · 选 A–F，或指定想组合的部分', MUTED, BG)])
    sys.stdout.write('\r\n'.join(output) + '\033[J')
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
