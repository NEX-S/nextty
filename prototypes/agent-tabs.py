#!/usr/bin/env python3
import os
import select
import shutil
import sys
import termios
import tty
import unicodedata

RESET = '\033[0m'
BG = '#202020'
LINE = '#191919'
FG = '#cccccc'
MUTED = '#858585'
GREEN = '#b5d2a2'


def width(text):
    return sum(2 if unicodedata.east_asian_width(c) in 'WF' else 1 for c in text)


def paint(text, fg=FG, bg=BG, underline=False):
    def rgb(color):
        return ';'.join(str(int(color[i:i + 2], 16)) for i in (1, 3, 5))
    style = f'\033[38;2;{rgb(fg)};48;2;{rgb(bg)}m'
    if underline:
        style += f'\033[4;58;2;{rgb(GREEN)}m'
    return style + text + RESET


def render():
    columns, rows = shutil.get_terminal_size()
    size = min(78, columns - 4)
    output = ['\033[H\033[2J', paint('  Agent tabline · 四套终端方案', GREEN),
              paint('  第一个激活 / 第二个模拟 hover / 第三个非激活', MUTED)]
    detailed = rows >= 34
    for number, title, note in [
        (1, '细下划线', '最接近 terminal tabline，减少背景色块'),
        (2, '竖线分组', '用分隔线建立秩序，激活会话更醒目'),
        (3, '描边标签', '边框强调当前会话，背景保持统一'),
        (4, '柔和色块', '激活块更明亮，其余轻量，层次最明显'),
    ]:
        output.append('')
        output.append(paint(f'  {number}  {title}', GREEN) + paint(f'  {note}', MUTED))
        pieces = []
        count = 0

        def add(text, fg=FG, bg=LINE, underline=False):
            nonlocal count
            pieces.append(paint(text, fg, bg, underline))
            count += width(text)

        if number == 1:
            add(' Agent  ', MUTED)
            add(' Research × ', FG, '#303030', True)
            add(' Review × ', '#f0f0f0', '#222222')
            add(' Notes ', MUTED, '#222222')
        elif number == 2:
            add(' ▎', GREEN)
            add(' Research × ', FG, '#30342d', True)
            add(' │ ', '#555555')
            add('Review ×', '#f0f0f0')
            add(' │ ', '#555555')
            add('Notes ', MUTED)
        elif number == 3:
            add(' ', MUTED)
            add('╭ Research × ╮', GREEN, '#202020')
            add('  Review ×  ', '#f0f0f0', '#202020')
            add(' Notes ', MUTED, '#202020')
        else:
            add(' ', MUTED)
            add(' Research × ', '#eeeeee', '#3b4137', True)
            add(' ', MUTED)
            add(' Review × ', '#eeeeee', '#292929')
            add(' ', MUTED)
            add(' Notes ', MUTED, '#292929')
        add('  + ', MUTED)
        add(' ' * max(1, size - count - 3))
        add(' › ', FG)
        output.append('  ' + ''.join(pieces))
        if detailed:
            output.append('  ' + paint('│  已检查终端状态与命令退出码。'.ljust(size - 15), MUTED))
            output.append('  ' + paint('├' + '─' * (size - 1), '#444444'))
            output.append('  ' + paint('│  输入消息…', MUTED))
    output.extend(['', paint('  q / Esc 退出预览 · 告诉我方案编号，或组合喜欢的部分', MUTED)])
    sys.stdout.write('\r\n'.join(output))
    sys.stdout.flush()


if __name__ == '__main__':
    if not sys.stdin.isatty():
        render()
    else:
        settings = termios.tcgetattr(sys.stdin)
        try:
            tty.setcbreak(sys.stdin.fileno())
            sys.stdout.write('\033[?1049h\033[?25l')
            previous = None
            while True:
                current = shutil.get_terminal_size()
                if current != previous:
                    render()
                    previous = current
                if select.select([sys.stdin], [], [], 0.2)[0]:
                    if os.read(sys.stdin.fileno(), 1) in (b'q', b'\x1b', b'\x03'):
                        break
        finally:
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, settings)
            sys.stdout.write('\033[0m\033[?25h\033[?1049l')
            sys.stdout.flush()
