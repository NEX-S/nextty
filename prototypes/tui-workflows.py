#!/usr/bin/env python3
import curses
import locale
import unicodedata

CASES = [
    ('文件与预览', 'Yazi', '文件树 + 居中预览', '文件列表、Git 状态与固定预览联动'),
    ('修改审阅', 'Lazygit', '终端查看 git diff', '文件列表、diff 与验证证据并排'),
    ('系统监控', 'btop', '已有资源页与进程列表', '选中进程、详情与趋势建立层次'),
    ('工作区切换', 'Television', '使用 list / attach', '搜索名称、目录、主机并预览布局'),
    ('命令检索', 'Atuin', '当前 pane 输出搜索', '按目录与退出状态检索跨 tab 命令'),
    ('焦点导航', 'Posting', 'Alt 打开面板、Tab 导航', '按目标标记直接跳转到区域'),
    ('后台任务', 'Lazydocker', 'tab 状态点、命令输出', '任务列表与运行事实、输出联动'),
    ('布局恢复', 'Zellij', '进程存活期间 detach / attach', '恢复布局和目录，命令等待手动执行'),
]
FILES = ['src/agent/panel.rs', 'native/render.c', 'src/native/tests.rs']
COMMANDS = ['cargo test --lib native::tests', 'cargo clippy --all-targets', 'cargo build --release']
WORKSPACES = ['nextty / main', 'api / feat-auth', 'devbox / backend']


def width(text):
    return sum(2 if unicodedata.east_asian_width(c) in 'WF' else 1 for c in text)


def clip(text, maximum):
    result = ''
    used = 0
    for c in text:
        size = 2 if unicodedata.east_asian_width(c) in 'WF' else 1
        if used + size > maximum:
            break
        result += c
        used += size
    return result


def current(case, item):
    if case == 0:
        return ['FILES                  TERMINAL', '', '▾ src                  $ cargo test', '  ▾ agent              running 30 tests', '    panel.rs           test result: ok.', '▸ native', '▸ tests', '', 'l 打开居中预览；Enter 插入路径', '', '已有：树形导航、搜索、异步文本预览']
    if case == 1:
        return ['$ git diff --stat', ' src/agent/panel.rs    | 72 ++++----', ' native/render.c      | 36 +++++++', '', '$ git diff -- ' + FILES[item], '@@ changed implementation @@', '- previous implementation', '+ updated implementation', '', '已有：终端运行 Git；尚无内置 diff 面板']
    if case == 2:
        return ['Overview  CPU  Memory  Storage  Network  Processes', '', 'CPU  24%       MEM  6.2 / 16 GiB', '▁▂▃▂▄▆▄▃▂▃▅▄   ▁▁▂▂▂▃▃▂▃▃▃▃', '', ' PID    CPU    MEM     COMMAND', ' 7312   18.2   426M    nextty', ' 8814    4.6   182M    rustc', ' 9011    1.2    38M    zsh', '', '已有：趋势、进程过滤及详情；j/k 选择控件']
    if case == 3:
        return ['$ nextty list', '', 'nextty   3 tabs   4 panes   1 attached', 'api      2 tabs   2 panes   0 attached', '', '$ nextty list devbox', 'backend  2 tabs   3 panes   0 attached', '', '$ nextty attach api --gui', '', '已有：命名会话、本地/SSH attach；无统一选择器']
    if case == 4:
        return ['TERMINAL / current pane', '', '$ cargo test --lib native::tests', 'running 6 tests', 'test result: ok. 6 passed', '', '⌕ cargo                 1 / 3 matches', '', 'Enter 下一处；Shift+Enter 上一处', '', '已有：当前 pane 输出搜索、命令块跳转和复制']
    if case == 5:
        return ['TERMINAL                 AGENT', '', '$ cargo test             • Fix render ×', '                         You', 'test result: ok.          修复块字符的接缝', '', '                         Agent', '                         已修改并验证。', '', '                         输入消息…', '', '已有：Alt+a/m/f、Tab、鼠标切换区域']
    if case == 6:
        return ['• build    • tests    • logs', '', '$ cargo build --release', '   Compiling nextty v0.1.0', '', '在其他 tab 查看日志…', '', '已有：tab 活动状态、命令退出状态', '尚无跨 tab 任务列表与完成通知']
    return ['nextty attach dev --gui', '', '标签页 / 分屏 / Shell / Agent', '由当前 daemon 持有', '', '关闭窗口 → Keep running → 可重新连接', '', '重启系统 / daemon 退出 → 无法恢复原会话', '', '已有：进程存活期间 detach / attach', '尚无重启后的布局恢复']


