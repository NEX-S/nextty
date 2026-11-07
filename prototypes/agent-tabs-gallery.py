#!/usr/bin/env python3
import curses
import locale

TITLES = [
    ('纯文字', '仅字重和前景色'), ('细下划线', '激活项下划线'),
    ('顶部横线', '顶部标记激活项'), ('反色实底', '高对比激活块'),
    ('方括号', '命令行风格边界'), ('圆括号', '柔和文字轮廓'),
    ('圆角描边', '独立圆角标签'), ('文件夹页签', '底线连接当前内容'),
    ('斜切标签', '斜线分割色块'), ('箭头页签', '连续方向感'),
    ('状态圆点', '状态色与激活态独立'), ('状态竖条', '每项左侧状态锚点'),
    ('编号会话', '数字定位会话'), ('紧凑计数', '标题后显示消息数'),
    ('面包屑', '路径式会话导航'), ('细线分隔', '单行轻量分组'),
    ('粗线分隔', '清晰的组间边界'), ('虚线分隔', '降低分隔线重量'),
    ('双线分隔', '经典终端质感'), ('左侧光标', '当前项使用指针'),
    ('居中标签', '居中排列，左右留白'), ('双层标题', '标题和状态分两行'),
    ('分段色块', '连续等宽分段'), ('矩形描边', '完整边框强调激活项'),
]
LABELS = ['Research', 'Tests', 'Review']
STATUS = ['完成', '执行中', '待处理']


def main(screen):
    curses.curs_set(0)
    curses.use_default_colors()
    screen.timeout(150)
    # Pair: muted, foreground, accent, active, hover, green, amber, red, rail.
    for index, (fg, bg) in enumerate([
        (245, 234), (252, 234), (151, 234), (234, 151), (255, 238),
        (151, 234), (215, 234), (203, 234), (240, 234),
    ], 1):
        curses.init_pair(index, fg, bg)
    screen.bkgd(' ', curses.color_pair(1))
    selected, active, hover = 0, 0, 1

    def draw(y, x, text, pair=2, bold=False, underline=False):
        rows, cols = screen.getmaxyx()
        if y >= rows - 1 or x >= cols - 1:
            return
        attr = curses.color_pair(pair)
        if bold:
            attr |= curses.A_BOLD
        if underline:
            attr |= curses.A_UNDERLINE
        screen.addnstr(y, x, text, max(0, cols - x - 1), attr)

    while True:
        rows, cols = screen.getmaxyx()
        per_page = max(1, (rows - 6) // 5)
        page = selected // per_page
        start = page * per_page
        screen.erase()
        draw(0, 2, 'AGENT / TABLINE GALLERY', 3, True)
        draw(1, 2, '24 套方案 · 相同会话 / 状态 / hover · 当前方案 %02d' % (selected + 1))
        for number in range(start, min(24, start + per_page)):
            y = 3 + (number - start) * 5
            title, note = TITLES[number]
            draw(y, 2, ('› ' if number == selected else '  ') + f'{number + 1:02d}  {title}', 3 if number == selected else 2, True)
            draw(y, 28, note, 1)
            x = 4
            if number == 20:
                x = max(4, (min(cols, 90) - 39) // 2)
            for i, label in enumerate(LABELS):
                is_active = i == active
                pair = 3 if is_active else 5 if i == hover else 1
                text = f' {label} × '
                above, below = '', ''
                if number == 0:
                    text = f' {label} '
                elif number == 2 and is_active:
                    above = '─' * len(text)
                elif number == 3 and is_active:
                    pair = 4
                elif number == 4:
                    text = f'[{label} ×]' if is_active else f' {label}  '
                elif number == 5:
                    text = f'({label} ×)' if is_active else f' {label}  '
                elif number == 6 and is_active:
                    above, below = '╭' + '─' * (len(text) - 2) + '╮', '╰' + '─' * (len(text) - 2) + '╯'
                    text = '│' + text[1:-1] + '│'
                elif number == 7:
                    above = ('┌' + '─' * (len(text) - 2) + '┐') if is_active else ''
                    below = '┘' + ' ' * (len(text) - 2) + '└' if is_active else '─' * len(text)
                elif number == 8:
                    text = f' / {label} × /'
                    pair = 4 if is_active else pair
                elif number == 9:
                    text = f'❯ {label} × '
                    pair = 4 if is_active else pair
                elif number in (10, 11):
                    draw(y + 2, x, '●' if number == 10 else '▎', 6 + i)
                    x += 2
                elif number == 12:
                    text = f' {i + 1:02d} {label} × '
                elif number == 13:
                    text = f' {label} { [12, 8, 3][i] } × '
                elif number == 14:
                    text = f' {label} ' + ('›' if i < 2 else '')
                elif number in (15, 16, 17, 18):
                    text = f' {label} × '
                elif number == 19:
                    text = ('▸ ' if is_active else '  ') + label + ' × '
                elif number == 21:
                    below = ' ' + STATUS[i]
                elif number == 22:
                    text = f' {label:^10} × '
                    pair = 4 if is_active else 5 if i == hover else 1
                elif number == 23 and is_active:
                    above, below = '┌' + '─' * (len(text) - 2) + '┐', '└' + '─' * (len(text) - 2) + '┘'
                    text = '│' + text[1:-1] + '│'
                draw(y + 1, x, above, 3)
                draw(y + 2, x, text, pair, is_active, number == 1 and is_active)
                draw(y + 3, x, below, 6 + i if number == 21 else 3)
                x += len(text)
                if number in (15, 16, 17, 18):
                    draw(y + 2, x, ['│', '┃', '┊', '║'][number - 15], 9)
                    x += 1
                elif number != 22:
                    x += 1
            draw(y + 2, x + 1, '+', 2)
        draw(rows - 3, 2, 'j/k 选方案 · n/p 翻页 · 1/2/3 切激活会话 · h 切换 hover', 2)
        draw(rows - 2, 2, 'q 退出  |  页 %d/%d  |  告诉我喜欢的方案编号' % (page + 1, (23 // per_page) + 1), 1)
        screen.refresh()
        key = screen.getch()
        if key in (ord('q'), 27):
            return
        if key in (ord('j'), curses.KEY_DOWN):
            selected = (selected + 1) % 24
        elif key in (ord('k'), curses.KEY_UP):
            selected = (selected - 1) % 24
        elif key in (ord('n'), curses.KEY_NPAGE, ord(' ')):
            selected = min(23, selected + per_page)
        elif key in (ord('p'), curses.KEY_PPAGE):
            selected = max(0, selected - per_page)
        elif key in (ord('1'), ord('2'), ord('3')):
            active = key - ord('1')
        elif key == ord('h'):
            hover = (hover + 1) % 3


if __name__ == '__main__':
    locale.setlocale(locale.LC_ALL, '')
    curses.wrapper(main)
