#!/usr/bin/env python3
import curses
import locale
import unicodedata

FILES = [
    ('panel.rs', 'src/agent/panel.rs', [
        'fn draw_agent_tabs(...) {',
        '    let line_bg = 0x111111;',
        '    let tab_bg = if active {',
        '        0x363636',
        '    } else {',
        '        0x1b1b1b',
        '    };',
        '    draw_title("• title ×");',
        '}']),
    ('render.c', 'native/render.c', [
        '/* Shared pixel edges */',
        'float x0 = roundf(left * scale);',
        'float x1 = roundf(right * scale);',
        '',
        'draw_rectangle(',
        '    x0, y0,',
        '    x1 - x0, y1 - y0,',
        '    color',
        ');']),
    ('config.rs', 'src/config.rs', [
        'Config {',
        '    font_size_px: 17.0,',
        '    foreground: 0xcccccc,',
        '    background: 0x202020,',
        '    background_opacity: 1.0,',
        '    tab_style: "angled",',
        '}']),
]


def clip(text, cols):
    out, used = '', 0
    for ch in text:
        size = 2 if unicodedata.east_asian_width(ch) in 'WF' else 1
        if used + size > cols:
            break
        out += ch
        used += size
    return out


def main(screen):
    curses.curs_set(0)
    curses.use_default_colors()
    for i, (fg, bg) in enumerate([(252, 235), (245, 235), (151, 235), (255, 238), (240, 235), (215, 235)], 1):
        curses.init_pair(i, fg, bg)
    screen.bkgd(' ', curses.color_pair(1))
    screen.timeout(150)
    improved, selected, preview = False, 0, False
    note = ''

    def put(y, x, text, pair=1, cols=None):
        h, w = screen.getmaxyx()
        if 0 <= y < h - 1 and 0 <= x < w - 1:
            screen.addstr(y, x, clip(text, min(w - x - 1, cols or w)), curses.color_pair(pair))

    def line(y, x, length):
        put(y, x, '─' * max(0, length), 5)

    def divider(x, top, bottom):
        for y in range(top, bottom):
            put(y, x, '│', 5)

    def code(y, x, width, height):
        name, path, content = FILES[selected]
        put(y, x, path, 3, width)
        put(y + 1, x, '只读预览 · 示例片段', 2, width)
        line(y + 2, x, width)
        for i, text in enumerate(content[:max(0, height - 3)]):
            put(y + 3 + i, x, f'{i + 1:>3}  ', 2, width)
            put(y + 3 + i, x + 5, text, 1, max(1, width - 5))

    while True:
        h, w = screen.getmaxyx()
        screen.erase()
        put(0, 2, 'NEXTTY / 文件浏览器改版对照', 3)
        put(2, 2, ' 1  现在 ', 4 if not improved else 2)
        put(2, 14, ' 2  改后 ', 4 if improved else 2)
        put(4, 2, '改后：文件列表旁固定预览，切换文件即更新内容。' if improved else '现在：文件树旁是终端，按 l 后在中央打开预览。', 1)
        put(5, 2, '这是界面原型：使用同一组固定示例文件，不修改真实项目。', 2)
        top, bottom = 8, max(10, h - 5)
        left = min(31, max(17, w // 3))
        put(top - 1, 2, '~/GitHub/next-terminal', 2)
        divider(left, top - 1, bottom)
        put(top + 1, 2, 'FILES', 2)
        for i, (name, path, _) in enumerate(FILES):
            label = ('› ' if selected == i else '  ') + name
            put(top + 3 + i * 2, 2, label.ljust(left - 3), 4 if selected == i else 1, left - 3)
            if improved:
                put(top + 4 + i * 2, 4, path.rsplit('/', 1)[0], 2, left - 5)
        right, body_width = left + 3, max(1, w - left - 6)
        if improved:
            code(top + 1, right, body_width, bottom - top - 1)
        else:
            put(top + 1, right, 'TERMINAL', 2)
            put(top + 3, right, '$ cargo test --lib', 1, body_width)
            put(top + 5, right, 'running 30 tests', 2, body_width)
            put(top + 6, right, 'test result: ok. 30 passed', 3, body_width)
            put(top + 9, right, '$ _', 1, body_width)
            if preview:
                x, y = max(1, w * 15 // 100), max(top - 1, h * 22 // 100)
                pw, ph = max(1, w * 70 // 100), max(5, min(h * 55 // 100, bottom - y))
                for row in range(y, y + ph):
                    put(row, x, ' ' * pw, 4, pw)
                line(y, x, pw)
                code(y + 1, x + 2, max(1, pw - 4), ph - 3)
                line(y + ph - 1, x, pw)
                put(y + ph - 1, x + 2, ' Esc 关闭预览 ', 2, pw - 4)
        line(h - 5, 2, w - 4)
        put(h - 4, 2, note or ('j/k 选文件，右侧预览立即切换。' if improved else '先 j/k 选文件，再按 l 打开预览；Esc 关闭。'), 3)
        put(h - 3, 2, '1 现在 · 2 改后 · Tab 切换 · j/k 选文件 · l 预览 · q 退出', 2)
        screen.refresh()
        key = screen.getch()
        if key == ord('q'):
            return
        if key in (ord('1'), ord('2'), 9):
            improved = key == ord('2') if key != 9 else not improved
            preview, note = False, ''
        elif key in (ord('j'), curses.KEY_DOWN) and (improved or not preview):
            selected, note = (selected + 1) % len(FILES), ''
        elif key in (ord('k'), curses.KEY_UP) and (improved or not preview):
            selected, note = (selected - 1) % len(FILES), ''
        elif key == ord('l'):
            preview, note = not improved, ''
        elif key == 27:
            preview, note = False, ''
        elif key in (10, 13):
            note = '插入路径示意：' + FILES[selected][1] + '（不执行）'


if __name__ == '__main__':
    locale.setlocale(locale.LC_ALL, '')
    curses.wrapper(main)