def main(screen):
    curses.curs_set(0)
    curses.use_default_colors()
    for i, (fg, bg) in enumerate([(252, 235), (245, 235), (151, 235), (255, 238), (240, 235), (215, 235), (203, 235)], 1):
        curses.init_pair(i, fg, bg)
    screen.bkgd(' ', curses.color_pair(1))
    screen.timeout(150)
    case, item, improved, popup = 0, 0, False, False
    note = ''

    def put(y, x, text, color=1, limit=10000):
        rows, cols = screen.getmaxyx()
        if 0 <= y < rows - 1 and 0 <= x < cols - 1:
            screen.addstr(y, x, clip(text, max(0, min(cols - x - 1, limit))), curses.color_pair(color))

    def box(y, x, h, w, title):
        if h < 3 or w < 4:
            return
        put(y, x, '┌' + '─' * (w - 2) + '┐', 5)
        for row in range(y + 1, y + h - 1):
            put(row, x, '│', 5)
            put(row, x + 1, ' ' * (w - 2))
            put(row, x + w - 1, '│', 5)
        put(y + h - 1, x, '└' + '─' * (w - 2) + '┘', 5)
        put(y, x + 2, ' ' + title + ' ', 3, w - 5)

    def lines(y, x, texts, w, h):
        for offset, text in enumerate(texts[:max(0, h)]):
            color = 3 if text.startswith(('+', '✓')) else 7 if text.startswith(('-', '✗')) else 2 if text.startswith(('│', ' cwd', ' host')) else 1
            put(y + offset, x, text, color, w)

    def terminal(y, x, h, w, texts):
        box(y, x, h, w, 'Terminal · nextty / main')
        lines(y + 2, x + 2, texts, w - 4, h - 3)

    def choose(y, x, h, w, title, entries):
        box(y, x, h, w, title)
        for i, (name, extra) in enumerate(entries):
            row = y + 2 + i * 3
            if row + 1 >= y + h - 1:
                break
            text = ('› ' if item == i else '  ') + name
            if i == item:
                text += ' ' * max(0, w - 4 - width(text))
            put(row, x + 2, text, 4 if item == i else 1, w - 4)
            put(row + 1, x + 4, extra, 2, w - 6)

    while True:
        rows, cols = screen.getmaxyx()
        screen.erase()
        name, source, before, after = CASES[case]
        put(0, 2, 'NEXTTY / 八组界面改版对照', 3)
        nav_width = max(1, (cols - 4) // 4)
        for i, entry in enumerate(CASES):
            put(2 + i // 4, 2 + i % 4 * nav_width, f' {i + 1} {entry[0]} ', 4 if i == case else 2, nav_width - 1)
        put(5, 2, ' A  现在 ', 2 if improved else 4)
        put(5, 15, ' B  改后 ', 4 if improved else 2)
        put(5, 30, '借鉴 ' + source, 3)
        put(6, 2, after if improved else before)
        top, height = 8, rows - 13
        w = cols - 4
        split = max(20, min(38, w * 2 // 5))
        right, rw = split + 3, w - split - 1
        if height < 12 or cols < 74:
            put(top, 2, '请放大窗口至至少 74 列、25 行，以查看完整布局。', 6)
        elif improved:
            entries = [
                [(f, 'M · modified') for f in FILES],
                [(f, '+12 −4 · working tree') for f in FILES],
                [('7312 nextty', 'CPU 18.2%  RSS 426 MiB'), ('8814 rustc', 'CPU 4.6%  RSS 182 MiB'), ('9011 zsh', 'CPU 1.2%  RSS 38 MiB')],
                [(n, 'devbox · SSH' if i == 2 else 'Local · attached' if i == 0 else 'Local · detached') for i, n in enumerate(WORKSPACES)],
                [(c, 'exit 0 · 3.2s · render-fix') for c in COMMANDS],
                [('a · Terminal', 'Shell and command output'), ('s · Agent output', 'Conversation and tools'), ('d · Agent input', 'Write a message')],
                [('build', 'RUN · pane 2 · elapsed 12s'), ('tests', 'OK · pane 3 · exit 0'), ('dev server', 'ERR · pane 4 · exit 1')],
                [(n, 'saved layout · 3 panes') for n in WORKSPACES],
            ][case]
            titles = ['Files · /nextty', 'Changes · 3 files', 'Processes · Local', 'Workspaces · search', 'Commands · all tabs', 'Jump · choose destination', 'Activity · 3 commands', 'Saved workspaces']
            choose(top, 2, height, split, titles[case], entries)
            if case == 0:
                box(top, right, height, rw, FILES[item])
                code = [
                    ['fn draw_agent_tabs(...) {', '    let line_bg = 0x111111;', '    let tab_bg = if active {', '        0x363636', '    } else { 0x1b1b1b };', '    draw_title("• title ×");', '}'],
                    ['/* Shared pixel boundaries */', 'float x0 = roundf(left * scale);', 'float x1 = roundf(right * scale);', '', 'draw_rect(x0, y0,', '          x1 - x0, y1 - y0);'],
                    ['#[test]', 'fn block_graphics_use_cell_geometry() {', '    for scale in [1.0, 1.25, 1.5] {', '        check_cell_positions(scale);', '    }', '}'],
                ][item]
                lines(top + 2, right + 2, [f'{i + 1:>3}  {s}' for i, s in enumerate(code)], rw - 4, height - 3)
            elif case == 1:
                upper = max(8, height - 6)
                box(top, right, upper, rw, 'Diff · ' + FILES[item])
                lines(top + 2, right + 2, ['@@ changed implementation @@', '', '- rasterize_font_glyph();', '+ draw_cell_geometry();', '+ align_shared_pixel_edges();', '', ' context / unchanged text'], rw - 4, upper - 3)
                box(top + upper, right, height - upper, rw, 'Validation evidence')
                lines(top + upper + 1, right + 2, ['✓ native tests · 6 passed', '✓ clippy · completed', 'working tree may include user changes'], rw - 4, height - upper - 2)
            elif case == 2:
                box(top, right, height, rw, entries[item][0] + ' · details')
                cpu = ['18.2%', '4.6%', '1.2%'][item]
                lines(top + 2, right + 2, ['CPU    ' + cpu, '▁▂▃▂▄▆▄▃▂▃▅▄▃▂▁▃▄▂', '', 'RSS    ' + ['426 MiB', '182 MiB', '38 MiB'][item], '████████░░░░░░░░░░░░', '', 'cwd    ~/GitHub/next-terminal', 'host   Local', '', 'Current value → trend → process detail'], rw - 4, height - 3)
            elif case in (3, 7):
                box(top, right, height, rw, WORKSPACES[item])
                layout = ['host  ' + ('devbox / SSH' if item == 2 else 'Local'), 'cwd   ~/GitHub/' + ['next-terminal', 'api', 'backend'][item], '', '┌─────────────┬───────────┐', '│ terminal    │ agent     │', '│             │           │', '├─────────────┤           │', '│ tests       │           │', '└─────────────┴───────────┘', '', 'Enter: ' + ('restore directories and layout' if case == 7 else 'attach this workspace')]
                if case == 7:
                    layout += ['Commands remain stopped until confirmed.']
                lines(top + 2, right + 2, layout, rw - 4, height - 3)
            elif case == 4:
                box(top, right, height, rw, 'Command output')
                lines(top + 2, right + 2, ['$ ' + COMMANDS[item], '', 'cwd   ~/GitHub/next-terminal', 'tab   render-fix  / pane 3', 'exit  0     duration 3.2s', '', 'running 6 tests', 'test native::block_graphics ... ok', 'test result: ok. 6 passed', '', 'Enter jumps to the original output.'], rw - 4, height - 3)
            elif case == 5:
                box(top, right, height, rw, 'Workspace · target overlay')
                middle = right + rw // 2
                put(top + 2, right + 2, '[a] Terminal', 4 if item == 0 else 3)
                put(top + 4, right + 2, '$ cargo test', 1, rw // 2 - 3)
                for y in range(top + 1, top + height - 1):
                    put(y, middle, '│', 5)
                put(top + 2, middle + 2, '[s] Agent', 4 if item == 1 else 3)
                put(top + 4, middle + 2, '已检查并验证。', 1, rw // 2 - 4)
                put(top + height - 5, middle + 2, '[d] Input', 4 if item == 2 else 3)
                put(top + height - 3, middle + 2, '输入消息…', 2, rw // 2 - 4)
            else:
                box(top, right, height, rw, 'Task output · ' + entries[item][0])
                lines(top + 2, right + 2, ['pane ' + str(item + 2) + ' / Local', 'state  ' + ['RUNNING', 'COMPLETED / exit 0', 'COMPLETED / exit 1'][item], '', '$ ' + ['cargo build --release', 'cargo test', 'npm run dev'][item], '', ['Compiling nextty v0.1.0', 'test result: ok. 6 passed', 'Error: address already in use'][item], '', 'Enter jumps to this terminal pane.'], rw - 4, height - 3)
        elif case == 0:
            choose(top, 2, height, split, 'Files · tree', [(f.rsplit('/', 1)[1], f.rsplit('/', 1)[0]) for f in FILES])
            terminal(top, right, height, rw, ['$ cargo test', '', 'running 6 tests', 'test result: ok.', '', '$ _'])
            if popup:
                x, pw = max(2, cols // 6), cols * 2 // 3
                box(top + 1, x, height - 2, pw, FILES[item] + ' · Esc closes')
                lines(top + 3, x + 2, ['fn example() {', '    // file preview', '    draw_cell_geometry();', '}'], pw - 4, height - 5)
        elif case == 2:
            box(top, 2, height, w, 'Monitor · Overview / Processes')
            lines(top + 2, 4, ['Overview  CPU  Memory  Storage  Network  Processes', '', 'CPU 24%  ▁▂▃▂▄▆▄▃▂▃▅▄    MEM 39%  ▁▁▂▂▃▃▂▃', '', ' PID     CPU     RSS      COMMAND', ' 7312    18.2%   426M     nextty', ' 8814     4.6%   182M     rustc', ' 9011     1.2%    38M     zsh', '', '已有：资源趋势、进程过滤、进程详情。'], w - 4, height - 3)
        elif case == 5:
            terminal(top, 2, height, split, ['$ cargo test', '', 'test result: ok.', '', '$ _'])
            box(top, right, height, rw, '• Fix render ×  +')
            lines(top + 2, right + 2, ['You', '修复块字符接缝。', '', 'Agent', '已修改并验证。', '', '────────────────────', '输入消息…', '', 'Alt+a/m/f · Tab · mouse'], rw - 4, height - 3)
        else:
            terminal(top, 2, height, w, current(case, item))
            if case in (1, 3, 4, 6, 7):
                put(top + height - 2, 4, '当前没有内置这类专用面板；此处展示已有 CLI / 搜索流程。', 6, w - 4)
        put(rows - 4, 2, note or ('试试 j/k：选中项变化时，右侧内容跟着变化。' if improved else '按 Tab 看改后。文件页可按 l 打开当前的弹窗预览。'), 3)
        put(rows - 3, 2, '1–8 主题 · A 现在 / B 改后 · Tab 对比 · j/k 选条目 · q 退出', 2)
        put(rows - 2, 2, '界面原型 / 固定样例 / 非实时截图 / 优化功能未接入产品', 2)
        screen.refresh()
        key = screen.getch()
        if key == ord('q'):
            return
        if ord('1') <= key <= ord('8'):
            case, item, popup, note = key - ord('1'), 0, False, ''
        elif key == 9:
            improved, popup, note = not improved, False, ''
        elif key in (ord('a'), ord('A'), ord('b'), ord('B')):
            improved, popup, note = key in (ord('b'), ord('B')), False, ''
        elif key in (ord('j'), curses.KEY_DOWN):
            item, note = (item + 1) % 3, ''
        elif key in (ord('k'), curses.KEY_UP):
            item, note = (item - 1) % 3, ''
        elif key == ord('l') and case == 0 and not improved:
            popup = True
        elif key == 27:
            popup = False
        elif key in (10, 13):
            note = '演示：选中第 %d 项；此原型不执行任何命令。' % (item + 1)


if __name__ == '__main__':
    locale.setlocale(locale.LC_ALL, '')
    curses.wrapper(main)
